use alloy_primitives::{BlockNumber, Bytes};
use reth_db::{TableSet, TableType, TableViewer, table::TableInfo, tables};
use std::fmt;

tables! {
    /// Read precompile calls for each block.
    table BlockReadPrecompileCalls {
        type Key = BlockNumber;
        type Value = Bytes;
    }
}
