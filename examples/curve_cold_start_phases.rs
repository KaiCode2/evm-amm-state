//! Curve cold-start phase breakdown: discovery vs verify-only vs `cold_start_many`.
//!
//! Curve's *first* cold start is a discover→verify run — it fetches the pool's
//! Vyper runtime and runs `get_dy` in a local revm over a cold cache, lazily
//! faulting in each SLOAD it touches. That first-discovery cost is what makes a
//! cold Curve boot slower than Uniswap V2/V3, whose hot state is a known slot set
//! (or a tick-bitmap program) hydrated in one bundled `eth_call`.
//!
//! Once a Curve pool's read-set is known, the crate closes that gap two ways —
//! both measured here against the same real pool:
//!
//! - **verify-only `cold_start`**: pre-populate `CurveMetadata.discovered_slots`
//!   and the planner skips discovery, warming exactly those slots in one verify
//!   round;
//! - **`cold_start_many`**: the same known read-set becomes a single bundled
//!   storage program — the identical one-shot path Uniswap V2/V3 take.
//!
//! It also shows the optional `CurveMetadata.code_seed`: attaching the pool's
//! runtime removes the one lazy code fetch a Curve pool otherwise pays on its
//! first `simulate_swap`, making it fully offline after bootstrap like V2/V3.
//!
//! ```text
//! E2E_RPC_URL=<https-mainnet-rpc> cargo run --release --example curve_cold_start_phases
//! ```
//!
//! If `E2E_RPC_URL` is unset, the runner uses `https://ethereum.publicnode.com`
//! so it stays runnable from a clean shell. Results are provider-dependent; use a
//! paid/archive endpoint for stable numbers. `CURVE_PHASES_ITERS` sets iterations.

use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use alloy_eips::{BlockId, BlockNumberOrTag};
use alloy_network::AnyNetwork;
use alloy_primitives::{Address, U256, address};
use alloy_provider::{Provider, RootProvider};
use alloy_rpc_client::RpcClient;
use alloy_transport_http::Http;
use anyhow::{Context, Result};
use evm_amm_state::adapters::{
    AdapterRegistry, ColdStartOutcome, ColdStartPolicy, CurveAdapter, CurveMetadata, CurveVariant,
    PoolKey, PoolRegistration, ProtocolMetadata, supports_one_shot_hydration,
};
use evm_fork_cache::cache::EvmCache;

const DEFAULT_RPC_URL: &str = "https://ethereum.publicnode.com";
const DEFAULT_ITERS: usize = 3;

// Curve Tricrypto2 (USDT/WBTC/WETH), the CryptoSwap v2 pool the showcase warms.
const CURVE_TRICRYPTO2: Address = address!("D51a44d3FaE010294C616388b506AcdA1bfAAE46");
const USDT: Address = address!("dAC17F958D2ee523a2206206994597C13D831ec7");
const WBTC: Address = address!("2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599");
const WETH: Address = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");

type SharedProvider = Arc<RootProvider<AnyNetwork>>;

/// Wall-clock samples for one cold-start path, plus a one-line description.
struct PhaseStats {
    durations: Vec<Duration>,
    details: String,
}

impl PhaseStats {
    fn median_ms(&self) -> f64 {
        let mut durations = self.durations.clone();
        durations.sort_unstable();
        durations[durations.len() / 2].as_secs_f64() * 1000.0
    }

    fn min_ms(&self) -> f64 {
        self.durations
            .iter()
            .min()
            .map(|d| d.as_secs_f64() * 1000.0)
            .unwrap_or_default()
    }

    fn max_ms(&self) -> f64 {
        self.durations
            .iter()
            .max()
            .map(|d| d.as_secs_f64() * 1000.0)
            .unwrap_or_default()
    }
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let url = std::env::var("E2E_RPC_URL").unwrap_or_else(|_| DEFAULT_RPC_URL.to_string());
    let iterations = std::env::var("CURVE_PHASES_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_ITERS);

    let provider = provider(&url)?;
    let latest = provider.get_block_number().await.context("get block")?;
    let pinned = latest.saturating_sub(8);
    let block = BlockId::Number(BlockNumberOrTag::Number(pinned));

    println!("# Curve cold-start phase breakdown\n");
    println!("- rpc: {}", redact_url(&url));
    println!("- pool: Tricrypto2 {CURVE_TRICRYPTO2}");
    println!("- block: {pinned}");
    println!("- iterations: {iterations}\n");

    // First, one discovery run to capture the read-set the fast paths reuse.
    let mut discovered_slots = discover_once(provider.clone(), block).await?;
    discovered_slots.sort_unstable();
    discovered_slots.dedup();
    if discovered_slots.is_empty() {
        println!(
            "Discovery captured no slots (is this an archive node at a recent block?). \
             Cannot measure the fast paths; aborting."
        );
        return Ok(());
    }
    println!(
        "Discovered read-set: {} slots (reused by both fast paths below).",
        discovered_slots.len()
    );
    // The slot KEYS are fixed by the pool's Vyper layout (block-independent), so
    // they can be captured once and persisted. Print them paste-ready for a
    // `CurveMetadata::with_discovered_slots([..])` in a consumer (e.g. a demo).
    println!("Persist these to skip discovery on later boots:");
    for slot in &discovered_slots {
        println!("    U256::from_str_radix(\"{slot:x}\", 16).unwrap(),");
    }
    println!();

    // 1) The slow first boot: discover -> verify (fetches code, faults SLOADs).
    let discovery = measure(iterations, || {
        let provider = provider.clone();
        async move {
            let mut cache = cache(provider, block).await;
            let mut reg = curve_registration(Vec::new(), None);
            let outcome =
                curve_registry().cold_start(&mut reg, &mut cache, ColdStartPolicy::Eager)?;
            ensure_ready(&outcome, "discovery cold_start")?;
            Ok(())
        }
    })
    .await
    .map(|durations| PhaseStats {
        durations,
        details: "discover -> verify: fetch code + fault get_dy read-set".to_string(),
    })?;

    // 2) verify-only cold_start: the read-set is known, so discovery is skipped.
    let verify_only = {
        let slots = discovered_slots.clone();
        measure(iterations, || {
            let provider = provider.clone();
            let slots = slots.clone();
            async move {
                let mut cache = cache(provider, block).await;
                let mut reg = curve_registration(slots, None);
                let outcome =
                    curve_registry().cold_start(&mut reg, &mut cache, ColdStartPolicy::Eager)?;
                ensure_ready(&outcome, "verify-only cold_start")?;
                Ok(())
            }
        })
        .await
        .map(|durations| PhaseStats {
            durations,
            details: "single verify round over the known slots (no discovery)".to_string(),
        })?
    };

    // 3) cold_start_many: the known read-set as one bundled storage program.
    let bundled = {
        let slots = discovered_slots.clone();
        measure(iterations, || {
            let provider = provider.clone();
            let slots = slots.clone();
            async move {
                let mut cache = cache(provider.clone(), block).await;
                let mut pools = vec![curve_registration(slots, None)];
                debug_assert!(
                    supports_one_shot_hydration(&pools[0]),
                    "a known-read-set Curve pool must be one-shot eligible"
                );
                let outcomes = curve_registry()
                    .cold_start_many(&mut pools, &mut cache, provider.as_ref(), ColdStartPolicy::Eager)
                    .await?;
                ensure_ready(&outcomes[0], "cold_start_many")?;
                Ok(())
            }
        })
        .await
        .map(|durations| PhaseStats {
            durations,
            details: "one bundled storage program (the V2/V3 fast path)".to_string(),
        })?
    };

    print_row("discovery cold_start (cold first boot)", &discovery);
    print_row("verify-only cold_start (known read-set)", &verify_only);
    print_row("cold_start_many (known read-set)", &bundled);

    let base = discovery.median_ms();
    println!(
        "\nverify-only is {:.1}x faster than first-discovery; cold_start_many is {:.1}x faster.",
        base / verify_only.median_ms().max(f64::MIN_POSITIVE),
        base / bundled.median_ms().max(f64::MIN_POSITIVE),
    );

    // Optional bytecode seed: fetch the pool runtime and show it verifies once,
    // so a later first quote needs no lazy code fetch (fully offline like V2/V3).
    let code = provider
        .get_code_at(CURVE_TRICRYPTO2)
        .block_id(block)
        .await
        .context("eth_getCode for Tricrypto2")?;
    let mut cache = cache(provider.clone(), block).await;
    let mut seeded = curve_registration(discovered_slots.clone(), Some(code.clone()));
    let outcome = curve_registry().cold_start(&mut seeded, &mut cache, ColdStartPolicy::Eager)?;
    let verified = match &outcome {
        ColdStartOutcome::Ready(report) => report
            .code_seeds
            .as_ref()
            .map(|seeds| seeds.verified.len())
            .unwrap_or(0),
        _ => 0,
    };
    println!(
        "\nBytecode seed: attached {} bytes of pool runtime; cold-start verified {} seed(s) \
         against on-chain code. With the seed, the first simulate_swap needs no lazy code fetch.",
        code.len(),
        verified,
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

/// Run `iterations` timed passes of `f`, each a fresh cold start.
async fn measure<F, Fut>(iterations: usize, mut f: F) -> Result<Vec<Duration>>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<()>>,
{
    let mut samples = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let start = Instant::now();
        f().await?;
        samples.push(start.elapsed());
    }
    Ok(samples)
}

/// One discovery cold start, returning the captured `get_dy` read-set.
async fn discover_once(provider: SharedProvider, block: BlockId) -> Result<Vec<U256>> {
    let mut cache = cache(provider, block).await;
    let mut reg = curve_registration(Vec::new(), None);
    let outcome = curve_registry().cold_start(&mut reg, &mut cache, ColdStartPolicy::Eager)?;
    ensure_ready(&outcome, "discovery cold_start")?;
    Ok(match &reg.metadata {
        ProtocolMetadata::Curve(m) => m.discovered_slots.clone(),
        _ => Vec::new(),
    })
}

fn curve_registration(discovered_slots: Vec<U256>, code_seed: Option<alloy_primitives::Bytes>) -> PoolRegistration {
    let mut metadata = CurveMetadata::default()
        .with_coins(vec![USDT, WBTC, WETH])
        .with_discovered_slots(discovered_slots)
        .with_variant(CurveVariant::CryptoSwap);
    if let Some(code) = code_seed {
        metadata = metadata.with_code_seed(code);
    }
    PoolRegistration::new(PoolKey::Curve(CURVE_TRICRYPTO2))
        .with_state_address(CURVE_TRICRYPTO2)
        .with_metadata(ProtocolMetadata::Curve(metadata))
}

fn curve_registry() -> AdapterRegistry {
    let mut registry = AdapterRegistry::new();
    registry
        .register_adapter(Arc::new(CurveAdapter::default()))
        .expect("register curve adapter");
    registry
}

fn ensure_ready(outcome: &ColdStartOutcome, label: &str) -> Result<()> {
    match outcome {
        ColdStartOutcome::Ready(_) | ColdStartOutcome::ReadyWithDeferred(_, _) => Ok(()),
        other => Err(anyhow::anyhow!("{label} did not reach Ready: {other:?}")),
    }
}

fn print_row(name: &str, stats: &PhaseStats) {
    println!(
        "- {name:<44} {:>7.1} ms (min..max {:.1}..{:.1})  [{}]",
        stats.median_ms(),
        stats.min_ms(),
        stats.max_ms(),
        stats.details,
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
