//! End-to-end adapters-path demo over a live WebSocket endpoint.
//!
//! Walks the full pipeline against a busy Uniswap V2 pool (USDC/WETH):
//!   1. Build an [`EvmCache`] pinned at the latest block `B0`.
//!   2. Register the pool and **cold-start** it — warm its reserves + token
//!      slots into the cache from forked storage (no further RPC needed to read
//!      them back).
//!   3. **Subscribe** to the pool's `Sync` logs over a `wss://` endpoint and
//!      **apply them reactively** through the resync-capable AMM runtime,
//!      mutating the cached reserves in place with zero storage refetch.
//!   4. **`simulate_swap`** against the live-synced cached state.
//!
//! This is the same plumbing exercised by `tests/reactive_ws_e2e.rs`
//! (`EvmCache` + `AdapterRegistry` + `AmmSyncEngine` + `simulate_swap`),
//! packaged as a runnable demo.
//!
//! Endpoint: set `ETH_WS_URL` to a `wss://`/`ws://` URL, or `E2E_RPC_URL` to an
//! `https://`/`http://` URL (the `wss://` URL is derived from it). If neither is
//! set the example prints a hint and exits successfully — it never panics.
//!
//! ```text
//! ETH_WS_URL=wss://… cargo run --example adapter_pipeline
//! # or
//! E2E_RPC_URL=https://… cargo run --example adapter_pipeline
//! ```

use std::sync::Arc;
use std::time::Duration;

use alloy_eips::{BlockId, BlockNumberOrTag};
use alloy_network::AnyNetwork;
use alloy_primitives::{Address, U256, address, keccak256};
use alloy_provider::{Provider, RootProvider};
use alloy_rpc_types_eth::{Filter, Log as RpcLog};
use anyhow::{Context, Result, anyhow};
use evm_amm_state::adapters::storage::V2_RESERVES_SLOT;
use evm_amm_state::adapters::{
    AdapterRegistry, AmmAdapter, AmmSyncEngine, ColdStartPolicy, PoolKey, PoolRegistration,
    ProtocolMetadata, SimConfig, UniswapV2Adapter, UniswapV2Metadata,
};
use evm_fork_cache::cache::EvmCache;
use evm_fork_cache::reactive::{
    BlockRef, ChainStatus, InputSource, ReactiveContext, ReactiveInput, ReactiveInputBatch,
    ReactiveInputRecord,
};
use futures::StreamExt;

const USDC: Address = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
const WETH: Address = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
const V2_ROUTER_02: Address = address!("7a250d5630B4cF539739dF2C5dAcb4c659F2488D");
const V2_USDC_WETH_PAIR: Address = address!("B4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc");

fn mask112() -> U256 {
    (U256::from(1) << 112) - U256::from(1)
}

/// The cached `(reserve0, reserve1)` for the pair (low 224 bits of the packed
/// reserves slot; the top 32 timestamp bits are ignored).
fn cached_reserves(cache: &EvmCache) -> (U256, U256) {
    let raw = cache
        .cached_storage_value(V2_USDC_WETH_PAIR, V2_RESERVES_SLOT)
        .unwrap_or_default();
    (raw & mask112(), (raw >> 112) & mask112())
}

/// Build a `ReactiveContext` from a subscription log (block + index metadata).
fn ctx_from_log(log: &RpcLog) -> ReactiveContext {
    let number = log.block_number.unwrap_or_default();
    let hash = log.block_hash.unwrap_or_default();
    let block = BlockRef {
        number,
        hash,
        parent_hash: None,
        timestamp: log.block_timestamp,
    };
    ReactiveContext {
        chain_id: Some(1),
        source: InputSource::Synthetic,
        chain_status: ChainStatus::Included {
            block: block.clone(),
            confirmations: 0,
        },
        block: Some(block),
        transaction_index: log.transaction_index,
        log_index: log.log_index,
    }
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    // Resolve the WS endpoint from ETH_WS_URL (preferred) or derive it from
    // E2E_RPC_URL. If neither is set, print a hint and exit cleanly.
    let ws_url = match std::env::var("ETH_WS_URL") {
        Ok(url) => url,
        Err(_) => match std::env::var("E2E_RPC_URL") {
            Ok(rpc) => rpc
                .replacen("https://", "wss://", 1)
                .replacen("http://", "ws://", 1),
            Err(_) => {
                println!(
                    "adapter_pipeline: set ETH_WS_URL=wss://… (or E2E_RPC_URL=https://…) to run \
                     the live demo; skipping."
                );
                return Ok(());
            }
        },
    };

    let secs: u64 = std::env::var("ADAPTER_PIPELINE_SECONDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(60);

    let provider = Arc::new(
        RootProvider::<AnyNetwork>::connect(&ws_url)
            .await
            .context("connect wss:// endpoint")?,
    );

    // 1. Pin the cache backend at the latest block B0.
    let b0 = provider.get_block_number().await.context("latest block")?;
    println!(
        "[adapter_pipeline] pinned fork at B0={b0}; cold-starting V2 USDC/WETH and applying Sync \
         events for {secs}s"
    );
    let mut cache = EvmCache::at_block(
        provider.clone(),
        BlockId::Number(BlockNumberOrTag::Number(b0)),
    )
    .await;

    // 2. Register + cold-start the pair (warms reserves + token slots at B0).
    let mut registration = PoolRegistration::new(PoolKey::UniswapV2(V2_USDC_WETH_PAIR))
        .with_state_address(V2_USDC_WETH_PAIR)
        .with_metadata(ProtocolMetadata::UniswapV2(
            UniswapV2Metadata::default()
                .with_token0(USDC)
                .with_token1(WETH)
                .with_fee_bps(30),
        ));
    {
        let mut cold = AdapterRegistry::new();
        cold.register_adapter(Arc::new(UniswapV2Adapter::default()))?;
        cold.cold_start(&mut registration, &mut cache, ColdStartPolicy::Eager)?;
    }
    let r0 = cached_reserves(&cache);
    println!(
        "[adapter_pipeline] cold-start reserves at B0: ({}, {})",
        r0.0, r0.1
    );

    // 3. Reactive runtime with the pair registered + its event sources wired.
    let adapter = UniswapV2Adapter::default();
    let sources = adapter.event_sources(&registration);
    let mut registry = AdapterRegistry::new();
    registry.register_adapter(Arc::new(UniswapV2Adapter::default()))?;
    registry.register_pool(registration.clone().with_event_sources(sources))?;
    let mut sync = AmmSyncEngine::new(registry)?;

    // Subscribe topic-only to all Uniswap-V2 `Sync` events; the reactive handler
    // routes each log by address, so only the registered pair's Syncs are
    // applied — everything else is ignored.
    let sync_topic = keccak256("Sync(uint112,uint112)");
    let filter = Filter::new().event_signature(sync_topic);
    let mut stream = provider
        .subscribe_logs(&filter)
        .await
        .context("subscribe_logs (needs a wss endpoint)")?
        .into_stream();

    let mut applied = 0u64;
    let mut last_block = b0;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => break,
            maybe_log = stream.next() => {
                let Some(log) = maybe_log else { break };
                let block_n = log.block_number.unwrap_or(last_block);
                let ctx = ctx_from_log(&log);
                let batch = ReactiveInputBatch::new(vec![ReactiveInputRecord::new(
                    ReactiveInput::Log(log),
                    ctx,
                )]);
                let report = sync.ingest_batch(&mut cache, batch)?;
                if !report.reactive.applied.is_empty() {
                    applied += 1;
                    last_block = block_n;
                    let r = cached_reserves(&cache);
                    println!(
                        "[adapter_pipeline] applied Sync #{applied} at block {last_block}; \
                         reserves ({}, {})",
                        r.0, r.1
                    );
                }
            }
        }
    }

    if applied == 0 {
        println!(
            "[adapter_pipeline] no USDC/WETH Sync events arrived in {secs}s (pool quiet or WS not \
             delivering). Simulating against the cold-start state instead."
        );
    }

    // 4. Simulate a 1 USDC -> WETH swap against the (live-synced) cached state.
    let amount_in = U256::from(1_000_000_u64); // 1 USDC (6 decimals)
    let config = SimConfig::default().with_v2_router(V2_ROUTER_02);
    let sim = adapter
        .simulate_swap(&registration, &mut cache, USDC, WETH, amount_in, &config)
        .map_err(|e| anyhow!("simulate_swap failed: {e}"))?;
    println!(
        "[adapter_pipeline] simulate_swap(1 USDC -> WETH) = {} wei WETH (over state at block {})",
        sim.amount_out, last_block
    );
    println!("[adapter_pipeline] done.");
    Ok(())
}
