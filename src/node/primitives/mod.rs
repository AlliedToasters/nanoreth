use reth_ethereum_primitives::Receipt;
use reth_primitives::NodePrimitives;

pub mod transaction;
pub use transaction::{BlockBody, TransactionSigned};

pub mod block;
pub use block::HlBlock;
pub mod body;
pub use body::HlBlockBody;
pub mod header;
pub use header::HlHeader;

pub mod rlp;
pub mod serde_bincode_compat;

/// Primitive types for HyperEVM.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct HlPrimitives;

impl NodePrimitives for HlPrimitives {
    type Block = HlBlock;
    type BlockHeader = HlHeader;
    type BlockBody = HlBlockBody;
    type SignedTx = TransactionSigned;
    type Receipt = Receipt;
}
