//! Live OP Stack benchmark comparing Flashblock and canonical WebSocket delivery.
//!
//! The benchmark subscribes to a Uniswap V3 USDC/WETH pool on Base or Optimism
//! through one provider generation. Base correlates native `pendingLogs` with
//! `newFlashblocks`; Optimism samples its bounded pending-state surface.
//! Flashblocks apply to the disposable cache overlay and quote through the V3
//! adapter. Canonical `logs` records for the same swaps apply to an independently
//! bootstrapped vanilla cache and quote through the same adapter. Results are
//! paired by `(transaction_hash, log_index)`.
//!
//! Set `FLASHBLOCKS_BENCH_CHAIN=base` or `optimism` with the corresponding
//! `<CHAIN>_HTTP_URL` / `<CHAIN>_WS_URL`. The default window is five minutes or
//! 100 canonical swaps, whichever happens first.

use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
    time::{Duration, Instant},
};

use alloy_eips::{BlockId, BlockNumberOrTag};
use alloy_network::{AnyNetwork, Ethereum};
use alloy_primitives::{Address, B256, U256, address, keccak256};
use alloy_provider::{DynProvider, Provider, RootProvider};
use alloy_rpc_client::RpcClient;
use alloy_rpc_types_eth::{Filter, Header};
use alloy_transport_http::Http;
use anyhow::{Context, Result, bail, ensure};
use evm_amm_state::adapters::{
    AdapterRegistry, AmmSyncEngine, ColdStartOutcome, ColdStartPolicy,
    ConcentratedLiquidityAdapter, FactoryConfig, PoolDiscovery, PoolQuery, PoolRegistration,
    PoolStatus, ProtocolId, ProtocolMetadata, QuoteWarmup, SimConfig, StorageAccessList,
    UniswapV3FactoryConfig,
};
use evm_fork_cache::{
    CacheSpeedMode,
    cache::EvmCache,
    reactive::{
        AlloySubscriber, BlockRef, ChainControl, ChainStatus, EventSubscriber, InputSource,
        LogInterest, PreconfirmationMode, ProviderRef, ReactiveContext, ReactiveInput,
        ReactiveInputBatch, ReactiveInputRecord, ReactiveInterest, SubscriberConfig,
        SubscriberMode,
    },
};
use tokio::time::timeout;

type SharedProvider = Arc<RootProvider<AnyNetwork>>;
type SwapKey = (B256, u64);

const BASE_UNISWAP_V3_FACTORY: Address = address!("33128a8fC17869897dcE68Ed026d694621f6FDfD");
const BASE_QUOTER_V2: Address = address!("3d4e44Eb1374240CE5F1B871ab261CD16335B76a");
const BASE_USDC: Address = address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913");
const OP_UNISWAP_V3_FACTORY: Address = address!("1F98431c8aD98523631AE4a59f267346ea31F984");
const OP_QUOTER_V2: Address = address!("61fFE014bA17989E743c5F6cB21bF9697530B21e");
const OP_USDC: Address = address!("0b2C639c533813f4Aa9D7837CAf62653d097Ff85");
const WETH: Address = address!("4200000000000000000000000000000000000006");
const FEE_TIERS: [u32; 4] = [100, 500, 3_000, 10_000];
const QUOTE_AMOUNT_USDC: u64 = 1_000_000;
const QUOTE_AMOUNT_WETH: u64 = 1_000_000_000_000_000;

#[derive(Clone, Copy)]
struct BenchChain {
    name: &'static str,
    chain_id: u64,
    env_prefix: &'static str,
    factory: Address,
    quoter: Address,
    usdc: Address,
}

impl BenchChain {
    fn from_env() -> Result<Self> {
        match std::env::var("FLASHBLOCKS_BENCH_CHAIN")
            .unwrap_or_else(|_| "base".to_owned())
            .to_ascii_lowercase()
            .as_str()
        {
            "base" => Ok(Self {
                name: "base",
                chain_id: 8_453,
                env_prefix: "BASE",
                factory: BASE_UNISWAP_V3_FACTORY,
                quoter: BASE_QUOTER_V2,
                usdc: BASE_USDC,
            }),
            "op" | "optimism" => Ok(Self {
                name: "optimism",
                chain_id: 10,
                env_prefix: "OPTIMISM",
                factory: OP_UNISWAP_V3_FACTORY,
                quoter: OP_QUOTER_V2,
                usdc: OP_USDC,
            }),
            value => bail!("unsupported FLASHBLOCKS_BENCH_CHAIN {value:?}; use base or optimism"),
        }
    }

    fn env(self, suffix: &str) -> String {
        format!("{}_{suffix}", self.env_prefix)
    }
}

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
    let chain = BenchChain::from_env()?;
    let http_variable = chain.env("HTTP_URL");
    let ws_variable = chain.env("WS_URL");
    let Ok(http_url) = std::env::var(&http_variable) else {
        println!("flashblocks_latency_live: set {http_variable} and {ws_variable}; skipping");
        return Ok(());
    };
    let ws_url = std::env::var(&ws_variable).with_context(|| format!("set {ws_variable}"))?;
    let provider_id =
        std::env::var("FLASHBLOCKS_PROVIDER_ID").unwrap_or_else(|_| format!("paid-{}", chain.name));
    let run_seconds = env_u64("FLASHBLOCKS_BENCH_SECONDS", 300);
    let max_swaps = env_usize("FLASHBLOCKS_BENCH_MAX_SWAPS", 100);
    let default_setup_pause_ms =
        u64::from(provider_id.to_ascii_lowercase().contains("quicknode")) * 1_100;
    let setup_pause = Duration::from_millis(env_u64(
        "FLASHBLOCKS_SETUP_PAUSE_MS",
        default_setup_pause_ms,
    ));

    let setup_started = Instant::now();
    let rpc = provider(&http_url)?;
    let chain_id = rpc
        .get_chain_id()
        .await
        .context("read benchmark chain id")?;
    if chain_id != chain.chain_id {
        bail!(
            "expected {} chain id {}, got {chain_id}",
            chain.name,
            chain.chain_id
        );
    }
    let pinned_block = rpc
        .get_block_number()
        .await
        .context("read benchmark head")?;
    let canonical_provider = subscriber_http_provider(&http_url)?;
    let baseline_header = canonical_provider
        .get_block_by_number(BlockNumberOrTag::Number(pinned_block))
        .await?
        .context("canonical baseline unavailable")?
        .header;
    ensure!(
        baseline_header.hash == baseline_header.inner.hash_slow(),
        "invalid baseline header hash"
    );
    let sim_config = SimConfig::default().with_v3_quoter(chain.quoter);
    let adapter = Arc::new(ConcentratedLiquidityAdapter::default());
    let mut registry = AdapterRegistry::new().with_sim_config(sim_config);
    registry.register_adapter(adapter.clone())?;

    let mut flash_cache = EvmCache::builder(rpc.clone())
        .block(BlockId::from((baseline_header.hash, Some(true))))
        .chain_id(chain.chain_id)
        .speed_mode(CacheSpeedMode::XSlow)
        .max_concurrent_proofs(1)
        .build()
        .await;
    let discovered = discover_pools(chain, &registry, &mut flash_cache)?;
    let (discovered, selected_fee, recent_swaps) = select_active_pool(
        rpc.as_ref(),
        discovered,
        pinned_block,
        env_u64("FLASHBLOCKS_ACTIVITY_LOOKBACK_BLOCKS", 300),
    )
    .await?;
    let pool_address = discovered
        .key
        .address()
        .context("discovered pool is not address-backed")?;
    pace_setup(setup_pause).await;
    let mut flash_registration = discovered.clone();
    bootstrap(
        &registry,
        &mut flash_registration,
        &mut flash_cache,
        rpc.as_ref(),
        chain,
    )
    .await?;
    pace_setup(setup_pause).await;

    let mut vanilla_cache = EvmCache::builder(rpc.clone())
        .block(BlockId::from((baseline_header.hash, Some(true))))
        .chain_id(chain.chain_id)
        .speed_mode(CacheSpeedMode::XSlow)
        .max_concurrent_proofs(1)
        .build()
        .await;
    let mut vanilla_registration = discovered;
    bootstrap(
        &registry,
        &mut vanilla_registration,
        &mut vanilla_cache,
        rpc.as_ref(),
        chain,
    )
    .await?;
    pace_setup(setup_pause).await;

    let warmups = representative_warmups(&flash_registration, chain)?;
    let mut flash_registry = registry.clone();
    flash_registry.register_pool(flash_registration.clone())?;
    warm_and_verify(
        "flashblock",
        &mut flash_registry,
        &mut flash_cache,
        &warmups,
    )?;
    pace_setup(setup_pause).await;
    let mut vanilla_registry = registry;
    vanilla_registry.register_pool(vanilla_registration.clone())?;
    warm_and_verify(
        "canonical",
        &mut vanilla_registry,
        &mut vanilla_cache,
        &warmups,
    )?;

    let mut flash_engine = AmmSyncEngine::new(flash_registry)?;
    let mut vanilla_engine = AmmSyncEngine::new(vanilla_registry)?;
    let mut flash_head = block_ref(&baseline_header);
    let mut vanilla_head = flash_head;
    let mut canonical_proofs = CanonicalProofs::default();
    for (engine, cache) in [
        (&mut flash_engine, &mut flash_cache),
        (&mut vanilla_engine, &mut vanilla_cache),
    ] {
        set_header_context(cache, &baseline_header);
        engine.ingest_batch(
            cache,
            ReactiveInputBatch::new(Vec::new())
                .with_chain_id(chain.chain_id)
                .with_chain_controls([ChainControl::CanonicalProgress(flash_head)]),
        )?;
    }

    let ws_connect_timeout = Duration::from_secs(env_u64("FLASHBLOCKS_CONNECT_SECONDS", 15));
    let ws = timeout(
        ws_connect_timeout,
        RootProvider::<Ethereum>::connect(&ws_url),
    )
    .await
    .with_context(|| {
        format!(
            "connect {} WebSocket within {} seconds",
            chain.name,
            ws_connect_timeout.as_secs()
        )
    })?
    .with_context(|| format!("connect {} WebSocket", chain.name))?;
    let filter = Filter::new().address(pool_address);
    let state_provider = subscriber_http_provider(&http_url)?;
    let mut subscriber = AlloySubscriber::new(
        ws.erased(),
        SubscriberMode::PubSub,
        SubscriberConfig {
            preconfirmations: PreconfirmationMode::Required,
            max_batch_size: 256,
            ..SubscriberConfig::default()
        },
    )
    .with_provider_ref(ProviderRef::new(provider_id, 1))
    .with_flashblocks_state_provider(state_provider);
    subscriber
        .register_interests(&[ReactiveInterest::Logs(LogInterest {
            provider_filter: filter,
            local_matcher: None,
            route_key: None,
        })])
        .await
        .with_context(|| format!("register {} swap interest", chain.name))?;
    let preflight = timeout(
        ws_connect_timeout,
        subscriber.establish_flashblocks_preflight(chain.chain_id),
    )
    .await
    .with_context(|| {
        format!(
            "establish {} Flashblocks subscriptions within {} seconds",
            chain.name,
            ws_connect_timeout.as_secs()
        )
    })?
    .with_context(|| format!("preflight {} Flashblocks endpoint", chain.name))?;

    println!(
        "benchmark_start: chain={}, chain_id={}, provider={}, generation={}, delivery={:?}, pending_log_subscriptions={}, pending_log_filters={}, capabilities_advertised={}, block={pinned_block}, pool={pool_address}, fee={selected_fee}, recent_swaps={recent_swaps}, limit={} swaps or {}s, setup={:.3}s, setup_pause={}ms",
        chain.name,
        chain.chain_id,
        preflight.provider().endpoint,
        preflight.provider().generation,
        preflight.delivery(),
        preflight.pending_log_subscriptions(),
        preflight.pending_log_filters(),
        preflight.advertised_capabilities().is_some(),
        max_swaps,
        run_seconds,
        setup_started.elapsed().as_secs_f64(),
        setup_pause.as_millis()
    );

    let started = Instant::now();
    let deadline = Duration::from_secs(run_seconds);
    // Poll the two subscription lanes continuously. Canonical proof RPC work
    // must not delay pending-log correlation or become notification latency.
    let (deliveries_tx, mut deliveries_rx) = tokio::sync::mpsc::channel(256);
    let (stop_tx, mut stop_rx) = tokio::sync::watch::channel(false);
    let collector =
        tokio::spawn(async move {
            loop {
                let remaining = deadline.saturating_sub(started.elapsed());
                if remaining.is_zero() {
                    break;
                }
                let next = tokio::select! {
                    _ = stop_rx.changed() => break,
                    next = timeout(remaining, subscriber.next_scoped_batch()) => next,
                };
                match next {
                    Ok(Ok(Some(batch))) => deliveries_tx
                        .try_send((started.elapsed(), batch))
                        .map_err(|_| {
                            anyhow::anyhow!(
                                "subscription collector queue unavailable; timing sample rejected"
                            )
                        })?,
                    Ok(Ok(None)) | Err(_) => break,
                    Ok(Err(error)) => {
                        return Err(anyhow::anyhow!(error).context("receive subscriber batch"));
                    }
                }
            }
            Ok(subscriber.flashblocks_rpc_metrics())
        });
    let mut flash = HashMap::<SwapKey, PathTiming>::new();
    let mut vanilla = HashMap::<SwapKey, PathTiming>::new();
    let mut flash_counters = PathCounters::default();
    let mut vanilla_counters = PathCounters::default();

    while started.elapsed() < deadline && vanilla.len() < max_swaps {
        let remaining = deadline.saturating_sub(started.elapsed());
        let (received_at, batch) = match timeout(remaining, deliveries_rx.recv()).await {
            Ok(Some(delivery)) => delivery,
            Ok(None) | Err(_) => break,
        };
        if batch.preconfirmation_invalidated() {
            flash_engine.discard_preconfirmation(&mut flash_cache);
        }
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
        if batch.records().is_empty() {
            continue;
        }
        let reactive = batch.into_reactive_batch();

        if is_preconfirmed {
            let ChainStatus::Preconfirmed { flashblock } =
                &reactive.records()[0].context.chain_status
            else {
                bail!("preconfirmed batch lacks exact lineage");
            };
            if flashblock.block_number <= flash_head.number {
                continue; // A queued preview cannot replace a newer canonical baseline.
            }
            let parent = flashblock
                .block_number
                .checked_sub(1)
                .context("invalid preview height")?;
            reconcile_canonical_path(
                &canonical_provider,
                chain.chain_id,
                pool_address,
                parent,
                &mut canonical_proofs,
                &mut flash_head,
                &mut flash_engine,
                &mut flash_cache,
            )
            .await?;
            ensure!(
                flashblock.parent_hash == Some(flash_head.hash),
                "preview parent disagrees with canonical proof"
            );
            flash_counters.notices += keys.len();
            let timing = apply_and_quote(
                "flashblock",
                started,
                received_at,
                &mut flash_engine,
                &mut flash_cache,
                reactive,
                &flash_registration.key,
                &mut flash_counters,
            )
            .await?;
            for key in keys {
                flash.entry(key).or_insert(timing);
            }
        } else {
            vanilla_counters.notices += keys.len();
            let target = reactive
                .records()
                .iter()
                .filter_map(|record| record.context.block.map(|block| block.number))
                .max()
                .context("canonical notice lacks a block")?;
            let cache_started = Instant::now();
            reconcile_canonical_path(
                &canonical_provider,
                chain.chain_id,
                pool_address,
                target,
                &mut canonical_proofs,
                &mut vanilla_head,
                &mut vanilla_engine,
                &mut vanilla_cache,
            )
            .await?;
            let cache_apply = cache_started.elapsed();
            let cache_ready_at = started.elapsed();
            let quote_started = Instant::now();
            vanilla_engine
                .validate_quote_read_sets(vanilla_cache.snapshot(), [&vanilla_registration.key])?;
            let timing = PathTiming {
                received_at,
                cache_ready_at,
                amm_ready_at: started.elapsed(),
                cache_apply,
                quote: quote_started.elapsed(),
            };
            vanilla_counters.cache_applied += 1;
            vanilla_counters.quotes += 1;
            for key in keys {
                vanilla.entry(key).or_insert(timing);
            }
            reconcile_canonical_path(
                &canonical_provider,
                chain.chain_id,
                pool_address,
                target,
                &mut canonical_proofs,
                &mut flash_head,
                &mut flash_engine,
                &mut flash_cache,
            )
            .await?;
            let retained_from = flash_head.number.min(vanilla_head.number);
            canonical_proofs
                .blocks
                .retain(|number, _| *number > retained_from);
        }
    }

    println!(
        "canonical_proof_rpc: verified_blocks={}, header_requests={}, hash_pinned_log_requests={}",
        canonical_proofs.fetched, canonical_proofs.fetched, canonical_proofs.fetched
    );
    println!(
        "canonical_quote_refresh: passes={}, accounts={}, slots={}",
        canonical_proofs.hydration_passes,
        canonical_proofs.hydrated_accounts,
        canonical_proofs.hydrated_slots
    );
    let elapsed = started.elapsed();
    let stop_reason = if vanilla.len() >= max_swaps {
        "swap_limit"
    } else {
        "time_limit"
    };
    let _ = stop_tx.send(true);
    let rpc_metrics = collector.await.context("join subscription collector")??;
    print_rpc_metrics(rpc_metrics, elapsed);
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
    let paired = vanilla.keys().filter(|key| flash.contains_key(key)).count();
    if stop_reason == "time_limit" && vanilla.len() < 20 {
        bail!(
            "active-pool acceptance requires at least 20 canonical swaps in a timed window; observed {}",
            vanilla.len()
        );
    }
    if paired * 100 < vanilla.len() * 95 {
        bail!(
            "Flashblock correlation acceptance requires at least 95% coverage; paired {paired} of {} canonical swaps",
            vanilla.len()
        );
    }
    if flash_counters.quote_retries != 0 || vanilla_counters.quote_retries != 0 {
        bail!("warmed quote acceptance observed an unexpected provider-read retry");
    }
    let mut cache_to_quote = flash
        .values()
        .filter_map(|timing| timing.amm_ready_at.checked_sub(timing.received_at))
        .collect::<Vec<_>>();
    cache_to_quote.sort_unstable();
    let cache_to_quote_p95 = percentile(&cache_to_quote, 95);
    let max_cache_to_quote =
        Duration::from_millis(env_u64("FLASHBLOCKS_MAX_CACHE_TO_QUOTE_MS", 25));
    if cache_to_quote_p95 > max_cache_to_quote {
        bail!(
            "Flashblock cache-to-quote p95 {:.3}ms exceeds the {:.3}ms acceptance ceiling",
            millis(cache_to_quote_p95),
            millis(max_cache_to_quote)
        );
    }
    Ok(())
}

fn print_rpc_metrics(metrics: evm_fork_cache::reactive::FlashblocksRpcMetrics, elapsed: Duration) {
    let requests_per_second = if elapsed.is_zero() {
        0.0
    } else {
        metrics.total_requests() as f64 / elapsed.as_secs_f64()
    };
    println!(
        "flashblocks_rpc: capabilities={}, provider_pair_chains={}, canonical_heads={}, pending_blocks={}, pending_logs={}, pending_receipts={}, receipts_completed={}, receipts_unavailable={}, failed_requests={}, raced_samples={}, total={}, mean_requests_per_second={requests_per_second:.3}",
        metrics.capability_requests(),
        metrics.provider_pair_chain_requests(),
        metrics.canonical_head_requests(),
        metrics.pending_block_requests(),
        metrics.pending_log_requests(),
        metrics.pending_receipt_requests(),
        metrics.pending_receipts_completed(),
        metrics.pending_receipts_unavailable(),
        metrics.failed_requests(),
        metrics.raced_samples(),
        metrics.total_requests()
    );
}

fn discover_pools(
    chain: BenchChain,
    registry: &AdapterRegistry,
    cache: &mut EvmCache,
) -> Result<Vec<PoolRegistration>> {
    let discovery = PoolDiscovery::for_registry(
        registry,
        FactoryConfig::default().with_uniswap_v3(
            UniswapV3FactoryConfig::uniswap_v3(chain.factory).with_fee_tiers(FEE_TIERS),
        ),
    );
    let pools = discovery
        .find(
            cache,
            PoolQuery::pair(chain.usdc, WETH).on(ProtocolId::UniswapV3),
        )
        .with_context(|| format!("discover {} Uniswap V3 USDC/WETH pools", chain.name))?;
    if pools.is_empty() {
        bail!("no {} Uniswap V3 USDC/WETH pool was discovered", chain.name);
    }
    Ok(pools.into_iter().map(|pool| pool.registration).collect())
}

async fn select_active_pool<P>(
    provider: &P,
    candidates: Vec<PoolRegistration>,
    head: u64,
    lookback_blocks: u64,
) -> Result<(PoolRegistration, u32, usize)>
where
    P: Provider<AnyNetwork>,
{
    let swap_topic = keccak256(b"Swap(address,address,int256,int256,uint160,uint128,int24)");
    let from = head.saturating_sub(lookback_blocks);
    let mut ranked = Vec::with_capacity(candidates.len());
    for registration in candidates {
        let address = registration
            .key
            .address()
            .context("discovered V3 pool is not address-backed")?;
        let fee = match &registration.metadata {
            ProtocolMetadata::UniswapV3(metadata) => metadata.fee,
            _ => None,
        }
        .context("discovered Uniswap V3 pool omitted its fee")?;
        let filter = Filter::new()
            .address(address)
            .event_signature(swap_topic)
            .from_block(BlockNumberOrTag::Number(from))
            .to_block(BlockNumberOrTag::Number(head));
        let swaps = provider
            .get_logs(&filter)
            .await
            .with_context(|| format!("measure recent activity for V3 pool {address}"))?
            .len();
        println!(
            "pool_candidate: address={address}, fee={fee}, from_block={from}, to_block={head}, swaps={swaps}"
        );
        ranked.push((swaps, fee, registration));
    }
    ranked.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
    let (swaps, fee, registration) = ranked
        .into_iter()
        .next()
        .context("active-pool ranking is empty")?;
    Ok((registration, fee, swaps))
}

async fn bootstrap<P>(
    registry: &AdapterRegistry,
    registration: &mut PoolRegistration,
    cache: &mut EvmCache,
    provider: &P,
    chain: BenchChain,
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
        .with_context(|| format!("cold-start {} Uniswap V3 pool", chain.name))?;
    if !matches!(outcomes.as_slice(), [ColdStartOutcome::Ready(_)])
        || registration.status != PoolStatus::Ready
    {
        bail!(
            "{} Uniswap V3 pool did not reach Ready: {outcomes:?}",
            chain.name
        );
    }
    Ok(())
}

fn representative_warmups(
    registration: &PoolRegistration,
    chain: BenchChain,
) -> Result<Vec<QuoteWarmup>> {
    let pool = registration.key.clone();
    Ok(vec![
        QuoteWarmup::exact_input(
            pool.clone(),
            chain.usdc,
            WETH,
            U256::from(QUOTE_AMOUNT_USDC),
        ),
        QuoteWarmup::exact_input(
            pool.clone(),
            chain.usdc,
            WETH,
            U256::from(QUOTE_AMOUNT_USDC) * U256::from(1_000),
        ),
        QuoteWarmup::exact_input(
            pool.clone(),
            WETH,
            chain.usdc,
            U256::from(QUOTE_AMOUNT_WETH),
        ),
        QuoteWarmup::exact_input(
            pool,
            WETH,
            chain.usdc,
            U256::from(QUOTE_AMOUNT_WETH) * U256::from(1_000),
        ),
    ])
}

fn warm_and_verify(
    path: &str,
    registry: &mut AdapterRegistry,
    cache: &mut EvmCache,
    warmups: &[QuoteWarmup],
) -> Result<()> {
    let initial = registry
        .warm_quote_read_sets(cache, warmups.iter().cloned())
        .with_context(|| format!("warm {path} representative quote read sets"))?;
    let repeated = registry
        .warm_quote_read_sets(cache, warmups.iter().cloned())
        .with_context(|| format!("repeat {path} representative quote warmup"))?;
    let repeated_provider_reads =
        repeated
            .entries()
            .iter()
            .fold(StorageAccessList::default(), |mut combined, entry| {
                combined.extend(entry.provider_reads());
                combined
            });
    if !repeated_provider_reads.is_empty() {
        bail!(
            "{path} representative quote warmup repeated provider reads: {} accounts, {} code hashes, {} slots, {} block hashes",
            repeated_provider_reads.accounts.len(),
            repeated_provider_reads.code_hashes.len(),
            repeated_provider_reads.slots.len(),
            repeated_provider_reads.block_numbers.len()
        );
    }
    let pool = &warmups
        .first()
        .context("representative warmup set is empty")?
        .pool;
    registry
        .validate_quote_read_sets(cache.snapshot(), [pool])
        .with_context(|| format!("validate {path} representative quotes offline"))?;
    let initial_provider_reads =
        initial
            .entries()
            .iter()
            .fold(StorageAccessList::default(), |mut combined, entry| {
                combined.extend(entry.provider_reads());
                combined
            });
    println!(
        "read_set_warmup: path={path}, quotes={}, first_touch_accounts={}, first_touch_code_hashes={}, first_touch_slots={}, first_touch_block_hashes={}, repeated_provider_reads=0",
        initial.entries().len(),
        initial_provider_reads.accounts.len(),
        initial_provider_reads.code_hashes.len(),
        initial_provider_reads.slots.len(),
        initial_provider_reads.block_numbers.len(),
    );
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
    pool: &evm_amm_state::adapters::PoolKey,
    counters: &mut PathCounters,
) -> Result<PathTiming> {
    if std::env::var_os("FLASHBLOCKS_TRACE_TRANSITIONS").is_some() {
        for record in batch.records() {
            eprintln!(
                "transition_input: path={path}, block={:?}, log_index={:?}, slot0={:?}, input={:?}",
                record.context.block.map(|b| b.number),
                record.context.log_index,
                pool.address()
                    .and_then(|a| cache.snapshot().storage_value(a, U256::ZERO)),
                record.input
            );
        }
    }
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
    engine
        .validate_quote_read_sets(cache.snapshot(), [pool])
        .map_err(|error| {
            for applied in &report.reactive.applied {
                for signal in &applied.hook_signals {
                    if signal.kind == "amm.decode_error" {
                        let payload = signal.payload.as_ref().and_then(|payload| {
                            payload.downcast_ref::<evm_amm_state::adapters::AmmReactiveSignal>()
                        });
                        eprintln!(
                            "transition_rejected: input={:?}, error={payload:?}",
                            applied.input_ref
                        );
                    }
                }
            }
            anyhow::anyhow!("validate warmed quotes offline on {path} path: {error:?}")
        })?;
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

fn block_ref(header: &Header) -> BlockRef {
    BlockRef {
        number: header.inner.number,
        hash: header.hash,
        parent_hash: Some(header.inner.parent_hash),
        timestamp: Some(header.inner.timestamp),
    }
}

fn set_header_context(cache: &mut EvmCache, header: &Header) {
    cache.set_block(BlockId::from((header.hash, Some(true))));
    cache.set_block_context(Some(header.inner.number), header.inner.base_fee_per_gas);
    cache.set_coinbase(Some(header.inner.beneficiary));
    cache.set_prevrandao(Some(header.inner.mix_hash));
    cache.set_block_gas_limit(Some(header.inner.gas_limit));
    cache.set_timestamp(Some(header.inner.timestamp));
}

#[derive(Default)]
struct CanonicalProofs {
    blocks: BTreeMap<u64, (Header, ReactiveInputBatch<Ethereum>)>,
    fetched: u64,
    hydrated_accounts: usize,
    hydrated_slots: usize,
    hydration_passes: usize,
}

// Subscription arrival is not proof that a block's complete log set arrived.
// Reconcile every intervening block by hash, including blocks without pool logs,
// before advancing the coverage baseline or accepting its speculative successor.
#[allow(clippy::too_many_arguments)]
async fn reconcile_canonical_path(
    provider: &DynProvider<Ethereum>,
    chain_id: u64,
    pool: Address,
    target: u64,
    proofs: &mut CanonicalProofs,
    head: &mut BlockRef,
    engine: &mut AmmSyncEngine,
    cache: &mut EvmCache,
) -> Result<()> {
    ensure!(
        target.saturating_sub(head.number) <= 300,
        "canonical catch-up exceeds bounded benchmark window"
    );
    let starting_number = head.number;
    while head.number < target {
        let number = head
            .number
            .checked_add(1)
            .context("canonical block overflow")?;
        if let std::collections::btree_map::Entry::Vacant(entry) = proofs.blocks.entry(number) {
            let header = provider
                .get_block_by_number(BlockNumberOrTag::Number(number))
                .await?
                .context("canonical catch-up block unavailable")?
                .header;
            ensure!(
                header.inner.number == number && header.hash == header.inner.hash_slow(),
                "invalid canonical header"
            );
            ensure!(
                header.inner.parent_hash == head.hash,
                "canonical lineage changed; restart benchmark from a fresh snapshot"
            );
            let block = block_ref(&header);
            let logs = provider
                .get_logs(&Filter::new().address(pool).at_block_hash(block.hash))
                .await?;
            let mut previous = None;
            let mut records = Vec::with_capacity(logs.len());
            for log in logs {
                ensure!(
                    !log.removed
                        && log.address() == pool
                        && log.block_number == Some(number)
                        && log.block_hash == Some(block.hash),
                    "log response disagrees with requested canonical block"
                );
                let position = (
                    log.transaction_index
                        .context("missing transaction position")?,
                    log.log_index.context("missing log position")?,
                );
                ensure!(
                    log.transaction_hash.is_some() && previous.is_none_or(|last| last < position),
                    "incomplete, duplicate, or unordered canonical log"
                );
                previous = Some(position);
                records.push(ReactiveInputRecord::new(
                    ReactiveInput::Log(log),
                    ReactiveContext {
                        chain_id: Some(chain_id),
                        source: InputSource::Backfill,
                        chain_status: ChainStatus::Included {
                            block,
                            confirmations: 0,
                        },
                        block: Some(block),
                        transaction_index: Some(position.0),
                        log_index: Some(position.1),
                    },
                ));
            }
            let batch = ReactiveInputBatch::new(records)
                .with_chain_id(chain_id)
                .with_chain_controls([ChainControl::CanonicalProgress(block)]);
            entry.insert((header, batch));
            proofs.fetched += 1;
        }
        let (header, batch) = &proofs.blocks[&number];
        ensure!(
            header.inner.parent_hash == head.hash,
            "canonical lineage changed; restart benchmark from a fresh snapshot"
        );
        engine.ingest_batch(cache, batch.clone())?;
        set_header_context(cache, header);
        if std::env::var_os("FLASHBLOCKS_TRACE_TRANSITIONS").is_some() {
            eprintln!(
                "canonical_advance: block={}, slot0={:?}",
                number,
                cache.snapshot().storage_value(pool, U256::ZERO)
            );
        }
        *head = block_ref(header);
    }
    if head.number != starting_number {
        let refreshed = engine.hydrate_quote_read_sets(cache);
        ensure!(
            refreshed.is_complete(),
            "canonical quote dependencies could not be refreshed: {refreshed:?}"
        );
        proofs.hydrated_accounts += refreshed.accounts_refreshed;
        proofs.hydrated_slots += refreshed.slots_refreshed;
        proofs.hydration_passes += 1;
    }
    Ok(())
}

fn swap_keys<'a>(
    records: impl Iterator<Item = &'a evm_fork_cache::reactive::ReactiveInputRecord<Ethereum>>,
) -> Result<Vec<SwapKey>> {
    let mut keys = Vec::new();
    for record in records {
        let ReactiveInput::Log(log) = &record.input else {
            bail!("swap subscriber emitted a non-log input");
        };
        if log.topic0()
            != Some(&keccak256(
                b"Swap(address,address,int256,int256,uint160,uint128,int24)",
            ))
        {
            continue;
        }
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
    print_stats("canonical_reconcile_and_apply", vanilla_apply);
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
    let transport = Http::with_client(client, url.parse().context("parse benchmark HTTP URL")?);
    Ok(Arc::new(RootProvider::<AnyNetwork>::new(RpcClient::new(
        transport, false,
    ))))
}

fn subscriber_http_provider(url: &str) -> Result<DynProvider<Ethereum>> {
    let client = reqwest::Client::builder()
        .gzip(true)
        .build()
        .context("build subscriber HTTP client")?;
    let transport = Http::with_client(client, url.parse().context("parse subscriber HTTP URL")?);
    Ok(RootProvider::<Ethereum>::new(RpcClient::new(transport, false)).erased())
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

async fn pace_setup(pause: Duration) {
    if !pause.is_zero() {
        tokio::time::sleep(pause).await;
    }
}

#[cfg(test)]
mod canonical_reconciliation_tests {
    use super::*;
    use alloy_rpc_types_eth::{Block, Log};
    use alloy_transport::mock::Asserter;

    async fn fixture() -> Result<(AmmSyncEngine, EvmCache, BlockRef, Header)> {
        let provider = RootProvider::<AnyNetwork>::new(RpcClient::mocked(Asserter::new()));
        let mut cache = EvmCache::new(Arc::new(provider)).await;
        let head = BlockRef {
            number: 100,
            hash: B256::repeat_byte(1),
            parent_hash: Some(B256::ZERO),
            timestamp: Some(1_000),
        };
        cache.set_block(BlockId::from((head.hash, Some(true))));
        cache.set_block_context(Some(head.number), None);
        let mut engine = AmmSyncEngine::new(AdapterRegistry::new())?;
        engine.ingest_batch(
            &mut cache,
            ReactiveInputBatch::new(Vec::new())
                .with_chain_id(1)
                .with_chain_controls([ChainControl::CanonicalProgress(head)]),
        )?;
        let inner = alloy_consensus::Header {
            number: 101,
            parent_hash: head.hash,
            timestamp: 1_012,
            ..Default::default()
        };
        let header = Header {
            hash: inner.hash_slow(),
            inner,
            total_difficulty: None,
            size: None,
        };
        Ok((engine, cache, head, header))
    }

    #[tokio::test]
    async fn empty_hash_reconciled_block_advances_coverage() -> Result<()> {
        let (mut engine, mut cache, mut head, header) = fixture().await?;
        let asserter = Asserter::new();
        asserter.push_success(&Some(Block::<alloy_rpc_types_eth::Transaction>::empty(
            header.clone(),
        )));
        asserter.push_success(&Vec::<Log>::new());
        let provider = RootProvider::<Ethereum>::new(RpcClient::mocked(asserter)).erased();
        reconcile_canonical_path(
            &provider,
            1,
            Address::repeat_byte(2),
            101,
            &mut CanonicalProofs::default(),
            &mut head,
            &mut engine,
            &mut cache,
        )
        .await?;
        assert_eq!(head, block_ref(&header));
        assert_eq!(engine.runtime().last_canonical_block(), Some(head));
        assert_eq!(cache.block_number(), Some(101));
        assert_eq!(cache.timestamp(), Some(1_012));
        Ok(())
    }

    #[tokio::test]
    async fn changed_parent_is_rejected_before_progress() -> Result<()> {
        let (mut engine, mut cache, mut head, mut header) = fixture().await?;
        header.inner.parent_hash = B256::repeat_byte(9);
        header.hash = header.inner.hash_slow();
        let asserter = Asserter::new();
        asserter.push_success(&Some(Block::<alloy_rpc_types_eth::Transaction>::empty(
            header,
        )));
        let provider = RootProvider::<Ethereum>::new(RpcClient::mocked(asserter)).erased();
        assert!(
            reconcile_canonical_path(
                &provider,
                1,
                Address::repeat_byte(2),
                101,
                &mut CanonicalProofs::default(),
                &mut head,
                &mut engine,
                &mut cache
            )
            .await
            .is_err()
        );
        assert_eq!(head.number, 100);
        assert_eq!(engine.runtime().last_canonical_block(), Some(head));
        Ok(())
    }

    #[tokio::test]
    async fn log_from_another_block_is_rejected_before_progress() -> Result<()> {
        let (mut engine, mut cache, mut head, header) = fixture().await?;
        let asserter = Asserter::new();
        asserter.push_success(&Some(Block::<alloy_rpc_types_eth::Transaction>::empty(
            header,
        )));
        asserter.push_success(&Vec::<Log>::from([Log::default()]));
        let provider = RootProvider::<Ethereum>::new(RpcClient::mocked(asserter)).erased();
        assert!(
            reconcile_canonical_path(
                &provider,
                1,
                Address::repeat_byte(2),
                101,
                &mut CanonicalProofs::default(),
                &mut head,
                &mut engine,
                &mut cache
            )
            .await
            .is_err()
        );
        assert_eq!(head.number, 100);
        assert_eq!(engine.runtime().last_canonical_block(), Some(head));
        Ok(())
    }
}
