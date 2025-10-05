use std::time::Duration;

use crate::node::rpc::{HlEthApi, HlRpcNodeCore};
use alloy_primitives::{B256, Bytes};
use reth::rpc::server_types::eth::EthApiError;
use reth_rpc_eth_api::{
    RpcConvert,
    helpers::{EthTransactions, LoadTransaction, spec::SignersForRpc},
};

impl<N, Rpc> LoadTransaction for HlEthApi<N, Rpc>
where
    N: HlRpcNodeCore,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = EthApiError>,
{
}

impl<N, Rpc> EthTransactions for HlEthApi<N, Rpc>
where
    N: HlRpcNodeCore,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = EthApiError>,
{
    fn signers(&self) -> &SignersForRpc<Self::Provider, Self::NetworkTypes> {
        self.inner.eth_api.signers()
    }

    async fn send_raw_transaction(&self, _tx: Bytes) -> Result<B256, Self::Error> {
        unreachable!()
    }

    fn send_raw_transaction_sync_timeout(&self) -> Duration {
        self.inner.eth_api.send_raw_transaction_sync_timeout()
    }
}
