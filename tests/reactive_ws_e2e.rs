//! Live WebSocket reactive E2E (env-gated, `#[ignore]`) — MANAGER RUNS THIS.
//!
//! Proves the NEW (adapters) pipeline keeps pool state correct using ONLY
//! WebSocket log events — never re-querying storage. Flow:
//!   1. Pin a fork at `B0` (latest at start) and cold-start a busy Uniswap V2
//!      pool (warms reserves + token slots at `B0`).
//!   2. For ~5 minutes, apply ONLY `Sync` events arriving over a `wss://`
//!      subscription, through the real reactive runtime (`ingest_batch` →
//!      masked write of the new reserves). No storage refetch.
//!   3. Assert the event-synced reserves match on-chain at the last event's
//!      block `N`, and `simulate_swap` matches the on-chain `getAmountsOut`
//!      quote at `N`.
//!
//! No-cheat discriminator: the cache backend is pinned at `B0`, so any sneaky
//! refetch would yield the STALE `B0` reserves. We assert the sim does NOT equal
//! the `B0` quote, and (when the chain has moved) does NOT equal the live-head
//! `M` quote — so the only way it matches `N` is by reading the event-sourced
//! state.
//!
//! Uniswap V2 is used because its `Sync` event carries the EXACT new reserves,
//! making the masked write pure event-sourcing with zero refetch. (V3
//! tick-crossing and Balancer's refresh-on-event are not "events-only" by
//! construction, so they are out of scope for this specific claim.)
//!
//! Run (derives `wss://` from `E2E_RPC_URL`; default 300s, override
//! `E2E_WS_SECONDS`):
//! ```text
//! E2E_RPC_URL=<https-archive> cargo test --test reactive_ws_e2e -- --ignored --nocapture
//! ```

use std::sync::Arc;
use std::time::Duration;

use alloy_eips::{BlockId, BlockNumberOrTag};
use alloy_network::{AnyNetwork, Ethereum, TransactionBuilder};
use alloy_primitives::{Address, Bytes, U256, address, keccak256};
use alloy_provider::{Provider, RootProvider};
use alloy_rpc_types_eth::{Filter, Log as RpcLog, TransactionRequest};
use alloy_sol_types::SolCall;
use anyhow::{Context, Result, anyhow};
use evm_amm_state::adapters::sim::getAmountsOutCall;
use evm_amm_state::adapters::storage::V2_RESERVES_SLOT;
use evm_amm_state::adapters::{
    AdapterRegistry, AmmAdapter, AmmReactiveHandler, ColdStartPolicy, PoolKey, PoolRegistration,
    ProtocolMetadata, SimConfig, UniswapV2Adapter, UniswapV2Metadata,
};
use evm_fork_cache::cache::EvmCache;
use evm_fork_cache::reactive::{
    BlockRef, ChainStatus, InputSource, ReactiveConfig, ReactiveContext, ReactiveInput,
    ReactiveInputBatch, ReactiveInputRecord, ReactiveRuntime,
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
/// reserves slot; the top 32 timestamp bits are ignored — `Sync` does not carry
/// a timestamp, so the masked write intentionally leaves them at the cold-start
/// value, and `getAmountsOut` does not depend on them).
fn cached_reserves(cache: &EvmCache) -> (U256, U256) {
    let raw = cache
        .cached_storage_value(V2_USDC_WETH_PAIR, V2_RESERVES_SLOT)
        .unwrap_or_default();
    (raw & mask112(), (raw >> 112) & mask112())
}

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

/// On-chain `Router02.getAmountsOut(amountIn, [USDC, WETH])` at `block` (ground
/// truth).
async fn amounts_out_at(
    provider: &RootProvider<AnyNetwork>,
    block: u64,
    amount_in: U256,
) -> Result<U256> {
    let calldata = Bytes::from(
        getAmountsOutCall {
            amountIn: amount_in,
            path: vec![USDC, WETH],
        }
        .abi_encode(),
    );
    let tx = TransactionRequest::default()
        .with_to(V2_ROUTER_02)
        .with_input(calldata);
    let out = provider
        .call(tx.into())
        .block(BlockId::Number(BlockNumberOrTag::Number(block)))
        .await
        .with_context(|| format!("eth_call getAmountsOut at block {block}"))?;
    let amounts = getAmountsOutCall::abi_decode_returns_validate(&out)?;
    Ok(*amounts.last().ok_or_else(|| anyhow!("empty amounts"))?)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "live WS subscription against E2E_RPC_URL; run with --ignored --nocapture"]
async fn ws_v2_reactive_sync_keeps_state_for_accurate_sim() -> Result<()> {
    let Ok(rpc) = std::env::var("E2E_RPC_URL") else {
        eprintln!("E2E_RPC_URL unset; skipping");
        return Ok(());
    };
    let ws_url = rpc
        .replacen("https://", "wss://", 1)
        .replacen("http://", "ws://", 1);
    let secs: u64 = std::env::var("E2E_WS_SECONDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(300);

    let provider = Arc::new(
        RootProvider::<AnyNetwork>::connect(&ws_url)
            .await
            .context("connect wss:// (derived from E2E_RPC_URL)")?,
    );

    let b0 = provider.get_block_number().await.context("latest block")?;
    eprintln!(
        "[ws-e2e] pinned fork at B0={b0}; cold-starting V2 USDC/WETH and collecting Sync for {secs}s"
    );

    // 1. Pin the cache backend at B0 and cold-start the pair.
    let mut cache = EvmCache::at_block(
        provider.clone(),
        BlockId::Number(BlockNumberOrTag::Number(b0)),
    )
    .await;

    let mut registration = PoolRegistration::new(PoolKey::UniswapV2(V2_USDC_WETH_PAIR))
        .with_state_address(V2_USDC_WETH_PAIR)
        .with_metadata(ProtocolMetadata::UniswapV2(UniswapV2Metadata {
            token0: Some(USDC),
            token1: Some(WETH),
            fee_bps: Some(30),
        }));
    {
        let mut cold = AdapterRegistry::new();
        cold.register_adapter(Arc::new(UniswapV2Adapter::default()))?;
        cold.cold_start(&mut registration, &mut cache, ColdStartPolicy::Eager)?;
    }
    let r0 = cached_reserves(&cache);
    eprintln!("[ws-e2e] B0 reserves: ({}, {})", r0.0, r0.1);

    // 2. Reactive runtime with the pair registered; subscribe to its Sync logs.
    let adapter = UniswapV2Adapter::default();
    let sources = adapter.event_sources(&registration);
    let mut registry = AdapterRegistry::new();
    registry.register_adapter(Arc::new(UniswapV2Adapter::default()))?;
    registry.register_pool(registration.clone().with_event_sources(sources))?;
    let mut runtime = ReactiveRuntime::<Ethereum>::new(ReactiveConfig::default());
    runtime.register_handler(Arc::new(AmmReactiveHandler::new(registry)))?;

    // Subscribe TOPIC-ONLY (all Uniswap-V2 `Sync`). This provider does not
    // reliably push ADDRESS-filtered log subscriptions (an address+topic
    // subscription delivered 0 in 5 min while topic-only delivered 42 in 45s),
    // and topic-only is also higher-rate so the window reliably contains our
    // pair's Syncs. The reactive handler routes each log by address, so ONLY the
    // registered USDC/WETH pair's Syncs are applied — everything else is ignored.
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
                let report = runtime.ingest_batch(&mut cache, batch)?;
                if !report.applied.is_empty() {
                    applied += 1;
                    last_block = block_n;
                    if applied.is_multiple_of(5) {
                        let r = cached_reserves(&cache);
                        eprintln!("[ws-e2e] applied {applied} Syncs; block {last_block}; reserves ({}, {})", r.0, r.1);
                    }
                }
            }
        }
    }
    eprintln!(
        "[ws-e2e] window done: {applied} Sync events applied; last event block N={last_block}"
    );

    // 3. Assertions.
    assert!(
        applied > 0,
        "no Sync events arrived in {secs}s — pool inactive or WS not delivering"
    );
    let r_event = cached_reserves(&cache);
    eprintln!(
        "[ws-e2e] event-synced reserves at N={last_block}: ({}, {})",
        r_event.0, r_event.1
    );
    assert!(
        r_event != r0,
        "reserves never changed during the window — inconclusive (need on-chain swaps)"
    );

    let amount_in = U256::from(1_000_000_u64); // 1 USDC
    let config = SimConfig::default().with_v2_router(V2_ROUTER_02);
    let sim = adapter
        .simulate_swap(&registration, &mut cache, USDC, WETH, amount_in, &config)
        .map_err(|e| anyhow!("simulate_swap failed: {e}"))?;

    // PRIMARY: sim over event-synced state == on-chain getAmountsOut at block N.
    let q_n = amounts_out_at(&provider, last_block, amount_in).await?;
    eprintln!("[ws-e2e] sim={} | eth_call@N={}", sim.amount_out, q_n);
    assert_eq!(
        sim.amount_out, q_n,
        "sim must match on-chain getAmountsOut at the last event block N"
    );

    // NO-CHEAT 1: sim must NOT equal the pinned cold-start (B0) quote. The
    // backend is pinned at B0, so a sneaky refetch would yield B0 reserves;
    // since reserves changed, matching N (not B0) proves event-sourced reads.
    let q_b0 = amounts_out_at(&provider, b0, amount_in).await?;
    assert_ne!(
        sim.amount_out, q_b0,
        "sim must NOT equal the B0 cold-start quote (would indicate a backend refetch, not event sync)"
    );

    // NO-CHEAT 2: if the chain moved past N, sim must NOT equal the live-head
    // quote (would indicate a live refetch instead of frozen event-state).
    let m = provider.get_block_number().await?;
    if m > last_block {
        let q_m = amounts_out_at(&provider, m, amount_in).await?;
        if q_m != q_n {
            assert_ne!(
                sim.amount_out, q_m,
                "sim must reflect event-state (N), not the live head (M)"
            );
            eprintln!("[ws-e2e] no-cheat: sim==Q_N({q_n}) != Q_M({q_m}) at live block {m}");
        } else {
            eprintln!("[ws-e2e] live head {m} quote unchanged vs N; no-cheat-2 inconclusive");
        }
    }

    eprintln!("[ws-e2e] PASS: WebSocket-event-only sync kept state accurate for swap simulation.");
    Ok(())
}

/// Fast (~45s) subscription health probe: does the derived `wss://` actually
/// PUSH logs? Subscribes to ALL Uniswap-V2-style `Sync` events (topic-only, no
/// address filter — hundreds per minute on mainnet) and asserts it receives
/// some. Isolates "subscription transport works" from "the target pool was
/// quiet during the window".
#[tokio::test(flavor = "multi_thread")]
#[ignore = "live WS health probe; run with --ignored --nocapture"]
async fn ws_subscription_health_probe() -> Result<()> {
    let Ok(rpc) = std::env::var("E2E_RPC_URL") else {
        eprintln!("E2E_RPC_URL unset; skipping");
        return Ok(());
    };
    let ws_url = rpc
        .replacen("https://", "wss://", 1)
        .replacen("http://", "ws://", 1);
    let provider = Arc::new(
        RootProvider::<AnyNetwork>::connect(&ws_url)
            .await
            .context("connect wss://")?,
    );

    let sync_topic = keccak256("Sync(uint112,uint112)");
    let filter = Filter::new().event_signature(sync_topic);
    let mut stream = provider
        .subscribe_logs(&filter)
        .await
        .context("subscribe_logs")?
        .into_stream();

    let mut count = 0u64;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(45);
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => break,
            maybe = stream.next() => match maybe {
                Some(_) => count += 1,
                None => break,
            },
        }
    }
    eprintln!(
        "[ws-probe] received {count} all-V2 Sync events in 45s over the derived wss subscription"
    );
    assert!(
        count > 0,
        "WS subscription delivered ZERO logs in 45s despite heavy on-chain activity — transport not pushing"
    );
    Ok(())
}
