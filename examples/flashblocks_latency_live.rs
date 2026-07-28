//! Live Base benchmark comparing Flashblock and canonical WebSocket delivery.
//!
//! The benchmark subscribes to the Base Uniswap V3 USDC/WETH 0.05% pool through
//! one provider session. `pendingLogs` records are correlated with
//! `newFlashblocks`, applied to the disposable cache overlay, and quoted through
//! the V3 adapter. Canonical `logs` records for the same swaps are applied to an
//! independently bootstrapped vanilla cache and quoted through the same adapter.
//! Results are paired by `(transaction_hash, log_index)`.
//!
//! Set `BASE_HTTP_URL` and `BASE_WS_URL`. The default window is five minutes or
//! 100 canonical swaps, whichever happens first.

use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use alloy_eips::BlockId;
use alloy_network::{AnyNetwork, Ethereum};
use alloy_primitives::{Address, B256, U256, address, keccak256};
use alloy_provider::{Provider, RootProvider};
use alloy_rpc_client::RpcClient;
use alloy_rpc_types_eth::Filter;
use alloy_transport_http::Http;
use anyhow::{Context, Result, bail};
use evm_amm_state::adapters::{
    AdapterRegistry, AmmAdapter, AmmSyncEngine, ColdStartOutcome, ColdStartPolicy,
    ConcentratedLiquidityAdapter, FactoryConfig, PoolDiscovery, PoolQuery, PoolRegistration,
    PoolStatus, ProtocolId, SimConfig, UniswapV3FactoryConfig,
};
use evm_fork_cache::{
    cache::EvmCache,
    reactive::{
        AlloySubscriber, EventSubscriber, LogInterest, PreconfirmationMode, ProviderRef,
        ReactiveInput, ReactiveInputBatch, ReactiveInterest, SubscriberConfig, SubscriberMode,
    },
};
use tokio::time::timeout;

type SharedProvider = Arc<RootProvider<AnyNetwork>>;
type SwapKey = (B256, u64);

const CHAIN_ID: u64 = 8_453;
const BASE_UNISWAP_V3_FACTORY: Address = address!("33128a8fC17869897dcE68Ed026d694621f6FDfD");
const BASE_QUOTER_V2: Address = address!("3d4e44Eb1374240CE5F1B871ab261CD16335B76a");
const USDC: Address = address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913");
const WETH: Address = address!("4200000000000000000000000000000000000006");
const FEE: u32 = 500;
const QUOTE_AMOUNT_USDC: u64 = 1_000_000;

#[derive(Clone, Copy, Debug)]
struct PathTiming {
    received_at: Duration,
    cache_ready_at: Duration,
    amm_ready_at: Duration,
    cache_apply: Duration,
    quote: Duration,
}

#[derive(Default)]
struct PathCounters {
    notices: usize,
    cache_applied: usize,
    quotes: usize,
    quote_retries: usize,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let Ok(http_url) = std::env::var("BASE_HTTP_URL") else {
        println!("flashblocks_latency_live: set BASE_HTTP_URL and BASE_WS_URL; skipping");
        return Ok(());
    };
    let ws_url = std::env::var("BASE_WS_URL").context("set BASE_WS_URL")?;
    let run_seconds = env_u64("FLASHBLOCKS_BENCH_SECONDS", 300);
    let max_swaps = env_usize("FLASHBLOCKS_BENCH_MAX_SWAPS", 100);

    let setup_started = Instant::now();
    let rpc = provider(&http_url)?;
    let chain_id = rpc.get_chain_id().await.context("read Base chain id")?;
    if chain_id != CHAIN_ID {
        bail!("expected Base chain id {CHAIN_ID}, got {chain_id}");
    }
    let pinned_block = rpc.get_block_number().await.context("read Base head")?;
    let sim_config = SimConfig::default().with_v3_quoter(BASE_QUOTER_V2);
    let adapter = Arc::new(ConcentratedLiquidityAdapter::default());
    let mut registry = AdapterRegistry::new().with_sim_config(sim_config);
    registry.register_adapter(adapter.clone())?;

    let mut flash_cache = EvmCache::builder(rpc.clone())
        .block(BlockId::number(pinned_block))
        .build()
        .await;
    let discovered = discover_pool(&registry, &mut flash_cache)?;
    let pool_address = discovered
        .key
        .address()
        .context("discovered pool is not address-backed")?;
    let mut flash_registration = discovered.clone();
    bootstrap(
        &registry,
        &mut flash_registration,
        &mut flash_cache,
        rpc.as_ref(),
    )
    .await?;

    let mut vanilla_cache = EvmCache::builder(rpc.clone())
        .block(BlockId::number(pinned_block))
        .build()
        .await;
    let mut vanilla_registration = discovered;
    bootstrap(
        &registry,
        &mut vanilla_registration,
        &mut vanilla_cache,
        rpc.as_ref(),
    )
    .await?;

    let mut flash_engine = AmmSyncEngine::new(registry.clone())?;
    flash_engine.register_pools([flash_registration.clone()])?;
    let mut vanilla_engine = AmmSyncEngine::new(registry)?;
    vanilla_engine.register_pools([vanilla_registration.clone()])?;

    let ws = RootProvider::<Ethereum>::connect(&ws_url)
        .await
        .context("connect QuickNode Base WebSocket")?;
    let swap_topic = keccak256(b"Swap(address,address,int256,int256,uint160,uint128,int24)");
    let filter = Filter::new()
        .address(pool_address)
        .event_signature(swap_topic);
    let mut subscriber = AlloySubscriber::new(
        ws,
        SubscriberMode::PubSub,
        SubscriberConfig {
            preconfirmations: PreconfirmationMode::Required,
            max_batch_size: 256,
            ..SubscriberConfig::default()
        },
    )
    .with_provider_ref(ProviderRef::new("quicknode-base", 1));
    subscriber
        .register_interests(&[ReactiveInterest::Logs(LogInterest {
            provider_filter: filter,
            local_matcher: None,
            route_key: None,
        })])
        .await
        .context("register Base swap interest")?;

    println!(
        "benchmark_start: chain_id={CHAIN_ID}, block={pinned_block}, pool={pool_address}, fee={FEE}, limit={} swaps or {}s, setup={:.3}s",
        max_swaps,
        run_seconds,
        setup_started.elapsed().as_secs_f64()
    );

    let started = Instant::now();
    let deadline = Duration::from_secs(run_seconds);
    let mut flash = HashMap::<SwapKey, PathTiming>::new();
    let mut vanilla = HashMap::<SwapKey, PathTiming>::new();
    let mut flash_counters = PathCounters::default();
    let mut vanilla_counters = PathCounters::default();

    while started.elapsed() < deadline && vanilla.len() < max_swaps {
        let remaining = deadline.saturating_sub(started.elapsed());
        let batch = match timeout(remaining, subscriber.next_scoped_batch()).await {
            Ok(Ok(Some(batch))) => batch,
            Ok(Ok(None)) => break,
            Ok(Err(error)) => return Err(error).context("receive subscriber batch"),
            Err(_) => break,
        };
        let received_at = started.elapsed();
        let is_preconfirmed = batch
            .records()
            .iter()
            .all(|record| record.scope().is_preconfirmed());
        let is_canonical = batch
            .records()
            .iter()
            .all(|record| record.scope().is_canonical());
        if !is_preconfirmed && !is_canonical {
            bail!("subscriber returned a mixed-scope batch");
        }
        let keys = swap_keys(batch.records().iter().map(|record| record.record()))?;
        if keys.is_empty() {
            continue;
        }
        let reactive = batch.into_reactive_batch();

        if is_preconfirmed {
            flash_counters.notices += keys.len();
            let timing = apply_and_quote(
                "flashblock",
                started,
                received_at,
                &mut flash_engine,
                &mut flash_cache,
                reactive,
                adapter.as_ref(),
                &flash_registration,
                sim_config,
                &mut flash_counters,
            )
            .await?;
            for key in keys {
                flash.entry(key).or_insert(timing);
            }
        } else {
            vanilla_counters.notices += keys.len();
            let flash_canonical = reactive.clone();
            let timing = apply_and_quote(
                "canonical",
                started,
                received_at,
                &mut vanilla_engine,
                &mut vanilla_cache,
                reactive,
                adapter.as_ref(),
                &vanilla_registration,
                sim_config,
                &mut vanilla_counters,
            )
            .await?;
            for key in keys {
                vanilla.entry(key).or_insert(timing);
            }
            flash_engine
                .ingest_batch(&mut flash_cache, flash_canonical)
                .context("converge Flashblock cache to canonical swap")?;
        }
    }

    let elapsed = started.elapsed();
    let stop_reason = if vanilla.len() >= max_swaps {
        "swap_limit"
    } else {
        "time_limit"
    };
    print_results(
        elapsed,
        stop_reason,
        &flash,
        &vanilla,
        &flash_counters,
        &vanilla_counters,
    );
    if flash.is_empty() {
        bail!("no Flashblock swap notices were applied");
    }
    if vanilla.is_empty() {
        bail!("no canonical WebSocket swap notices were applied");
    }
    if flash.keys().all(|key| !vanilla.contains_key(key)) {
        bail!("no swaps were observed on both Flashblock and canonical paths");
    }
    Ok(())
}

fn discover_pool(registry: &AdapterRegistry, cache: &mut EvmCache) -> Result<PoolRegistration> {
    let discovery = PoolDiscovery::for_registry(
        registry,
        FactoryConfig::default().with_uniswap_v3(
            UniswapV3FactoryConfig::uniswap_v3(BASE_UNISWAP_V3_FACTORY).with_fee_tiers([FEE]),
        ),
    );
    let mut pools = discovery
        .find(cache, PoolQuery::pair(USDC, WETH).on(ProtocolId::UniswapV3))
        .context("discover Base Uniswap V3 USDC/WETH 0.05% pool")?;
    if pools.len() != 1 {
        bail!(
            "expected one Base Uniswap V3 0.05% pool, got {}",
            pools.len()
        );
    }
    Ok(pools.remove(0).registration)
}

async fn bootstrap<P>(
    registry: &AdapterRegistry,
    registration: &mut PoolRegistration,
    cache: &mut EvmCache,
    provider: &P,
) -> Result<()>
where
    P: Provider<AnyNetwork>,
{
    let outcomes = registry
        .cold_start_many(
            std::slice::from_mut(registration),
            cache,
            provider,
            ColdStartPolicy::Eager,
        )
        .await
        .context("cold-start Base Uniswap V3 pool")?;
    if !matches!(outcomes.as_slice(), [ColdStartOutcome::Ready(_)])
        || registration.status != PoolStatus::Ready
    {
        bail!("Base Uniswap V3 pool did not reach Ready: {outcomes:?}");
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn apply_and_quote(
    path: &str,
    started: Instant,
    received_at: Duration,
    engine: &mut AmmSyncEngine,
    cache: &mut EvmCache,
    batch: ReactiveInputBatch<Ethereum>,
    adapter: &dyn AmmAdapter,
    registration: &PoolRegistration,
    sim_config: SimConfig,
    counters: &mut PathCounters,
) -> Result<PathTiming> {
    let cache_started = Instant::now();
    let report = engine
        .ingest_batch(cache, batch)
        .context("apply swap to AMM cache")?;
    let cache_apply = cache_started.elapsed();
    let cache_ready_at = started.elapsed();
    if !report.reactive.applied.is_empty() {
        counters.cache_applied += 1;
    }

    let quote_started = Instant::now();
    let quote_retry_deadline = Duration::from_secs(3);
    loop {
        match adapter.simulate_swap(
            registration,
            cache,
            USDC,
            WETH,
            U256::from(QUOTE_AMOUNT_USDC),
            &sim_config,
        ) {
            Ok(_) => break,
            Err(error)
                if error.to_string().contains("block not found")
                    && quote_started.elapsed() < quote_retry_deadline =>
            {
                counters.quote_retries += 1;
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("quote against updated Base Uniswap V3 cache on {path} path")
                });
            }
        }
    }
    let quote = quote_started.elapsed();
    let amm_ready_at = started.elapsed();
    counters.quotes += 1;
    Ok(PathTiming {
        received_at,
        cache_ready_at,
        amm_ready_at,
        cache_apply,
        quote,
    })
}

fn swap_keys<'a>(
    records: impl Iterator<Item = &'a evm_fork_cache::reactive::ReactiveInputRecord<Ethereum>>,
) -> Result<Vec<SwapKey>> {
    let mut keys = Vec::new();
    for record in records {
        let ReactiveInput::Log(log) = &record.input else {
            bail!("swap subscriber emitted a non-log input");
        };
        let transaction_hash = log
            .transaction_hash
            .context("swap log missing transaction hash")?;
        let log_index = log.log_index.context("swap log missing log index")?;
        keys.push((transaction_hash, log_index));
    }
    Ok(keys)
}

fn print_results(
    elapsed: Duration,
    stop_reason: &str,
    flash: &HashMap<SwapKey, PathTiming>,
    vanilla: &HashMap<SwapKey, PathTiming>,
    flash_counters: &PathCounters,
    vanilla_counters: &PathCounters,
) {
    let matched = flash
        .iter()
        .filter_map(|(key, flash_timing)| {
            vanilla
                .get(key)
                .map(|vanilla_timing| (*flash_timing, *vanilla_timing))
        })
        .collect::<Vec<_>>();
    let notification_lead = matched
        .iter()
        .filter_map(|(flash, vanilla)| vanilla.received_at.checked_sub(flash.received_at))
        .collect::<Vec<_>>();
    let cache_lead = matched
        .iter()
        .filter_map(|(flash, vanilla)| vanilla.cache_ready_at.checked_sub(flash.cache_ready_at))
        .collect::<Vec<_>>();
    let amm_lead = matched
        .iter()
        .filter_map(|(flash, vanilla)| vanilla.amm_ready_at.checked_sub(flash.amm_ready_at))
        .collect::<Vec<_>>();
    let flash_apply = flash.values().map(|timing| timing.cache_apply).collect();
    let vanilla_apply = vanilla.values().map(|timing| timing.cache_apply).collect();
    let flash_quote = flash.values().map(|timing| timing.quote).collect();
    let vanilla_quote = vanilla.values().map(|timing| timing.quote).collect();

    println!(
        "benchmark_end: stop_reason={stop_reason}, elapsed={:.3}s, flash_swaps={}, canonical_swaps={}, paired_swaps={}",
        elapsed.as_secs_f64(),
        flash.len(),
        vanilla.len(),
        matched.len()
    );
    println!(
        "path_counts: flash_notices={}, flash_cache_batches={}, flash_quotes={}, flash_quote_retries={}, canonical_notices={}, canonical_cache_batches={}, canonical_quotes={}, canonical_quote_retries={}",
        flash_counters.notices,
        flash_counters.cache_applied,
        flash_counters.quotes,
        flash_counters.quote_retries,
        vanilla_counters.notices,
        vanilla_counters.cache_applied,
        vanilla_counters.quotes,
        vanilla_counters.quote_retries,
    );
    print_stats("notification_lead", notification_lead);
    print_stats("cache_ready_lead", cache_lead);
    print_stats("amm_quote_ready_lead", amm_lead);
    print_stats("flash_cache_apply", flash_apply);
    print_stats("canonical_cache_apply", vanilla_apply);
    print_stats("flash_quote", flash_quote);
    print_stats("canonical_quote", vanilla_quote);
}

fn print_stats(label: &str, mut values: Vec<Duration>) {
    if values.is_empty() {
        println!("latency_ms: metric={label}, samples=0");
        return;
    }
    values.sort_unstable();
    let mean = values.iter().map(Duration::as_secs_f64).sum::<f64>() / values.len() as f64;
    println!(
        "latency_ms: metric={label}, samples={}, min={:.3}, mean={:.3}, p50={:.3}, p95={:.3}, max={:.3}",
        values.len(),
        millis(values[0]),
        mean * 1_000.0,
        millis(percentile(&values, 50)),
        millis(percentile(&values, 95)),
        millis(*values.last().expect("non-empty")),
    );
}

fn percentile(values: &[Duration], percentile: usize) -> Duration {
    let index = ((values.len() - 1) * percentile).div_ceil(100);
    values[index]
}

fn millis(value: Duration) -> f64 {
    value.as_secs_f64() * 1_000.0
}

fn provider(url: &str) -> Result<SharedProvider> {
    let client = reqwest::Client::builder()
        .gzip(true)
        .build()
        .context("build HTTP client")?;
    let transport = Http::with_client(client, url.parse().context("parse Base HTTP URL")?);
    Ok(Arc::new(RootProvider::<AnyNetwork>::new(RpcClient::new(
        transport, false,
    ))))
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}
