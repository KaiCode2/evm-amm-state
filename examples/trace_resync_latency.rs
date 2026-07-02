//! Live latency probe for event-time trace-backed AMM resync.
//!
//! This example finds a recent Curve 3pool `TokenExchange`, cold-starts the pool
//! at the previous block, then ingests the historical log through
//! [`AmmSyncEngine`]. It compares:
//!
//! - trace-only resync: storage fallback returns errors, so success means the
//!   block trace supplied every changed requested slot;
//! - storage fallback resync: trace is disabled, so the configured storage batch
//!   fetcher refreshes the known read-set at the event block.
//!
//! Reproduce:
//!
//! ```text
//! E2E_RPC_URL=<https-mainnet-rpc> TRACE_RESYNC_ITERS=3 \
//!   cargo run --release --example trace_resync_latency
//! ```

use std::env;
use std::sync::Arc;
use std::time::{Duration, Instant};

use alloy_eips::{BlockId, BlockNumberOrTag};
use alloy_network::{AnyNetwork, Ethereum};
use alloy_primitives::{Address, B256, address, keccak256};
use alloy_provider::{Provider, RootProvider};
use alloy_rpc_client::RpcClient;
use alloy_rpc_types_eth::{Filter, Log as RpcLog};
use alloy_transport_http::Http;
use anyhow::{Context, Result, bail};
use evm_amm_state::adapters::{
    AdapterRegistry, AmmAdapter, AmmSyncEngine, ColdStartOutcome, ColdStartPolicy, CurveAdapter,
    CurveMetadata, CurveVariant, PoolKey, PoolRegistration, ProtocolMetadata,
};
use evm_fork_cache::StorageFetchError;
use evm_fork_cache::cache::EvmCache;
use evm_fork_cache::reactive::{
    BlockRef, ChainStatus, InputSource, ReactiveContext, ReactiveInput, ReactiveInputBatch,
    ReactiveInputRecord,
};

type SharedProvider = Arc<RootProvider<AnyNetwork>>;

const CURVE_3POOL: Address = address!("bEbc44782C7dB0a1A60Cb6fe97d0b483032FF1C7");
const DAI: Address = address!("6B175474E89094C44Da98b954EedeAC495271d0F");
const USDC: Address = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
const USDT: Address = address!("dAC17F958D2ee523a2206206994597C13D831ec7");

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let Ok(url) = env::var("E2E_RPC_URL") else {
        println!(
            "Set E2E_RPC_URL to an Ethereum endpoint with eth_getLogs and debug_traceBlockByHash support."
        );
        return Ok(());
    };
    let iterations = env::var("TRACE_RESYNC_ITERS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(3)
        .max(1);
    let provider = provider(&url)?;
    let latest = provider.get_block_number().await?;
    let lookback = env::var("TRACE_RESYNC_LOOKBACK")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(512)
        .max(1);
    let to = latest.saturating_sub(8);
    let from = to.saturating_sub(lookback);

    println!(
        "trace_resync_latency: endpoint={url}, search_blocks={from}..={to}, iterations={iterations}"
    );

    let Some(log) = find_recent_curve_exchange(provider.clone(), from, to).await? else {
        println!("No Curve 3pool TokenExchange found in range; widen the range or retry later.");
        return Ok(());
    };
    let block = log.block_number.context("log missing block number")?;
    if block == 0 {
        println!("Skipping genesis block log.");
        return Ok(());
    }
    println!(
        "Using Curve 3pool event at block {block}, tx_index={:?}, log_index={:?}",
        log.transaction_index, log.log_index
    );

    let previous_block = BlockId::number(block - 1);
    let (mut trace_cache, mut trace_engine, slots) =
        prepare_curve_engine(provider.clone(), previous_block).await?;
    trace_cache.set_storage_batch_fetcher(Arc::new(|requests, _block| {
        requests
            .into_iter()
            .map(|(address, slot)| {
                (
                    address,
                    slot,
                    Err(StorageFetchError::custom(
                        "trace-only mode disabled storage fallback",
                    )),
                )
            })
            .collect()
    }));

    let (mut storage_cache, mut storage_engine, _) =
        prepare_curve_engine(provider.clone(), previous_block).await?;
    storage_cache.set_block_state_diff_fetcher(Arc::new(|_block| {
        Err(StorageFetchError::custom(
            "trace disabled to force storage fallback",
        ))
    }));

    let trace_samples = measure_resync(iterations, &mut trace_cache, &mut trace_engine, &log)?;
    let storage_samples =
        measure_resync(iterations, &mut storage_cache, &mut storage_engine, &log)?;

    print_stats("trace-only", slots, &trace_samples);
    print_stats("storage-fallback", slots, &storage_samples);
    if median_duration(&trace_samples) > Duration::ZERO
        && median_duration(&storage_samples) > Duration::ZERO
    {
        let trace_ms = median_duration(&trace_samples).as_secs_f64() * 1_000.0;
        let storage_ms = median_duration(&storage_samples).as_secs_f64() * 1_000.0;
        println!(
            "relative median: trace/storage = {:.2}x ({:.1} ms / {:.1} ms)",
            trace_ms / storage_ms,
            trace_ms,
            storage_ms
        );
    }

    Ok(())
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

async fn find_recent_curve_exchange(
    provider: SharedProvider,
    from: u64,
    to: u64,
) -> Result<Option<RpcLog>> {
    let filter = Filter::new()
        .address(CURVE_3POOL)
        .event_signature(curve_token_exchange_topic())
        .from_block(BlockNumberOrTag::Number(from))
        .to_block(BlockNumberOrTag::Number(to));
    let logs = provider
        .get_logs(&filter)
        .await
        .context("get Curve 3pool TokenExchange logs")?;
    Ok(logs.into_iter().last())
}

async fn prepare_curve_engine(
    provider: SharedProvider,
    block: BlockId,
) -> Result<(EvmCache, AmmSyncEngine, usize)> {
    let mut cache = EvmCache::builder(provider).block(block).build().await;
    let mut cold = AdapterRegistry::new();
    cold.register_adapter(Arc::new(CurveAdapter::default()))?;

    let mut registration = PoolRegistration::new(PoolKey::Curve(CURVE_3POOL))
        .with_state_address(CURVE_3POOL)
        .with_metadata(ProtocolMetadata::Curve(
            CurveMetadata::default()
                .with_coins([DAI, USDC, USDT])
                .with_variant(CurveVariant::StableSwap),
        ));
    let outcome = cold.cold_start(&mut registration, &mut cache, ColdStartPolicy::Eager)?;
    if !matches!(outcome, ColdStartOutcome::Ready(_)) {
        bail!("Curve cold-start did not reach Ready at {block:?}: {outcome:?}");
    }

    let discovered_slots = match &registration.metadata {
        ProtocolMetadata::Curve(metadata) => metadata.discovered_slots.len(),
        _ => 0,
    };
    if discovered_slots == 0 {
        bail!("Curve cold-start reached Ready but discovered zero slots at {block:?}");
    }

    let adapter = CurveAdapter::default();
    let sources = adapter.event_sources(&registration);
    let mut registry = AdapterRegistry::new();
    registry.register_adapter(Arc::new(CurveAdapter::default()))?;
    registry.register_pool(registration.with_event_sources(sources))?;

    Ok((cache, AmmSyncEngine::new(registry)?, discovered_slots))
}

fn run_resync(
    cache: &mut EvmCache,
    engine: &mut AmmSyncEngine,
    log: RpcLog,
) -> Result<(Duration, usize, usize)> {
    let start = Instant::now();
    let report = engine.ingest_batch(cache, batch_for_log(log)?)?;
    Ok((
        start.elapsed(),
        report.resync_state_updates,
        report.resync_failures,
    ))
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
        chain_id: Some(1),
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

fn curve_token_exchange_topic() -> B256 {
    keccak256("TokenExchange(address,int128,uint256,int128,uint256)")
}

fn measure_resync(
    iterations: usize,
    cache: &mut EvmCache,
    engine: &mut AmmSyncEngine,
    log: &RpcLog,
) -> Result<Vec<(Duration, usize, usize)>> {
    let mut samples = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        samples.push(run_resync(cache, engine, log.clone())?);
    }
    Ok(samples)
}

fn print_stats(label: &str, slots: usize, samples: &[(Duration, usize, usize)]) {
    let durations: Vec<_> = samples.iter().map(|(duration, _, _)| *duration).collect();
    let updates = samples.last().map(|(_, updates, _)| *updates).unwrap_or(0);
    let failures = samples
        .last()
        .map(|(_, _, failures)| *failures)
        .unwrap_or(0);
    println!(
        "{label}: median={:.1} ms, min={:.1} ms, max={:.1} ms, slots={slots}, \
         resync_updates={updates}, failures={failures}",
        median_duration(samples).as_secs_f64() * 1_000.0,
        durations
            .iter()
            .min()
            .copied()
            .unwrap_or_default()
            .as_secs_f64()
            * 1_000.0,
        durations
            .iter()
            .max()
            .copied()
            .unwrap_or_default()
            .as_secs_f64()
            * 1_000.0,
    );
}

fn median_duration(samples: &[(Duration, usize, usize)]) -> Duration {
    let mut durations: Vec<_> = samples.iter().map(|(duration, _, _)| *duration).collect();
    durations.sort_unstable();
    durations
        .get(durations.len() / 2)
        .copied()
        .unwrap_or_default()
}
