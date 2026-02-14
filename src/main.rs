use std::sync::Arc;

use clap::Parser;
use reth::{
    builder::{NodeBuilder, NodeHandle, WithLaunchContext},
    rpc::{api::EthPubSubApiServer, eth::RpcNodeCore},
};
use reth_db::DatabaseEnv;
use reth_hl::{
    addons::{
        call_forwarder::{self, CallForwarderApiServer},
        hl_node_compliance::install_hl_node_compliance,
        subscribe_fixup::SubscribeFixup,
        sync_server::{HlSyncApiServer, HlSyncServer, ProviderSyncReader, set_sync_db_reader},
        tx_forwarder::{self, EthForwarderApiServer},
    },
    chainspec::{HlChainSpec, parser::HlChainSpecParser},
    node::{
        HlNode,
        cli::{Cli, HlNodeArgs},
        rpc::precompile::{HlBlockPrecompileApiServer, HlBlockPrecompileExt},
        spot_meta::init as spot_meta_init,
        storage::tables::Tables,
        types::set_spot_metadata_db,
    },
};
use tracing::info;

// We use jemalloc for performance reasons
#[cfg(all(feature = "jemalloc", unix))]
#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

fn main() -> eyre::Result<()> {
    reth_cli_util::sigsegv_handler::install();

    // Initialize custom version metadata before parsing CLI so --version uses reth-hl values
    reth_hl::version::init_reth_hl_version();

    Cli::<HlChainSpecParser, HlNodeArgs>::parse().run(
        |builder: WithLaunchContext<NodeBuilder<Arc<DatabaseEnv>, HlChainSpec>>,
         ext: HlNodeArgs| async move {
            let default_upstream_rpc_url = builder.config().chain.official_rpc_url();

            let enable_sync_server = ext.enable_sync_server;
            let (node, engine_handle_tx) = HlNode::new(
                ext.block_source_args.parse().await?,
                ext.debug_cutoff_height,
                ext.allow_network_overrides,
            );
            let NodeHandle { node, node_exit_future: exit_future } = builder
                .node(node)
                .extend_rpc_modules(move |mut ctx| {
                    let upstream_rpc_url =
                        ext.upstream_rpc_url.unwrap_or_else(|| default_upstream_rpc_url.to_owned());

                    ctx.modules.replace_configured(
                        tx_forwarder::EthForwarderExt::new(upstream_rpc_url.clone()).into_rpc(),
                    )?;
                    info!("Transaction will be forwarded to {}", upstream_rpc_url);

                    if ext.forward_call {
                        ctx.modules.replace_configured(
                            call_forwarder::CallForwarderExt::new(
                                upstream_rpc_url.clone(),
                                ctx.registry.eth_api().clone(),
                            )
                            .into_rpc(),
                        )?;
                        info!("Call/gas estimation will be forwarded to {}", upstream_rpc_url);
                    }

                    // This is a temporary workaround to fix the issue with custom headers
                    // affects `eth_subscribe[type=newHeads]`
                    ctx.modules.replace_configured(
                        SubscribeFixup::new(
                            Arc::new(ctx.registry.eth_handlers().pubsub.clone()),
                            Arc::new(ctx.registry.eth_api().provider().clone()),
                            Box::new(ctx.node().task_executor.clone()),
                        )
                        .into_rpc(),
                    )?;

                    if ext.hl_node_compliant {
                        install_hl_node_compliance(&mut ctx)?;
                        info!("hl-node compliant mode enabled");
                    }

                    if !ext.experimental_eth_get_proof {
                        ctx.modules.remove_method_from_configured("eth_getProof");
                        info!("eth_getProof is disabled by default");
                    }

                    if enable_sync_server {
                        let provider = ctx.registry.eth_api().provider().clone();
                        set_sync_db_reader(Box::new(ProviderSyncReader::new(provider)));
                        ctx.modules.merge_configured(HlSyncServer.into_rpc())?;
                        info!("Sync server RPC enabled (serving blocks from database)");
                    }

                    ctx.modules.merge_configured(
                        HlBlockPrecompileExt::new(ctx.registry.eth_api().clone()).into_rpc(),
                    )?;

                    Ok(())
                })
                .apply(|mut builder| {
                    builder.db_mut().create_tables_for::<Tables>().expect("create tables");

                    let chain_id = builder.config().chain.inner.chain().id();
                    let db = builder.db_mut().clone();

                    // Set database handle for on-demand persistence
                    set_spot_metadata_db(db.clone());

                    // Load spot metadata from database and initialize cache
                    spot_meta_init::load_spot_metadata_cache(&db, chain_id);

                    builder
                })
                .launch()
                .await?;

            engine_handle_tx.send(node.beacon_engine_handle.clone()).unwrap();

            exit_future.await
        },
    )?;
    Ok(())
}
