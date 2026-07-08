//! Live factory-discovery demo for Ethereum Uniswap V2 + V3 USDC/WETH pools.
//!
//! Flow:
//!   1. Query the Ethereum Uniswap V2 factory `getPair` mapping and Uniswap V3
//!      factory `getPool` mappings for USDC/WETH fee tiers (one batched
//!      discovery pass per factory).
//!   2. Bootstrap every discovered pool in one shot with
//!      [`AdapterRegistry::cold_start_many`]: it batches code-seed verification
//!      and bundles per-pool hydration into a single multicall `eth_call`,
//!      falling back per pool to the multi-round cold-start only where a fast
//!      hydration cannot run. Request count scales with bootstrap phases, not
//!      with pool count.
//!   3. Register the discovered pools with [`AmmSyncEngine`].
//!   4. Subscribe over WS to the exact discovered pool addresses + their
//!      adapter event topics, then feed logs back through the resync-capable
//!      runtime.
//!
//! Set `E2E_RPC_URL` to an Ethereum HTTP RPC endpoint. Set `ETH_WS_URL` to a
//! WebSocket endpoint, or let the example derive it by replacing `https://` with
//! `wss://` in `E2E_RPC_URL`.
//!
//! ```text
//! E2E_RPC_URL=https://... ETH_WS_URL=wss://... \
//!   cargo run --release --example factory_discovery_live
//! ```

use std::env;
use std::sync::Arc;
use std::time::{Duration, Instant};

use alloy_eips::BlockId;
use alloy_network::{AnyNetwork, Ethereum};
use alloy_primitives::{Address, address};
use alloy_provider::{Provider, RootProvider};
use alloy_rpc_client::RpcClient;
use alloy_rpc_types_eth::{Filter, Log as RpcLog};
use alloy_transport_http::Http;
use anyhow::{Context, Result, bail};
use evm_amm_state::adapters::{
    AdapterRegistry, AmmSyncEngine, ColdStartOutcome, ColdStartPolicy,
    ConcentratedLiquidityAdapter, DiscoverySource, FactoryConfig, PoolDiscovery, PoolKey,
    PoolQuery, PoolRegistration, PoolStatus, ProtocolId, ProtocolMetadata, UniswapV2Adapter,
    UniswapV2FactoryConfig, UniswapV3FactoryConfig, supports_one_shot_hydration,
};
use evm_fork_cache::cache::EvmCache;
use evm_fork_cache::reactive::{
    BlockRef, ChainStatus, InputSource, ReactiveContext, ReactiveInput, ReactiveInputBatch,
    ReactiveInputRecord,
};
use futures::StreamExt;

type SharedProvider = Arc<RootProvider<AnyNetwork>>;

const CHAIN_ID: u64 = 1;
const USDC: Address = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
const WETH: Address = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
const UNISWAP_V2_FACTORY: Address = address!("5C69bEe701ef814a2B6a3EDD4B1652CB9cc5aA6f");
const UNISWAP_V3_FACTORY: Address = address!("1F98431c8aD98523631AE4a59f267346ea31F984");
const UNISWAP_V3_USDC_WETH_FEES: [u32; 4] = [100, 500, 3_000, 10_000];

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let total_start = Instant::now();

    let Ok(rpc_url) = env::var("E2E_RPC_URL") else {
        println!("factory_discovery_live: set E2E_RPC_URL to an Ethereum RPC endpoint; skipping.");
        return Ok(());
    };
    let ws_url = ws_url_from_env(&rpc_url)?;
    let seconds = env_u64("FACTORY_DISCOVERY_SECONDS", 60);
    let max_events = env_u64("FACTORY_DISCOVERY_MAX_EVENTS", 12);
    let block_lag = env_u64("FACTORY_DISCOVERY_BLOCK_LAG", 8);

    let setup_start = Instant::now();
    let rpc = provider(&rpc_url)?;
    let latest = rpc.get_block_number().await.context("latest block")?;
    let pinned = latest.saturating_sub(block_lag);
    let mut cache = EvmCache::builder(rpc.clone())
        .block(BlockId::number(pinned))
        .build()
        .await;
    let setup_elapsed = setup_start.elapsed();

    println!(
        "factory_discovery_live: rpc={}, ws={}, latest={latest}, pinned={pinned}, window={}s, max_events={max_events}",
        redact_url(&rpc_url),
        redact_url(&ws_url),
        seconds
    );

    let discovery_start = Instant::now();
    let registry = registry_with_uniswap_adapters()?;
    let discovery = PoolDiscovery::for_registry(
        &registry,
        FactoryConfig::default()
            .with_uniswap_v2(
                UniswapV2FactoryConfig::uniswap_v2(UNISWAP_V2_FACTORY).with_fee_bps(30),
            )
            .with_uniswap_v3(
                UniswapV3FactoryConfig::uniswap_v3(UNISWAP_V3_FACTORY)
                    .with_fee_tiers(UNISWAP_V3_USDC_WETH_FEES),
            ),
    );

    let mut discovered = Vec::new();
    discovered.extend(
        discovery
            .find(
                &mut cache,
                PoolQuery::pair(USDC, WETH).on(ProtocolId::UniswapV2),
            )
            .context("query Uniswap V2 factory")?,
    );
    discovered.extend(
        discovery
            .find(
                &mut cache,
                PoolQuery::pair(USDC, WETH).on(ProtocolId::UniswapV3),
            )
            .context("query Uniswap V3 factory")?,
    );
    let discovery_elapsed = discovery_start.elapsed();
    if discovered.is_empty() {
        bail!("factory queries returned no USDC/WETH pools");
    }

    println!("discovered {} USDC/WETH pools:", discovered.len());
    for pool in &discovered {
        print_discovered_pool(&pool.key, &pool.registration, &pool.source);
    }

    // Collect the discovered registrations and bootstrap them all in one shot:
    // `cold_start_many` batches code-seed verification and bundles per-pool
    // hydration into a single multicall `eth_call`, falling back per pool only
    // where a fast hydration cannot run.
    let mut ready: Vec<PoolRegistration> = discovered
        .into_iter()
        .map(|pool| pool.registration)
        .collect();
    let fast_eligible = ready
        .iter()
        .filter(|p| supports_one_shot_hydration(p))
        .count();

    let cold_start_start = Instant::now();
    let outcomes = registry
        .cold_start_many(&mut ready, &mut cache, rpc.as_ref(), ColdStartPolicy::Eager)
        .await
        .context("bootstrap discovered pools")?;
    let cold_start_elapsed = cold_start_start.elapsed();

    // A fast one-shot hydration finalizes `Ready` with no per-slot planner
    // trail; a fallback `Ready` carries the multi-round planner's verified/
    // changed slots. Use that to report the split and to keep the slot tallies
    // meaningful in the benchmark line below.
    let mut verified_slots = 0_usize;
    let mut changed_slots = 0_usize;
    let mut hydrated_fast = 0_usize;
    let mut fell_back = 0_usize;
    for (registration, outcome) in ready.iter().zip(&outcomes) {
        match outcome {
            ColdStartOutcome::Ready(report) => {
                if report.verified_slots.is_empty() && report.changed_slots.is_empty() {
                    hydrated_fast += 1;
                } else {
                    fell_back += 1;
                    verified_slots += report.verified_slots.len();
                    changed_slots += report.changed_slots.len();
                }
            }
            other => bail!("{:?} did not reach Ready: {other:?}", registration.key),
        }
    }
    if !ready.iter().all(|pool| pool.status == PoolStatus::Ready) {
        bail!("not every discovered pool reached PoolStatus::Ready after bootstrap");
    }
    println!(
        "bootstrapped {} pools to Ready ({fast_eligible} fast-eligible: {hydrated_fast} hydrated one-shot, {fell_back} via cold-start fallback)",
        ready.len()
    );

    let register_start = Instant::now();
    let pool_addresses: Vec<_> = ready.iter().filter_map(|pool| pool.key.address()).collect();
    let pool_count = pool_addresses.len();
    let mut engine = AmmSyncEngine::new(registry)?;
    engine.register_pools(ready)?;
    let register_elapsed = register_start.elapsed();

    let topics = engine.registry().subscription_topics();
    if topics.is_empty() {
        bail!("registered pools exposed no subscription topics");
    }
    println!(
        "subscribing to {} pool addresses and {} topic0 values",
        pool_count,
        topics.len()
    );

    let subscribe_start = Instant::now();
    let ws = RootProvider::<AnyNetwork>::connect(&ws_url)
        .await
        .context("connect WS provider")?;
    let filter = Filter::new()
        .address(pool_addresses)
        .event_signature(topics);
    let mut stream = ws
        .subscribe_logs(&filter)
        .await
        .context("subscribe to discovered pools")?
        .into_stream();
    let subscribe_elapsed = subscribe_start.elapsed();

    let event_loop_start = Instant::now();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(seconds);
    let mut seen = 0_u64;
    let mut applied = 0_u64;
    let mut resync_updates = 0_usize;
    let mut resync_failures = 0_usize;
    let mut first_log_elapsed = None;
    let mut first_applied_elapsed = None;
    let mut cumulative_ingest = Duration::ZERO;
    let mut max_ingest = Duration::ZERO;
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => break,
            maybe_log = stream.next() => {
                let Some(log) = maybe_log else { break };
                first_log_elapsed.get_or_insert_with(|| event_loop_start.elapsed());
                seen += 1;
                let address = log.address();
                let block = log.block_number.unwrap_or_default();
                let topic0 = log.topics().first().copied();
                let batch = batch_for_log(log)?;
                let ingest_start = Instant::now();
                let report = engine.ingest_batch(&mut cache, batch)?;
                let ingest_elapsed = ingest_start.elapsed();
                cumulative_ingest += ingest_elapsed;
                max_ingest = max_ingest.max(ingest_elapsed);
                if !report.reactive.applied.is_empty() || report.resync_state_updates > 0 {
                    first_applied_elapsed.get_or_insert_with(|| event_loop_start.elapsed());
                    applied += 1;
                    resync_updates += report.resync_state_updates;
                    resync_failures += report.resync_failures;
                    println!(
                        "applied event #{applied}: address={address:?}, block={block}, topic0={topic0:?}, direct_effects={}, resync_updates={}, resync_failures={}, ingest={}",
                        report.reactive.applied.len(),
                        report.resync_state_updates,
                        report.resync_failures,
                        fmt_duration(ingest_elapsed)
                    );
                } else {
                    println!(
                        "ignored routed subscription log: address={address:?}, block={block}, topic0={topic0:?}, ingest={}",
                        fmt_duration(ingest_elapsed)
                    );
                }
                if applied >= max_events {
                    break;
                }
            }
        }
    }
    let event_loop_elapsed = event_loop_start.elapsed();

    println!(
        "done: seen_logs={seen}, applied_events={applied}, cumulative_resync_updates={resync_updates}, cumulative_resync_failures={resync_failures}"
    );
    println!(
        "benchmark: setup={}, factory_discovery={}, cold_start={}, engine_register={}, ws_subscribe={}, event_loop={}, total={}",
        fmt_duration(setup_elapsed),
        fmt_duration(discovery_elapsed),
        fmt_duration(cold_start_elapsed),
        fmt_duration(register_elapsed),
        fmt_duration(subscribe_elapsed),
        fmt_duration(event_loop_elapsed),
        fmt_duration(total_start.elapsed()),
    );
    println!(
        "benchmark_detail: pools={}, verified_slots={}, changed_slots={}, first_log={}, first_applied={}, ingest_total={}, ingest_avg={}, ingest_max={}",
        pool_count,
        verified_slots,
        changed_slots,
        fmt_optional_duration(first_log_elapsed),
        fmt_optional_duration(first_applied_elapsed),
        fmt_duration(cumulative_ingest),
        fmt_duration(avg_duration(cumulative_ingest, seen)),
        fmt_duration(max_ingest),
    );
    Ok(())
}

fn registry_with_uniswap_adapters() -> Result<AdapterRegistry> {
    let mut registry = AdapterRegistry::new();
    registry.register_adapter(Arc::new(UniswapV2Adapter::default()))?;
    registry.register_adapter(Arc::new(ConcentratedLiquidityAdapter::default()))?;
    Ok(registry)
}

fn print_discovered_pool(key: &PoolKey, registration: &PoolRegistration, source: &DiscoverySource) {
    match &registration.metadata {
        ProtocolMetadata::UniswapV2(metadata) => {
            println!(
                "  {key:?} from {source:?}: token0={:?}, token1={:?}, fee_bps={:?}",
                metadata.token0, metadata.token1, metadata.fee_bps
            );
        }
        ProtocolMetadata::UniswapV3(metadata) => {
            println!(
                "  {key:?} from {source:?}: token0={:?}, token1={:?}, fee={:?}, tick_spacing={:?}, factory={:?}",
                metadata.token0,
                metadata.token1,
                metadata.fee,
                metadata.tick_spacing,
                metadata.factory
            );
        }
        metadata => {
            println!("  {key:?} from {source:?}: metadata={metadata:?}");
        }
    }
}

fn batch_for_log(log: RpcLog) -> Result<ReactiveInputBatch<Ethereum>> {
    let ctx = ctx_from_log(&log)?;
    Ok(ReactiveInputBatch::new(vec![ReactiveInputRecord::new(
        ReactiveInput::Log(log),
        ctx,
    )]))
}

fn ctx_from_log(log: &RpcLog) -> Result<ReactiveContext> {
    let number = log.block_number.context("log missing block number")?;
    let hash = log.block_hash.context("log missing block hash")?;
    let block = BlockRef {
        number,
        hash,
        parent_hash: None,
        timestamp: log.block_timestamp,
    };
    Ok(ReactiveContext {
        chain_id: Some(CHAIN_ID),
        source: InputSource::Batch,
        chain_status: ChainStatus::Included {
            block: block.clone(),
            confirmations: 0,
        },
        block: Some(block),
        transaction_index: log.transaction_index,
        log_index: log.log_index,
    })
}

fn provider(url: &str) -> Result<SharedProvider> {
    let client = reqwest::Client::builder()
        .gzip(true)
        .build()
        .context("build reqwest client")?;
    let http = Http::with_client(client, url.parse().context("parse RPC URL")?);
    Ok(Arc::new(RootProvider::<AnyNetwork>::new(RpcClient::new(
        http, false,
    ))))
}

fn ws_url_from_env(rpc_url: &str) -> Result<String> {
    if let Ok(ws_url) = env::var("ETH_WS_URL") {
        return Ok(ws_url);
    }
    if let Some(rest) = rpc_url.strip_prefix("https://") {
        return Ok(format!("wss://{rest}"));
    }
    if let Some(rest) = rpc_url.strip_prefix("http://") {
        return Ok(format!("ws://{rest}"));
    }
    bail!("set ETH_WS_URL explicitly when E2E_RPC_URL is not http(s)")
}

fn env_u64(name: &str, default: u64) -> u64 {
    env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn avg_duration(total: Duration, samples: u64) -> Duration {
    if samples == 0 {
        Duration::ZERO
    } else {
        Duration::from_secs_f64(total.as_secs_f64() / samples as f64)
    }
}

fn fmt_optional_duration(duration: Option<Duration>) -> String {
    duration
        .map(fmt_duration)
        .unwrap_or_else(|| "n/a".to_string())
}

fn fmt_duration(duration: Duration) -> String {
    format!("{:.2}ms", duration.as_secs_f64() * 1_000.0)
}

fn redact_url(url: &str) -> String {
    match url.split_once("://") {
        Some((scheme, rest)) => {
            let host = rest.split('/').next().unwrap_or(rest);
            format!("{scheme}://{host}/...")
        }
        None => "<redacted>".to_string(),
    }
}
