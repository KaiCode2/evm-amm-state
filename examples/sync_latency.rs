//! Live, network-bound sync latency comparison for AMM state loading.
//!
//! ```text
//! E2E_RPC_URL=<https-mainnet-rpc> cargo run --release --example sync_latency
//! ```
//!
//! If `E2E_RPC_URL` is unset, the runner uses `https://ethereum.publicnode.com`
//! so the benchmark remains runnable from a clean shell. Results are highly
//! provider-dependent; use a paid/archive endpoint for stable numbers.

use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use alloy_eips::{BlockId, BlockNumberOrTag};
use alloy_network::AnyNetwork;
use alloy_primitives::{Address, B256, address, b256};
use alloy_provider::{Provider, RootProvider};
use alloy_rpc_client::RpcClient;
use alloy_transport_http::Http;
use anyhow::{Context, Result};
use evm_amm_state::adapters::storage::V3StorageLayout;
use evm_amm_state::adapters::v3_sync::{V3SyncSpec, run_full_sync};
use evm_amm_state::adapters::{
    AdapterRegistry, BalancerV2Adapter, BalancerV2Metadata, ColdStartOutcome, ColdStartPolicy,
    ConcentratedLiquidityAdapter, CurveAdapter, CurveMetadata, CurveVariant, PoolKey,
    PoolRegistration, ProtocolMetadata, UniswapV2Adapter, UniswapV2Metadata, V3Metadata,
    run_storage_sync, storage_sync_spec_for_pool,
};
use evm_fork_cache::cache::EvmCache;

const DEFAULT_RPC_URL: &str = "https://ethereum.publicnode.com";
const DEFAULT_ITERS: usize = 3;

const USDC: Address = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
const WETH: Address = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");

const V3_USDC_WETH_005: Address = address!("88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640");

const V2_USDC_WETH_PAIR: Address = address!("B4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc");

const BALANCER_VAULT: Address = address!("BA12222222228d8Ba445958a75a0704d566BF2C8");
const BALANCER_BAL_WETH_POOL_ID: B256 =
    b256!("5c6ee304399dbdb9c8ef030ab642b10820db8f56000200000000000000000014");

const CURVE_3POOL: Address = address!("bEbc44782C7dB0a1A60Cb6fe97d0b483032FF1C7");
const DAI: Address = address!("6B175474E89094C44Da98b954EedeAC495271d0F");
const USDT: Address = address!("dAC17F958D2ee523a2206206994597C13D831ec7");

type SharedProvider = Arc<RootProvider<AnyNetwork>>;

#[derive(Clone, Debug)]
struct SyncStats {
    durations: Vec<Duration>,
    slots: usize,
    details: String,
}

impl SyncStats {
    fn median_ms(&self) -> f64 {
        let mut durations = self.durations.clone();
        durations.sort_unstable();
        durations[durations.len() / 2].as_secs_f64() * 1000.0
    }

    fn min_ms(&self) -> f64 {
        self.durations
            .iter()
            .min()
            .map(|duration| duration.as_secs_f64() * 1000.0)
            .unwrap_or_default()
    }

    fn max_ms(&self) -> f64 {
        self.durations
            .iter()
            .max()
            .map(|duration| duration.as_secs_f64() * 1000.0)
            .unwrap_or_default()
    }
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let url = std::env::var("E2E_RPC_URL").unwrap_or_else(|_| DEFAULT_RPC_URL.to_string());
    let iterations = std::env::var("SYNC_BENCH_ITERS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_ITERS);

    let provider = provider(&url)?;
    let latest = provider.get_block_number().await.context("get block")?;
    let pinned = latest.saturating_sub(8);
    let block = BlockId::Number(BlockNumberOrTag::Number(pinned));

    println!("# AMM sync latency benchmark\n");
    println!("- rpc: {}", redact_url(&url));
    println!("- block: {pinned}");
    println!("- iterations: {iterations}\n");

    let v3_prior = measure_v3_prior(provider.clone(), block, iterations).await?;
    let v3_new = measure_v3_full(provider.clone(), block, iterations).await?;
    print_pair("Uniswap V3 USDC/WETH 0.05%", &v3_prior, &v3_new);

    let v2_prior = measure_v2_prior(provider.clone(), block, iterations).await?;
    let v2_new = measure_v2_storage_sync(provider.clone(), block, iterations).await?;
    print_pair("Uniswap V2 USDC/WETH", &v2_prior, &v2_new);

    let balancer_prior = measure_balancer_prior(provider.clone(), block, iterations).await?;
    let balancer_template = discover_balancer(provider.clone(), block).await?;
    let balancer_new = measure_storage_sync_from_registration(
        provider.clone(),
        block,
        &balancer_template,
        iterations,
    )
    .await?;
    print_pair("Balancer V2 80BAL/20WETH", &balancer_prior, &balancer_new);

    let curve_prior = measure_curve_prior(provider.clone(), block, iterations).await?;
    let curve_template = discover_curve(provider.clone(), block).await?;
    let curve_new = measure_storage_sync_from_registration(
        provider.clone(),
        block,
        &curve_template,
        iterations,
    )
    .await?;
    print_pair("Curve 3pool StableSwap", &curve_prior, &curve_new);

    println!(
        "\nNote: Balancer/Curve new-path timings are refreshes after their read-set metadata exists. \
         The current prior path includes discover -> verify; a future trace-based loader can populate \
         those read-sets without the view-call discover round."
    );

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

async fn cache(provider: SharedProvider, block: BlockId) -> EvmCache {
    EvmCache::at_block(provider, block).await
}

async fn measure<F, Fut>(iterations: usize, mut f: F) -> Result<Vec<(Duration, usize, String)>>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<(usize, String)>>,
{
    let mut samples = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let start = Instant::now();
        let (slots, details) = f().await?;
        samples.push((start.elapsed(), slots, details));
    }
    Ok(samples)
}

fn stats(samples: Vec<(Duration, usize, String)>) -> SyncStats {
    let slots = samples
        .last()
        .map(|(_, slots, _)| *slots)
        .unwrap_or_default();
    let details = samples
        .last()
        .map(|(_, _, details)| details.clone())
        .unwrap_or_default();
    SyncStats {
        durations: samples
            .into_iter()
            .map(|(duration, _, _)| duration)
            .collect(),
        slots,
        details,
    }
}

async fn measure_v3_prior(
    provider: SharedProvider,
    block: BlockId,
    iterations: usize,
) -> Result<SyncStats> {
    let samples = measure(iterations, || {
        let provider = provider.clone();
        async move {
            let mut cache = cache(provider, block).await;
            let mut registration = v3_registration();
            let outcome =
                v3_registry().cold_start(&mut registration, &mut cache, ColdStartPolicy::Eager)?;
            Ok((
                outcome_slots(&outcome),
                "prior cold_start: active tick window only, not full-pool".to_string(),
            ))
        }
    })
    .await?;
    Ok(stats(samples))
}

async fn measure_v3_full(
    provider: SharedProvider,
    block: BlockId,
    iterations: usize,
) -> Result<SyncStats> {
    let spec = V3SyncSpec::uniswap(V3StorageLayout::uniswap(10));
    let samples = measure(iterations, || {
        let provider = provider.clone();
        let spec = spec.clone();
        async move {
            let mut cache = cache(provider.clone(), block).await;
            let snapshot = run_full_sync(provider.as_ref(), block, V3_USDC_WETH_005, &spec).await?;
            let slots = snapshot.inject(&mut cache, V3_USDC_WETH_005, &spec);
            Ok((
                slots,
                format!(
                    "full-pool: {} initialized ticks, {} observations",
                    snapshot.ticks.len(),
                    snapshot.observations.len()
                ),
            ))
        }
    })
    .await?;
    Ok(stats(samples))
}

async fn measure_v2_prior(
    provider: SharedProvider,
    block: BlockId,
    iterations: usize,
) -> Result<SyncStats> {
    let samples = measure(iterations, || {
        let provider = provider.clone();
        async move {
            let mut cache = cache(provider, block).await;
            let mut registration = v2_registration();
            let outcome =
                v2_registry().cold_start(&mut registration, &mut cache, ColdStartPolicy::Eager)?;
            Ok((
                outcome_slots(&outcome),
                "prior cold_start verify slots".to_string(),
            ))
        }
    })
    .await?;
    Ok(stats(samples))
}

async fn measure_v2_storage_sync(
    provider: SharedProvider,
    block: BlockId,
    iterations: usize,
) -> Result<SyncStats> {
    let registration = v2_registration();
    measure_storage_sync_from_registration(provider, block, &registration, iterations).await
}

async fn measure_balancer_prior(
    provider: SharedProvider,
    block: BlockId,
    iterations: usize,
) -> Result<SyncStats> {
    let samples = measure(iterations, || {
        let provider = provider.clone();
        async move {
            let mut cache = cache(provider, block).await;
            let mut registration = balancer_registration();
            let outcome = balancer_registry().cold_start(
                &mut registration,
                &mut cache,
                ColdStartPolicy::Eager,
            )?;
            Ok((
                outcome_slots(&outcome),
                "prior cold_start discover -> verify".to_string(),
            ))
        }
    })
    .await?;
    Ok(stats(samples))
}

async fn discover_balancer(provider: SharedProvider, block: BlockId) -> Result<PoolRegistration> {
    let mut cache = cache(provider, block).await;
    let mut registration = balancer_registration();
    balancer_registry().cold_start(&mut registration, &mut cache, ColdStartPolicy::Eager)?;
    Ok(registration)
}

async fn measure_curve_prior(
    provider: SharedProvider,
    block: BlockId,
    iterations: usize,
) -> Result<SyncStats> {
    let samples = measure(iterations, || {
        let provider = provider.clone();
        async move {
            let mut cache = cache(provider, block).await;
            let mut registration = curve_registration();
            let outcome = curve_registry().cold_start(
                &mut registration,
                &mut cache,
                ColdStartPolicy::Eager,
            )?;
            Ok((
                outcome_slots(&outcome),
                "prior cold_start discover -> verify".to_string(),
            ))
        }
    })
    .await?;
    Ok(stats(samples))
}

async fn discover_curve(provider: SharedProvider, block: BlockId) -> Result<PoolRegistration> {
    let mut cache = cache(provider, block).await;
    let mut registration = curve_registration();
    curve_registry().cold_start(&mut registration, &mut cache, ColdStartPolicy::Eager)?;
    Ok(registration)
}

async fn measure_storage_sync_from_registration(
    provider: SharedProvider,
    block: BlockId,
    registration: &PoolRegistration,
    iterations: usize,
) -> Result<SyncStats> {
    let spec = storage_sync_spec_for_pool(registration)?;
    let slots = spec.slots.len();
    let samples = measure(iterations, || {
        let provider = provider.clone();
        let spec = spec.clone();
        async move {
            let mut cache = cache(provider.clone(), block).await;
            let snapshot = run_storage_sync(provider.as_ref(), block, &spec).await?;
            Ok((
                snapshot.inject(&mut cache),
                format!("storage program over {slots} known slots"),
            ))
        }
    })
    .await?;
    Ok(stats(samples))
}

fn v2_registration() -> PoolRegistration {
    PoolRegistration::new(PoolKey::UniswapV2(V2_USDC_WETH_PAIR))
        .with_state_address(V2_USDC_WETH_PAIR)
        .with_metadata(ProtocolMetadata::UniswapV2(
            UniswapV2Metadata::default()
                .with_token0(USDC)
                .with_token1(WETH)
                .with_fee_bps(30),
        ))
}

fn v3_registration() -> PoolRegistration {
    PoolRegistration::new(PoolKey::UniswapV3(V3_USDC_WETH_005))
        .with_state_address(V3_USDC_WETH_005)
        .with_metadata(ProtocolMetadata::UniswapV3(
            V3Metadata::default()
                .with_token0(USDC)
                .with_token1(WETH)
                .with_fee(500)
                .with_tick_spacing(10)
                .with_storage_layout(V3StorageLayout::uniswap(10)),
        ))
}

fn balancer_registration() -> PoolRegistration {
    PoolRegistration::new(PoolKey::BalancerV2(BALANCER_BAL_WETH_POOL_ID))
        .with_state_address(BALANCER_VAULT)
        .with_metadata(ProtocolMetadata::BalancerV2(
            BalancerV2Metadata::default().with_vault(BALANCER_VAULT),
        ))
}

fn curve_registration() -> PoolRegistration {
    PoolRegistration::new(PoolKey::Curve(CURVE_3POOL))
        .with_state_address(CURVE_3POOL)
        .with_metadata(ProtocolMetadata::Curve(
            CurveMetadata::default()
                .with_coins(vec![DAI, USDC, USDT])
                .with_variant(CurveVariant::StableSwap),
        ))
}

fn v2_registry() -> AdapterRegistry {
    let mut registry = AdapterRegistry::new();
    registry
        .register_adapter(Arc::new(UniswapV2Adapter::default()))
        .expect("register v2 adapter");
    registry
}

fn v3_registry() -> AdapterRegistry {
    let mut registry = AdapterRegistry::new();
    registry
        .register_adapter(Arc::new(ConcentratedLiquidityAdapter::default()))
        .expect("register v3 adapter");
    registry
}

fn balancer_registry() -> AdapterRegistry {
    let mut registry = AdapterRegistry::new();
    registry
        .register_adapter(Arc::new(BalancerV2Adapter::default()))
        .expect("register balancer adapter");
    registry
}

fn curve_registry() -> AdapterRegistry {
    let mut registry = AdapterRegistry::new();
    registry
        .register_adapter(Arc::new(CurveAdapter::default()))
        .expect("register curve adapter");
    registry
}

fn outcome_slots(outcome: &ColdStartOutcome) -> usize {
    match outcome {
        ColdStartOutcome::Ready(report)
        | ColdStartOutcome::ReadyWithDeferred(report, _)
        | ColdStartOutcome::NeedsRepair(report, _) => report.verified_slots.len(),
        ColdStartOutcome::Unsupported(_) => 0,
        _ => 0,
    }
}

fn print_pair(name: &str, prior: &SyncStats, new: &SyncStats) {
    let prior_ms = prior.median_ms();
    let new_ms = new.median_ms();
    let speedup = prior_ms / new_ms;
    let reduction = (1.0 - (new_ms / prior_ms)) * 100.0;
    println!("## {name}");
    println!("| Path | Median | Min..max | Slots | Notes |\n| --- | ---: | ---: | ---: | --- |");
    println!(
        "| Prior | {:.1} ms | {:.1}..{:.1} ms | {} | {} |",
        prior_ms,
        prior.min_ms(),
        prior.max_ms(),
        prior.slots,
        prior.details
    );
    println!(
        "| New | {:.1} ms | {:.1}..{:.1} ms | {} | {} |",
        new_ms,
        new.min_ms(),
        new.max_ms(),
        new.slots,
        new.details
    );
    println!(
        "| Relative | {:.2}x | {:.1}% lower latency | | |\n",
        speedup, reduction
    );
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
