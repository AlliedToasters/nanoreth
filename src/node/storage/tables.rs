use alloy_primitives::{BlockNumber, Bytes};
use reth_db::{TableSet, TableType, TableViewer, table::TableInfo, tables};
use std::fmt;

/// Static key used for spot metadata, as the database is unique to each chain.
/// This may later serve as a versioning key to assist with future database migrations.
pub const SPOT_METADATA_KEY: u64 = 0;

tables! {
    /// Read precompile calls for each block.
    table BlockReadPrecompileCalls {
        type Key = BlockNumber;
        type Value = Bytes;
    }

    /// Spot metadata mapping (EVM address to spot token index).
    /// Uses a constant key since the database is chain-specific.
    table SpotMetadata {
        type Key = u64;
        type Value = Bytes;
    }
}
