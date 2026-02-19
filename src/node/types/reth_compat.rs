//! Copy of reth codebase to preserve serialization compatibility
use crate::node::storage::tables::{SPOT_METADATA_KEY, SpotMetadata};
use alloy_consensus::{Header, Signed, TxEip1559, TxEip2930, TxEip4844, TxEip7702, TxLegacy};
use alloy_primitives::{Address, BlockHash, Bytes, Signature, TxKind, U256, U64, normalize_v};
use reth_db::{DatabaseEnv, DatabaseError, cursor::DbCursorRW};
use reth_db_api::{Database, transaction::DbTxMut};
use reth_primitives::TransactionSigned as RethTxSigned;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    sync::{Arc, LazyLock, Mutex, RwLock},
};
use tracing::info;

use crate::{
    HlBlock, HlBlockBody, HlHeader,
    node::{
        primitives::TransactionSigned as TxSigned,
        spot_meta::{SpotId, erc20_contract_to_spot_token},
        types::{LegacyReceipt, ReadPrecompileCalls, SystemTx},
    },
};

/// A raw transaction.
///
/// Transaction types were introduced in [EIP-2718](https://eips.ethereum.org/EIPS/eip-2718).
#[derive(Debug, Clone, PartialEq, Eq, Hash, derive_more::From, Serialize, Deserialize)]
pub enum Transaction {
    Legacy(TxLegacy),
    Eip2930(TxEip2930),
    Eip1559(TxEip1559),
    Eip4844(TxEip4844),
    Eip7702(TxEip7702),
}

/// Custom deserializer for [`Signature`] that accepts legacy `v` values (27, 28, EIP-155 ≥35)
/// in addition to the standard 0/1 parity. S3 testnet blocks encode the full `v` value in
/// the msgpack tuple rather than the normalized boolean parity that alloy-primitives expects.
fn deserialize_compat_signature<'de, D>(deserializer: D) -> Result<Signature, D::Error>
where
    D: serde::Deserializer<'de>,
{
    if deserializer.is_human_readable() {
        // Human-readable path (JSON) — delegate to default which already handles v normalization
        Signature::deserialize(deserializer)
    } else {
        // Non-human-readable path (msgpack) — deserialize raw tuple and normalize v
        let (r, s, v_raw) = <(U256, U256, U64)>::deserialize(deserializer)?;
        let v = v_raw.to::<u64>();
        let y_parity = normalize_v(v)
            .ok_or_else(|| serde::de::Error::custom(format!("invalid v value: {v}")))?;
        Ok(Signature::new(r, s, y_parity))
    }
}

/// Signed Ethereum transaction.
#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, derive_more::AsRef, derive_more::Deref,
)]
#[serde(rename_all = "camelCase")]
pub struct TransactionSigned {
    /// The transaction signature values
    #[serde(deserialize_with = "deserialize_compat_signature")]
    signature: Signature,
    /// Raw transaction info
    #[deref]
    #[as_ref]
    transaction: Transaction,
}
impl TransactionSigned {
    /// Convert from the node's TransactionSigned back to reth_compat format.
    pub fn from_node_tx(tx: TxSigned) -> Self {
        use alloy_consensus::EthereumTxEnvelope;
        let inner = tx.into_inner();
        match inner {
            EthereumTxEnvelope::Legacy(signed) => {
                let (tx, sig, _) = signed.into_parts();
                Self { signature: sig, transaction: Transaction::Legacy(tx) }
            }
            EthereumTxEnvelope::Eip2930(signed) => {
                let (tx, sig, _) = signed.into_parts();
                Self { signature: sig, transaction: Transaction::Eip2930(tx) }
            }
            EthereumTxEnvelope::Eip1559(signed) => {
                let (tx, sig, _) = signed.into_parts();
                Self { signature: sig, transaction: Transaction::Eip1559(tx) }
            }
            EthereumTxEnvelope::Eip4844(signed) => {
                let (tx, sig, _) = signed.into_parts();
                Self { signature: sig, transaction: Transaction::Eip4844(tx) }
            }
            EthereumTxEnvelope::Eip7702(signed) => {
                let (tx, sig, _) = signed.into_parts();
                Self { signature: sig, transaction: Transaction::Eip7702(tx) }
            }
        }
    }

    /// Extract just the transaction (without signature) from a node TransactionSigned.
    /// Used for system transactions where the signature is fabricated.
    pub fn extract_transaction(tx: TxSigned) -> Transaction {
        use alloy_consensus::EthereumTxEnvelope;
        let inner = tx.into_inner();
        match inner {
            EthereumTxEnvelope::Legacy(signed) => Transaction::Legacy(signed.into_parts().0),
            EthereumTxEnvelope::Eip2930(signed) => Transaction::Eip2930(signed.into_parts().0),
            EthereumTxEnvelope::Eip1559(signed) => Transaction::Eip1559(signed.into_parts().0),
            EthereumTxEnvelope::Eip4844(signed) => Transaction::Eip4844(signed.into_parts().0),
            EthereumTxEnvelope::Eip7702(signed) => Transaction::Eip7702(signed.into_parts().0),
        }
    }

    fn to_reth_transaction(&self) -> TxSigned {
        match self.transaction.clone() {
            Transaction::Legacy(tx) => {
                TxSigned::Default(RethTxSigned::Legacy(Signed::new_unhashed(tx, self.signature)))
            }
            Transaction::Eip2930(tx) => {
                TxSigned::Default(RethTxSigned::Eip2930(Signed::new_unhashed(tx, self.signature)))
            }
            Transaction::Eip1559(tx) => {
                TxSigned::Default(RethTxSigned::Eip1559(Signed::new_unhashed(tx, self.signature)))
            }
            Transaction::Eip4844(tx) => {
                TxSigned::Default(RethTxSigned::Eip4844(Signed::new_unhashed(tx, self.signature)))
            }
            Transaction::Eip7702(tx) => {
                TxSigned::Default(RethTxSigned::Eip7702(Signed::new_unhashed(tx, self.signature)))
            }
        }
    }
}

type BlockBody = alloy_consensus::BlockBody<TransactionSigned, Header>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SealedHeader {
    pub hash: BlockHash,
    pub header: Header,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SealedBlock {
    /// Sealed Header.
    pub header: SealedHeader,
    /// the block's body.
    pub body: BlockBody,
}

static SPOT_EVM_MAP: LazyLock<Arc<RwLock<BTreeMap<Address, SpotId>>>> =
    LazyLock::new(|| Arc::new(RwLock::new(BTreeMap::new())));

// Optional database handle for persisting on-demand fetches
static DB_HANDLE: LazyLock<Mutex<Option<Arc<DatabaseEnv>>>> = LazyLock::new(|| Mutex::new(None));

/// Set the database handle for persisting spot metadata
pub fn set_spot_metadata_db(db: Arc<DatabaseEnv>) {
    *DB_HANDLE.lock().unwrap() = Some(db);
}

/// Initialize the spot metadata cache with data loaded from database.
/// This should be called during node initialization.
pub fn initialize_spot_metadata_cache(metadata: BTreeMap<Address, SpotId>) {
    *SPOT_EVM_MAP.write().unwrap() = metadata;
}

/// Helper function to serialize and store spot metadata to database
pub fn store_spot_metadata(
    db: &Arc<DatabaseEnv>,
    metadata: &BTreeMap<Address, SpotId>,
) -> Result<(), DatabaseError> {
    db.update(|tx| {
        let mut cursor = tx.cursor_write::<SpotMetadata>()?;

        // Serialize to BTreeMap<Address, u64>
        let serializable_map: BTreeMap<Address, u64> =
            metadata.iter().map(|(addr, spot)| (*addr, spot.index)).collect();

        cursor.upsert(
            SPOT_METADATA_KEY,
            &Bytes::from(
                rmp_serde::to_vec(&serializable_map).expect("Failed to serialize spot metadata"),
            ),
        )?;
        Ok(())
    })?
}

/// Persist spot metadata to database if handle is available
fn persist_spot_metadata_to_db(metadata: &BTreeMap<Address, SpotId>) {
    if let Some(db) = DB_HANDLE.lock().unwrap().as_ref() {
        match store_spot_metadata(db, metadata) {
            Ok(_) => info!("Persisted spot metadata to database"),
            Err(e) => info!("Failed to persist spot metadata to database: {}", e),
        }
    }
}

fn system_tx_to_reth_transaction(transaction: &SystemTx, chain_id: u64) -> TxSigned {
    let Transaction::Legacy(tx) = &transaction.tx else {
        panic!("Unexpected transaction type");
    };
    let TxKind::Call(to) = tx.to else {
        panic!("Unexpected contract creation");
    };
    let s = if tx.input.is_empty() {
        U256::from(0x1)
    } else {
        loop {
            if let Some(spot) = SPOT_EVM_MAP.read().unwrap().get(&to) {
                break spot.to_s();
            }

            // Cache miss - fetch from API, update cache, and persist to database
            info!("Contract not found: {to:?} from spot mapping, fetching from API...");
            let metadata = erc20_contract_to_spot_token(chain_id).unwrap();
            *SPOT_EVM_MAP.write().unwrap() = metadata.clone();
            persist_spot_metadata_to_db(&metadata);
        }
    };
    let signature = Signature::new(U256::from(0x1), s, true);
    TxSigned::Default(RethTxSigned::Legacy(Signed::new_unhashed(tx.clone(), signature)))
}

impl SealedBlock {
    pub fn to_reth_block(
        &self,
        read_precompile_calls: ReadPrecompileCalls,
        highest_precompile_address: Option<Address>,
        mut system_txs: Vec<super::SystemTx>,
        receipts: Vec<LegacyReceipt>,
        chain_id: u64,
    ) -> HlBlock {
        // NOTE: These types of transactions are tracked at #97.
        system_txs.retain(|tx| tx.receipt.is_some());

        let mut merged_txs = vec![];
        merged_txs.extend(system_txs.iter().map(|tx| system_tx_to_reth_transaction(tx, chain_id)));
        merged_txs.extend(self.body.transactions.iter().map(|tx| tx.to_reth_transaction()));

        let mut merged_receipts = vec![];
        merged_receipts.extend(system_txs.iter().map(|tx| tx.receipt.clone().unwrap().into()));
        merged_receipts.extend(receipts.into_iter().map(From::from));

        let block_body = HlBlockBody {
            inner: reth_primitives::BlockBody {
                transactions: merged_txs,
                withdrawals: self.body.withdrawals.clone(),
                ommers: vec![],
            },
            sidecars: None,
            read_precompile_calls: Some(read_precompile_calls),
            highest_precompile_address,
        };

        let system_tx_count = system_txs.len() as u64;
        HlBlock {
            header: HlHeader::from_ethereum_header(
                self.header.header.clone(),
                &merged_receipts,
                system_tx_count,
            ),
            body: block_body,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::TxLegacy;
    use alloy_primitives::{Signature, TxKind, U256};

    /// Helper: build a minimal TransactionSigned for testing.
    fn make_tx(y_parity: bool) -> TransactionSigned {
        TransactionSigned {
            signature: Signature::new(U256::from(1), U256::from(2), y_parity),
            transaction: Transaction::Legacy(TxLegacy {
                chain_id: Some(998),
                nonce: 0,
                gas_price: 0,
                gas_limit: 21000,
                to: TxKind::Call(Address::ZERO),
                value: U256::ZERO,
                input: Bytes::new(),
            }),
        }
    }

    #[test]
    fn test_msgpack_roundtrip_standard_parity() {
        // Standard 0/1 parity should round-trip through msgpack without issue.
        for parity in [false, true] {
            let tx = make_tx(parity);
            let encoded = rmp_serde::to_vec(&tx).expect("serialize");
            let decoded: TransactionSigned =
                rmp_serde::from_slice(&encoded).expect("deserialize");
            assert_eq!(tx, decoded);
        }
    }

    #[test]
    fn test_msgpack_legacy_v_values() {
        // Simulate S3 blocks that encode v=27 or v=28 instead of 0/1.
        for (legacy_v, expected_parity) in [(27u64, false), (28u64, true)] {
            let tx = make_tx(expected_parity);

            // The Signature is serialized as a (U256, U256, U64) tuple in msgpack.
            // U64 is 8 bytes big-endian. Standard encoding writes 0x00..00 or 0x00..01.
            // We need to find and replace the last U64 in the signature tuple with legacy_v.
            //
            // Rather than fragile byte patching, let's construct the tuple directly:
            // Serialize the signature portion manually and rebuild the full message.
            let sig_tuple: (U256, U256, U64) =
                (U256::from(1), U256::from(2), U64::from(legacy_v));
            let sig_bytes = rmp_serde::to_vec(&sig_tuple).expect("serialize sig tuple");

            // Now serialize just the transaction portion
            let tx_bytes =
                rmp_serde::to_vec(&tx.transaction).expect("serialize transaction");

            // TransactionSigned is serialized as a 2-element array [signature, transaction]
            // in msgpack non-human-readable format (serde tuple struct).
            // Build it manually: fixarray(2) + sig_bytes + tx_bytes
            let mut patched = Vec::new();
            patched.push(0x92); // msgpack fixarray of 2 elements
            patched.extend_from_slice(&sig_bytes);
            patched.extend_from_slice(&tx_bytes);

            let decoded: TransactionSigned =
                rmp_serde::from_slice(&patched).expect(&format!(
                    "deserialize with legacy v={legacy_v} should succeed"
                ));
            assert_eq!(decoded.signature.v(), expected_parity);
        }
    }

    /// Integration test: deserialize real S3 block files from the local cache.
    /// Run with: cargo test -- --ignored test_deserialize_real_blocks
    #[test]
    #[ignore]
    fn test_deserialize_real_blocks() {
        use crate::node::types::BlockAndReceipts;
        use std::path::Path;

        let blocks_dir = Path::new(
            &std::env::var("BLOCKS_DIR").unwrap_or_else(|_| {
                let home = std::env::var("HOME").expect("HOME not set");
                format!("{home}/projects/hyperliquid/nfth-mm/data/blocks")
            }),
        )
        .to_path_buf();

        if !blocks_dir.exists() {
            eprintln!("Skipping: blocks dir not found at {blocks_dir:?}");
            return;
        }

        // Test blocks from the problematic 45.9M range — includes larger files
        // that are more likely to contain user transactions with signatures.
        let test_blocks = [45_900_126u64, 45_900_432, 45_900_512, 45_900_737];
        let mut tested = 0;

        for block_num in test_blocks {
            let million = (block_num / 1_000_000) * 1_000_000;
            let thousand = (block_num / 1_000) * 1_000;
            let path = blocks_dir
                .join(million.to_string())
                .join(thousand.to_string())
                .join(format!("{block_num}.rmp.lz4"));

            if !path.exists() {
                eprintln!("Skipping block {block_num}: file not found at {path:?}");
                continue;
            }

            let file = std::fs::read(&path).unwrap();
            let mut decoder = lz4_flex::frame::FrameDecoder::new(&file[..]);
            let blocks: Vec<BlockAndReceipts> = rmp_serde::from_read(&mut decoder)
                .unwrap_or_else(|e| panic!("Failed to deserialize block {block_num}: {e}"));

            assert!(!blocks.is_empty(), "Block file {block_num} was empty");
            let crate::node::types::EvmBlock::Reth115(ref sealed) = blocks[0].block;
            eprintln!(
                "OK: block {block_num} deserialized ({} txs)",
                sealed.body.transactions.len()
            );
            tested += 1;
        }

        assert!(tested > 0, "No block files found to test — check BLOCKS_DIR");
    }
}
