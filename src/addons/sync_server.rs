use crate::pseudo_peer::sources::BlockSourceBoxed;
use alloy_primitives::Bytes;
use futures::StreamExt;
use jsonrpsee::proc_macros::rpc;
use jsonrpsee_core::{RpcResult, async_trait};
use reth::rpc::result::internal_rpc_err;
use std::sync::OnceLock;
use tracing::trace;

static BLOCK_SOURCE: OnceLock<BlockSourceBoxed> = OnceLock::new();

/// Set the block source for the sync server.
/// Called during node startup after the block source is created.
pub fn set_sync_block_source(source: BlockSourceBoxed) {
    BLOCK_SOURCE.set(source).ok();
}

fn get_sync_block_source() -> RpcResult<&'static BlockSourceBoxed> {
    BLOCK_SOURCE
        .get()
        .ok_or_else(|| internal_rpc_err("Sync server not yet initialized"))
}

/// RPC trait for node-to-node block syncing.
///
/// Exposes blocks from this node's block source so other nanoreth nodes
/// can sync without needing direct S3 access.
#[rpc(server, namespace = "hl")]
#[async_trait]
pub trait HlSyncApi {
    /// Returns a block at the given height, serialized as msgpack+lz4 bytes.
    #[method(name = "syncGetBlock")]
    async fn sync_get_block(&self, height: u64) -> RpcResult<Bytes>;

    /// Returns multiple blocks by height, serialized as msgpack+lz4 bytes.
    /// Heights are capped at 100 per request.
    #[method(name = "syncGetBlocks")]
    async fn sync_get_blocks(&self, heights: Vec<u64>) -> RpcResult<Bytes>;

    /// Returns the latest block number available from this node's block source.
    #[method(name = "syncLatestBlockNumber")]
    async fn sync_latest_block_number(&self) -> RpcResult<Option<u64>>;
}

pub struct HlSyncServer;

#[async_trait]
impl HlSyncApiServer for HlSyncServer {
    async fn sync_get_block(&self, height: u64) -> RpcResult<Bytes> {
        trace!(target: "rpc::hl", height, "Serving hl_syncGetBlock");
        let source = get_sync_block_source()?;
        let block = source
            .collect_block(height)
            .await
            .map_err(|e| internal_rpc_err(format!("Failed to collect block {height}: {e}")))?;

        // Encode as msgpack + lz4 (same format as S3/local block sources)
        // Use write_named (map format) to match the S3/Go msgpack format.
        // The default write() uses compact/array format which causes
        // deserialization errors due to field ordering differences.
        let mut encoder = lz4_flex::frame::FrameEncoder::new(Vec::new());
        rmp_serde::encode::write_named(&mut encoder, &vec![block])
            .map_err(|e| internal_rpc_err(format!("Failed to serialize block: {e}")))?;
        let compressed = encoder
            .finish()
            .map_err(|e| internal_rpc_err(format!("Failed to compress block: {e}")))?;

        Ok(Bytes::from(compressed))
    }

    async fn sync_get_blocks(&self, heights: Vec<u64>) -> RpcResult<Bytes> {
        const MAX_BATCH: usize = 100;
        let heights = if heights.len() > MAX_BATCH { &heights[..MAX_BATCH] } else { &heights };
        trace!(target: "rpc::hl", count = heights.len(), "Serving hl_syncGetBlocks");
        let source = get_sync_block_source()?;

        let futs: Vec<_> = heights.iter().map(|&h| source.collect_block(h)).collect();
        let blocks: Vec<_> = futures::stream::iter(futs)
            .buffered(MAX_BATCH)
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<_, _>>()
            .map_err(|e| internal_rpc_err(format!("Failed to collect blocks: {e}")))?;

        let mut encoder = lz4_flex::frame::FrameEncoder::new(Vec::new());
        rmp_serde::encode::write_named(&mut encoder, &blocks)
            .map_err(|e| internal_rpc_err(format!("Failed to serialize blocks: {e}")))?;
        let compressed = encoder
            .finish()
            .map_err(|e| internal_rpc_err(format!("Failed to compress blocks: {e}")))?;
        Ok(Bytes::from(compressed))
    }

    async fn sync_latest_block_number(&self) -> RpcResult<Option<u64>> {
        trace!(target: "rpc::hl", "Serving hl_syncLatestBlockNumber");
        let source = get_sync_block_source()?;
        Ok(source.find_latest_block_number().await)
    }
}
