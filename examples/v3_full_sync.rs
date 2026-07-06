//! One-shot Uniswap V3 pool sync: the entire tick range in a single `eth_call`.
//!
//! Demonstrates the `v3_sync` storage programs end to end against live
//! mainnet state:
//!
//! 1. **Full sync** — a ~360-byte generated EVM program (tick spacing and
//!    storage layout baked in as immediates, **zero calldata**) is injected
//!    over the pool's code via an `eth_call` state override. In-EVM it walks
//!    all 694 tick-bitmap words, loads every initialized tick's four info
//!    words, and returns statics + ticks + the whole observation ring in one
//!    response. The decoded snapshot materializes into thousands of
//!    `(slot, value)` pairs — including explicit zeros for empty bitmap words
//!    — and is injected into a fork cache.
//! 2. **Classic cold start** — the windowed three-round planner, for
//!    contrast.
//! 3. **Quote ladder** — identical `QuoterV2` quotes from both caches, from
//!    a dust swap to a hard multi-tick-crossing size. The program-synced
//!    cache never lazy-fetches a tick; the windowed cache pages far ticks in
//!    over RPC mid-simulation.
//! 4. **Partial sync** — the calldata-driven variant refreshing just the
//!    ±2-word active window (the planner-round shape, collapsed to one call).
//!
//! RPC economics (Alchemy): the full sync is ONE `eth_call` = 26 CU (20 via
//! `eth_callMany`) versus ~134k CU for the same slots as point reads.
//!
//! ```text
//! E2E_RPC_URL=<https endpoint> cargo run --release --example v3_full_sync
//! ```

use std::sync::Arc;
use std::time::Instant;

use alloy_eips::{BlockId, BlockNumberOrTag};
use alloy_primitives::{Address, U256, address};
use alloy_provider::network::AnyNetwork;
use alloy_provider::{Provider, RootProvider};
use alloy_rpc_client::RpcClient;
use alloy_transport_http::Http;
use anyhow::{Context, Result, anyhow};
use evm_amm_state::adapters::storage::V3StorageLayout;
use evm_amm_state::adapters::v3_sync::{
    V3SyncSpec, build_full_sync_program, full_word_range, run_full_sync, run_partial_sync,
};
use evm_amm_state::adapters::{
    AdapterRegistry, AmmAdapter, ColdStartPolicy, ConcentratedLiquidityAdapter, PoolKey,
    PoolRegistration, ProtocolMetadata, SimConfig, V3Metadata,
};
use evm_fork_cache::cache::EvmCache;

const USDC: Address = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
const WETH: Address = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
/// Uniswap V3 USDC/WETH 0.05% (fee 500, tick spacing 10).
const POOL: Address = address!("88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640");

fn registration() -> PoolRegistration {
    PoolRegistration::new(PoolKey::UniswapV3(POOL))
        .with_state_address(POOL)
        .with_metadata(ProtocolMetadata::UniswapV3(
            V3Metadata::default()
                .with_token0(USDC)
                .with_token1(WETH)
                .with_fee(500)
                .with_tick_spacing(10)
                .with_storage_layout(V3StorageLayout::uniswap(10)),
        ))
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let Ok(url) = std::env::var("E2E_RPC_URL") else {
        eprintln!("E2E_RPC_URL unset — skipping v3_full_sync (set an https endpoint to run).");
        return Ok(());
    };

    // Gzip-enabled transport: sync responses are hundreds of KB of nonzero
    // storage words and compress ~3x on the wire.
    let client = reqwest::Client::builder()
        .gzip(true)
        .build()
        .context("build reqwest client")?;
    let http = Http::with_client(client, url.parse().context("parse E2E_RPC_URL")?);
    let provider = Arc::new(RootProvider::<AnyNetwork>::new(RpcClient::new(http, false)));

    let latest = provider.get_block_number().await?;
    let pinned = latest.saturating_sub(8);
    let block = BlockId::Number(BlockNumberOrTag::Number(pinned));
    println!("# One-shot Uniswap V3 full-pool sync\n");
    println!("- pool: USDC/WETH 0.05% ({POOL})");
    println!("- pinned block: {pinned}\n");

    // ------------------------------------------------------------------
    // 1. Full sync: one eth_call for the whole pool.
    // ------------------------------------------------------------------
    let spec = V3SyncSpec::uniswap(V3StorageLayout::uniswap(10));
    let program = build_full_sync_program(&spec);
    let (min_word, max_word) = full_word_range(10);

    let started = Instant::now();
    let snapshot = run_full_sync(provider.as_ref(), block, POOL, &spec)
        .await
        .map_err(|e| anyhow!("full sync failed: {e}"))?;
    let sync_elapsed = started.elapsed();

    let mut synced_cache = EvmCache::at_block(provider.clone(), block).await;
    let injected = snapshot.inject(&mut synced_cache, POOL, &spec);

    println!("## 1. Full sync (one eth_call, zero calldata)\n");
    println!(
        "- program: {} bytes of generated EVM assembly",
        program.len()
    );
    println!(
        "- scanned bitmap words {min_word}..={max_word} in-EVM, returned {} initialized ticks + {} observations",
        snapshot.ticks.len(),
        snapshot.observations.len(),
    );
    println!(
        "- {injected} storage slots materialized + injected (incl. explicit zeros for empty bitmap words)",
    );
    println!(
        "- wall time: {:.0} ms; RPC cost: 1 eth_call = 26 CU (20 via eth_callMany) vs {} CU as point reads ({}x)\n",
        sync_elapsed.as_secs_f64() * 1000.0,
        injected * 20,
        injected * 20 / 26,
    );

    // ------------------------------------------------------------------
    // 2. Classic windowed cold start, for contrast.
    // ------------------------------------------------------------------
    let mut lazy_cache = EvmCache::at_block(provider.clone(), block).await;
    let registry = {
        let mut r = AdapterRegistry::new();
        r.register_adapter(Arc::new(ConcentratedLiquidityAdapter::default()))?;
        r
    };
    let mut lazy_registration = registration();
    let started = Instant::now();
    registry.cold_start(
        &mut lazy_registration,
        &mut lazy_cache,
        ColdStartPolicy::Eager,
    )?;
    println!("## 2. Classic windowed cold start (3 request rounds)\n");
    println!(
        "- slot0 + liquidity, then the ±2-word bitmap window, then its ticks: {:.0} ms",
        started.elapsed().as_secs_f64() * 1000.0,
    );
    println!("- far ticks stay cold — a big swap pages them in over RPC mid-simulation\n");

    // ------------------------------------------------------------------
    // 3. Quote ladder: both caches must agree at every size.
    // ------------------------------------------------------------------
    println!("## 3. QuoterV2 parity ladder (USDC -> WETH)\n");
    println!("| Amount in | Program-synced cache | Windowed cache | Equal |");
    println!("| ---: | ---: | ---: | :-: |");
    let adapter = ConcentratedLiquidityAdapter::default();
    let config = SimConfig::default();
    let synced_registration = registration();
    for usdc in [1u64, 100_000, 5_000_000] {
        let amount_in = U256::from(usdc) * U256::from(1_000_000u64); // 6 decimals
        let started = Instant::now();
        let synced = adapter
            .simulate_swap(
                &synced_registration,
                &mut synced_cache,
                USDC,
                WETH,
                amount_in,
                &config,
            )
            .map_err(|e| anyhow!("synced sim failed: {e}"))?;
        let synced_ms = started.elapsed().as_secs_f64() * 1000.0;

        let started = Instant::now();
        let lazy = adapter
            .simulate_swap(
                &lazy_registration,
                &mut lazy_cache,
                USDC,
                WETH,
                amount_in,
                &config,
            )
            .map_err(|e| anyhow!("lazy sim failed: {e}"))?;
        let lazy_ms = started.elapsed().as_secs_f64() * 1000.0;

        anyhow::ensure!(
            synced.amount_out == lazy.amount_out,
            "quote divergence at {usdc} USDC"
        );
        println!(
            "| {usdc} USDC | {} wei ({synced_ms:.0} ms) | {} wei ({lazy_ms:.0} ms) | yes |",
            synced.amount_out, lazy.amount_out,
        );
    }
    println!();

    // ------------------------------------------------------------------
    // 4. Partial sync: the active window in one calldata-driven call.
    // ------------------------------------------------------------------
    let slot0 = snapshot.statics[0].1;
    let raw_tick = ((slot0 >> 160usize) & U256::from(0xFF_FFFFu64)).to::<u64>() as u32;
    let current_tick = if raw_tick & 0x80_0000 != 0 {
        (raw_tick | 0xFF00_0000) as i32
    } else {
        raw_tick as i32
    };
    let current_word =
        evm_amm_state::adapters::storage::v3_word_position(current_tick, spec.layout.tick_spacing);
    let words: Vec<i16> = (current_word - 2..=current_word + 2).collect();
    let started = Instant::now();
    let window = run_partial_sync(provider.as_ref(), block, POOL, &spec, &words)
        .await
        .map_err(|e| anyhow!("partial sync failed: {e}"))?;
    println!("## 4. Partial sync (±2-word active window)\n");
    println!(
        "- {} ticks across words {:?} in {:.0} ms — the planner's rounds 2+3, collapsed into one call\n",
        window.len(),
        words,
        started.elapsed().as_secs_f64() * 1000.0,
    );

    println!(
        "Done: the whole pool — every tick over the full range, liquidity, fees, and the \
         observation ring — was resident after a single eth_call."
    );
    Ok(())
}
