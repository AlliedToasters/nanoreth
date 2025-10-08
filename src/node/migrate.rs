use alloy_consensus::Header;
use alloy_primitives::{b256, hex::ToHexExt, BlockHash, B256, U256};
use reth::{
    api::{NodeTypes, NodeTypesWithDBAdapter},
    args::{DatabaseArgs, DatadirArgs},
    dirs::{ChainPath, DataDirPath},
};
use reth_chainspec::EthChainSpec;
use reth_db::{
    mdbx::{tx::Tx, RO},
    models::CompactU256,
    static_file::iter_static_files,
    table::Decompress,
    DatabaseEnv,
};
use reth_errors::ProviderResult;
use reth_provider::{
    providers::{NodeTypesForProvider, StaticFileProvider},
    static_file::SegmentRangeInclusive,
    DatabaseProvider, ProviderFactory, ReceiptProvider, StaticFileProviderFactory,
    StaticFileSegment, StaticFileWriter,
};
use std::{marker::PhantomData, path::PathBuf, sync::Arc};
use tracing::{info, warn};

use crate::{chainspec::HlChainSpec, HlHeader, HlPrimitives};

pub(super) struct Migrator<N: NodeTypesForProvider> {
    data_dir: ChainPath<DataDirPath>,
    provider_factory: ProviderFactory<NodeTypesWithDBAdapter<N, Arc<DatabaseEnv>>>,
    _nt: PhantomData<N>,
}

impl<N: NodeTypesForProvider> Migrator<N>
where
    N: NodeTypes<ChainSpec = HlChainSpec, Primitives = HlPrimitives>,
{
    const MIGRATION_PATH_SUFFIX: &'static str = "migration-tmp";

    pub fn new(
        chain_spec: HlChainSpec,
        datadir: DatadirArgs,
        database_args: DatabaseArgs,
    ) -> eyre::Result<Self> {
        let data_dir = datadir.clone().resolve_datadir(chain_spec.chain());
        let provider_factory = Self::provider_factory(chain_spec, datadir, database_args)?;
        Ok(Self { data_dir, provider_factory, _nt: PhantomData })
    }

    pub fn sf_provider(&self) -> StaticFileProvider<HlPrimitives> {
        self.provider_factory.static_file_provider()
    }

    pub fn migrate_db(&self) -> eyre::Result<()> {
        let is_empty = Self::highest_block_number(&self.sf_provider()).is_none();

        if is_empty {
            return Ok(());
        }

        self.migrate_db_inner()
    }

    fn highest_block_number(sf_provider: &StaticFileProvider<HlPrimitives>) -> Option<u64> {
        sf_provider.get_highest_static_file_block(StaticFileSegment::Headers)
    }

    fn migrate_db_inner(&self) -> eyre::Result<()> {
        self.migrate_static_files()?;
        self.migrate_mdbx()?;
        info!("Database migrated successfully");
        Ok(())
    }

    fn conversion_tmp_dir(&self) -> PathBuf {
        self.data_dir.data_dir().join(Self::MIGRATION_PATH_SUFFIX)
    }

    fn iterate_files_for_segment(
        &self,
        block_range: SegmentRangeInclusive,
        dir: &PathBuf,
    ) -> eyre::Result<Vec<(PathBuf, String)>> {
        let prefix = StaticFileSegment::Headers.filename(&block_range);

        let entries = std::fs::read_dir(dir)?
            .map(|res| res.map(|e| e.path()))
            .collect::<Result<Vec<_>, _>>()?;

        Ok(entries
            .into_iter()
            .filter_map(|path| {
                let file_name = path.file_name().and_then(|f| f.to_str())?;
                if file_name.starts_with(&prefix) {
                    Some((path.clone(), file_name.to_string()))
                } else {
                    None
                }
            })
            .collect())
    }

    fn create_placeholder(&self, block_range: SegmentRangeInclusive) -> eyre::Result<()> {
        // The direction is opposite here
        let src = self.data_dir.static_files();
        let dst = self.conversion_tmp_dir();

        for (src_path, file_name) in self.iterate_files_for_segment(block_range, &src)? {
            let dst_path = dst.join(file_name);
            if dst_path.exists() {
                std::fs::remove_file(&dst_path)?;
            }
            std::os::unix::fs::symlink(src_path, dst_path)?;
        }

        Ok(())
    }

    fn move_static_files_for_segment(
        &self,
        block_range: SegmentRangeInclusive,
    ) -> eyre::Result<()> {
        let src = self.conversion_tmp_dir();
        let dst = self.data_dir.static_files();

        for (src_path, file_name) in self.iterate_files_for_segment(block_range, &src)? {
            let dst_path = dst.join(file_name);
            std::fs::remove_file(&dst_path)?;
            std::fs::rename(&src_path, &dst_path)?;
        }

        // Still StaticFileProvider needs the file to exist, so we create a symlink
        self.create_placeholder(block_range)
    }

    fn migrate_static_files(&self) -> eyre::Result<()> {
        let conversion_tmp = self.conversion_tmp_dir();
        let old_path = self.data_dir.static_files();

        if conversion_tmp.exists() {
            std::fs::remove_dir_all(&conversion_tmp)?;
        }
        std::fs::create_dir_all(&conversion_tmp)?;

        let mut all_static_files = iter_static_files(&old_path)?;
        let all_static_files =
            all_static_files.remove(&StaticFileSegment::Headers).unwrap_or_default();
        let provider = self.provider_factory.provider()?;

        let mut first = true;

        for (block_range, _tx_ranges) in all_static_files {
            let migration_needed = self.using_old_header(block_range.start())? ||
                self.using_old_header(block_range.end())?;
            if !migration_needed {
                // Create a placeholder symlink
                self.create_placeholder(block_range)?;
                continue;
            }

            if first {
                info!("Old database detected, migrating database...");
                first = false;
            }

            let sf_provider = self.sf_provider();
            let sf_tmp_provider = StaticFileProvider::<HlPrimitives>::read_write(&conversion_tmp)?;
            let block_range_for_filename = sf_provider.find_fixed_range(block_range.start());
            migrate_single_static_file(&sf_tmp_provider, &sf_provider, &provider, block_range)?;

            self.move_static_files_for_segment(block_range_for_filename)?;
        }

        Ok(())
    }

    fn provider_factory(
        chain_spec: HlChainSpec,
        datadir: DatadirArgs,
        database_args: DatabaseArgs,
    ) -> eyre::Result<ProviderFactory<NodeTypesWithDBAdapter<N, Arc<DatabaseEnv>>>> {
        let data_dir = datadir.clone().resolve_datadir(chain_spec.chain());
        let db_env = reth_db::init_db(data_dir.db(), database_args.database_args())?;
        let static_file_provider = StaticFileProvider::read_only(data_dir.static_files(), false)?;
        let db = Arc::new(db_env);
        Ok(ProviderFactory::new(db, Arc::new(chain_spec), static_file_provider))
    }

    fn migrate_mdbx(&self) -> eyre::Result<()> {
        // Actually not much here, all of blocks should be in the static files
        Ok(())
    }

    fn using_old_header(&self, number: u64) -> eyre::Result<bool> {
        let sf_provider = self.sf_provider();
        let content = old_headers_range(&sf_provider, number..=number)?;

        let &[row] = &content.as_slice() else {
            warn!("No header found for block {}", number);
            return Ok(false);
        };
        let header = &row[0];

        let deserialized_old = is_old_header(header);
        let deserialized_new = is_new_header(header);

        assert!(
            deserialized_old ^ deserialized_new,
            "Header is not valid: {} {}\ndeserialized_old: {}\ndeserialized_new: {}",
            number,
            header.encode_hex(),
            deserialized_old,
            deserialized_new
        );
        Ok(deserialized_old && !deserialized_new)
    }
}

// Problem is that decompress just panics when the header is not valid
// So we need heuristics...
fn is_old_header(header: &[u8]) -> bool {
    const SHA3_UNCLE_OFFSET: usize = 0x24;
    const SHA3_UNCLE_HASH: B256 =
        b256!("1dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347");
    const GENESIS_PREFIX: [u8; 4] = [0x01, 0x20, 0x00, 0xf8];
    let Some(sha3_uncle_hash) = header.get(SHA3_UNCLE_OFFSET..SHA3_UNCLE_OFFSET + 32) else {
        return false;
    };
    if sha3_uncle_hash == SHA3_UNCLE_HASH {
        return true;
    }

    // genesis block might be different
    if header.starts_with(&GENESIS_PREFIX) {
        return true;
    }

    false
}

fn is_new_header(header: &[u8]) -> bool {
    rmp_serde::from_slice::<HlHeader>(header).is_ok()
}

fn migrate_single_static_file<N: NodeTypesForProvider<Primitives = HlPrimitives>>(
    sf_out: &StaticFileProvider<HlPrimitives>,
    sf_in: &StaticFileProvider<HlPrimitives>,
    provider: &DatabaseProvider<Tx<RO>, NodeTypesWithDBAdapter<N, Arc<DatabaseEnv>>>,
    block_range: SegmentRangeInclusive,
) -> Result<(), eyre::Error> {
    info!("Migrating block range {}...", block_range);

    // block_ranges into chunks of 100000 blocks
    const CHUNK_SIZE: u64 = 100000;
    for chunk in (0..=block_range.end()).step_by(CHUNK_SIZE as usize) {
        let end = std::cmp::min(chunk + CHUNK_SIZE - 1, block_range.end());
        let block_range = chunk..=end;
        let headers = old_headers_range(sf_in, block_range.clone())?;
        let receipts = provider.receipts_by_block_range(block_range.clone())?;
        assert_eq!(headers.len(), receipts.len());
        let mut writer = sf_out.get_writer(*block_range.start(), StaticFileSegment::Headers)?;
        let new_headers = std::iter::zip(headers, receipts)
            .map(|(header, receipts)| {
                let system_tx_count =
                    receipts.iter().filter(|r| r.cumulative_gas_used == 0).count();
                let eth_header = Header::decompress(&header[0]).unwrap();
                let hl_header =
                    HlHeader::from_ethereum_header(eth_header, &receipts, system_tx_count as u64);

                let difficulty: U256 = CompactU256::decompress(&header[1]).unwrap().into();
                let hash = BlockHash::decompress(&header[2]).unwrap();
                (hl_header, difficulty, hash)
            })
            .collect::<Vec<_>>();
        for header in new_headers {
            writer.append_header(&header.0, header.1, &header.2)?;
        }
        writer.commit().unwrap();
        info!("Migrated block range {:?}...", block_range);
    }
    Ok(())
}

fn old_headers_range(
    provider: &StaticFileProvider<HlPrimitives>,
    block_range: impl std::ops::RangeBounds<u64>,
) -> ProviderResult<Vec<Vec<Vec<u8>>>> {
    Ok(provider
        .fetch_range_with_predicate(
            StaticFileSegment::Headers,
            to_range(block_range),
            |cursor, number| {
                cursor.get(number.into(), 0b111).map(|rows| {
                    rows.map(|columns| columns.into_iter().map(|column| column.to_vec()).collect())
                })
            },
            |_| true,
        )?
        .into_iter()
        .collect())
}

// Copied from reth
fn to_range<R: std::ops::RangeBounds<u64>>(bounds: R) -> std::ops::Range<u64> {
    let start = match bounds.start_bound() {
        std::ops::Bound::Included(&v) => v,
        std::ops::Bound::Excluded(&v) => v + 1,
        std::ops::Bound::Unbounded => 0,
    };

    let end = match bounds.end_bound() {
        std::ops::Bound::Included(&v) => v + 1,
        std::ops::Bound::Excluded(&v) => v,
        std::ops::Bound::Unbounded => u64::MAX,
    };

    start..end
}
