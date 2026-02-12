use super::BlockSource;
use crate::node::types::BlockAndReceipts;
use alloy_primitives::Bytes;
use futures::{FutureExt, future::BoxFuture};
use jsonrpsee::http_client::{HttpClient, HttpClientBuilder};
use jsonrpsee_core::client::ClientT;
use reth_metrics::{Metrics, metrics, metrics::Counter};
use std::{sync::Arc, time::Duration};
use tracing::info;

/// Block source that fetches blocks from a remote nanoreth node via RPC.
///
/// Connects to another nanoreth node running with `--enable-sync-server`
/// and fetches blocks through the `hl_sync` RPC namespace.
#[derive(Debug, Clone)]
pub struct RpcBlockSource {
    client: Arc<HttpClient>,
    polling_interval: Duration,
    metrics: RpcBlockSourceMetrics,
}

#[derive(Metrics, Clone)]
#[metrics(scope = "block_source.rpc")]
pub struct RpcBlockSourceMetrics {
    /// How many times the RPC block source is polling for a block
    pub polling_attempt: Counter,
    /// How many times the RPC block source has fetched a block
    pub fetched: Counter,
}

impl RpcBlockSource {
    pub fn new(url: String, polling_interval: Duration) -> Self {
        let client = HttpClientBuilder::default()
            .request_timeout(Duration::from_secs(30))
            .build(&url)
            .unwrap_or_else(|e| panic!("Failed to build RPC client for {url}: {e}"));
        info!("RPC block source connected to {url}");
        Self { client: Arc::new(client), polling_interval, metrics: RpcBlockSourceMetrics::default() }
    }
}

impl BlockSource for RpcBlockSource {
    fn collect_block(&self, height: u64) -> BoxFuture<'static, eyre::Result<BlockAndReceipts>> {
        let client = self.client.clone();
        let metrics = self.metrics.clone();
        async move {
            metrics.polling_attempt.increment(1);
            let bytes: Bytes = client.request("hl_syncGetBlock", (height,)).await?;
            let mut decoder = lz4_flex::frame::FrameDecoder::new(&bytes[..]);
            let blocks: Vec<BlockAndReceipts> = rmp_serde::from_read(&mut decoder)?;
            metrics.fetched.increment(1);
            Ok(blocks[0].clone())
        }
        .boxed()
    }

    fn find_latest_block_number(&self) -> BoxFuture<'static, Option<u64>> {
        let client = self.client.clone();
        async move {
            let result: Option<u64> =
                client.request("hl_syncLatestBlockNumber", Vec::<u64>::new()).await.ok()?;
            info!("Latest block number from remote: {:?}", result);
            result
        }
        .boxed()
    }

    fn recommended_chunk_size(&self) -> u64 {
        1000
    }

    fn polling_interval(&self) -> Duration {
        self.polling_interval
    }
}
