//! End-to-end **cross-DEX arbitrage** demo.
//!
//! This is the canonical use case for `evm-amm-state`: hold several pools that
//! trade the same pair across different DEXes in one warmed cache, then quote
//! across all of them *offline* to find a price dislocation — and simulate the
//! round-trip that would capture it, all without a single RPC call in the hot
//! loop.
//!
//! What it does:
//!   1. Registers three USDC/WETH venues across **three protocols** — Uniswap V2,
//!      Uniswap V3, and Curve (the tricryptoUSDC NG pool, which holds USDC+WETH).
//!   2. Cold-starts each into a single shared [`EvmCache`] forked at a pinned
//!      block (one-time RPC warm-up).
//!   3. For a fixed USDC input, quotes USDC->WETH on every venue and picks the
//!      one that returns the **most WETH** (best place to buy).
//!   4. Takes that WETH and quotes WETH->USDC on every venue, picking the one
//!      that returns the **most USDC** (best place to sell).
//!   5. Reports the round-trip P&L. Each quote is the pool's own on-chain quote
//!      entrypoint run in revm — no reimplemented AMM math.
//!
//! At a historical block the market is near-efficient, so the round trip is
//! typically a small **loss** (you pay each pool's fee twice) — that is the
//! honest, expected result. The value here is the *machinery*: detecting the
//! best venues and pricing the execution exactly. A real searcher fires only
//! when the spread exceeds fees + gas.
//!
//! Run (env-gated; needs an archive RPC to warm state):
//! ```text
//! E2E_RPC_URL=<archive-url> cargo run --example arbitrage_cross_dex
//! ```

use std::sync::Arc;

use alloy_eips::{BlockId, BlockNumberOrTag};
use alloy_network::AnyNetwork;
use alloy_primitives::{Address, U256, address};
use alloy_provider::RootProvider;
use anyhow::{Result, anyhow};

use evm_amm_state::adapters::storage::V3StorageLayout;
use evm_amm_state::adapters::{
    AdapterRegistry, ColdStartPolicy, ConcentratedLiquidityAdapter, CurveAdapter, CurveMetadata,
    CurveVariant, PoolKey, PoolRegistration, ProtocolMetadata, SimConfig, UniswapV2Adapter,
    UniswapV2Metadata, V3Metadata,
};
use evm_fork_cache::cache::EvmCache;

const FORK_BLOCK: u64 = 20_000_000;

const USDC: Address = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
const WETH: Address = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
const WBTC: Address = address!("2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599");

const V3_QUOTER_V2: Address = address!("61fFE014bA17989E743c5F6cB21bF9697530B21e");
const V3_USDC_WETH_005: Address = address!("88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640");
const V2_ROUTER_02: Address = address!("7a250d5630B4cF539739dF2C5dAcb4c659F2488D");
const V2_USDC_WETH_PAIR: Address = address!("B4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc");
// tricryptoUSDC (Tricrypto-NG): coins USDC / WBTC / WETH.
const TRICRYPTO_USDC_NG: Address = address!("7F86Bf177Dd4F3494b841a37e810A34dD56c829B");

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let Ok(url) = std::env::var("E2E_RPC_URL") else {
        eprintln!(
            "E2E_RPC_URL unset — this example warms real pools from an archive node.\n\
             Run with: E2E_RPC_URL=<archive-url> cargo run --example arbitrage_cross_dex"
        );
        return Ok(());
    };

    // --- 1. Register adapters for every protocol we will touch. ---
    let mut registry = AdapterRegistry::new();
    registry.register_adapter(Arc::new(UniswapV2Adapter::default()))?;
    registry.register_adapter(Arc::new(ConcentratedLiquidityAdapter::default()))?;
    registry.register_adapter(Arc::new(CurveAdapter::default()))?;

    // --- 2. One shared cache, forked at a pinned block. ---
    let provider = RootProvider::<AnyNetwork>::connect(&url).await?;
    let mut cache = EvmCache::at_block(
        Arc::new(provider),
        BlockId::Number(BlockNumberOrTag::Number(FORK_BLOCK)),
    )
    .await;

    // --- 3. Describe the three USDC/WETH venues and cold-start each. ---
    let venues: Vec<(&str, PoolRegistration)> = vec![
        (
            "Uniswap V2",
            PoolRegistration::new(PoolKey::UniswapV2(V2_USDC_WETH_PAIR))
                .with_state_address(V2_USDC_WETH_PAIR)
                .with_metadata(ProtocolMetadata::UniswapV2(
                    UniswapV2Metadata::default()
                        .with_token0(USDC)
                        .with_token1(WETH)
                        .with_fee_bps(30),
                )),
        ),
        (
            "Uniswap V3 0.05%",
            PoolRegistration::new(PoolKey::UniswapV3(V3_USDC_WETH_005))
                .with_state_address(V3_USDC_WETH_005)
                .with_metadata(ProtocolMetadata::UniswapV3(
                    V3Metadata::default()
                        .with_token0(USDC)
                        .with_token1(WETH)
                        .with_fee(500)
                        .with_tick_spacing(10)
                        .with_storage_layout(V3StorageLayout::uniswap(10)),
                )),
        ),
        (
            "Curve tricryptoUSDC",
            PoolRegistration::new(PoolKey::Curve(TRICRYPTO_USDC_NG))
                .with_state_address(TRICRYPTO_USDC_NG)
                .with_metadata(ProtocolMetadata::Curve(
                    CurveMetadata::default()
                        .with_coins(vec![USDC, WBTC, WETH])
                        .with_discovered_slots(Vec::new())
                        .with_variant(CurveVariant::CryptoSwapNG),
                )),
        ),
    ];

    let mut keys: Vec<(&str, PoolKey)> = Vec::new();
    for (name, mut reg) in venues {
        registry.cold_start(&mut reg, &mut cache, ColdStartPolicy::Eager)?;
        keys.push((name, reg.key.clone()));
        registry.register_pool(reg)?;
    }
    println!(
        "Cold-started {} venues at block {FORK_BLOCK}.\n",
        keys.len()
    );

    let cfg = SimConfig::default()
        .with_v3_quoter(V3_QUOTER_V2)
        .with_v2_router(V2_ROUTER_02);

    // --- 4. Leg 1: quote USDC -> WETH on every venue; pick the best (most WETH). ---
    let usdc_in = U256::from(100_000_000_000_u64); // 100,000 USDC (6 decimals)
    println!("Leg 1 — sell {} USDC for WETH:", fmt(usdc_in, 6));
    let mut best_buy: Option<(&str, &PoolKey, U256)> = None;
    for (name, key) in &keys {
        match quote(&registry, &mut cache, key, USDC, WETH, usdc_in, &cfg) {
            Some(out) => {
                println!("  {name:<22} -> {} WETH", fmt(out, 18));
                if best_buy.is_none_or(|(_, _, b)| out > b) {
                    best_buy = Some((*name, key, out));
                }
            }
            None => println!("  {name:<22} -> (no quote)"),
        }
    }
    let (buy_name, _buy_key, weth_out) =
        best_buy.ok_or_else(|| anyhow!("no venue quoted leg 1"))?;
    println!("  => best buy: {buy_name} ({} WETH)\n", fmt(weth_out, 18));

    // --- 5. Leg 2: quote that WETH -> USDC on every venue; pick the best (most USDC). ---
    println!("Leg 2 — sell {} WETH back to USDC:", fmt(weth_out, 18));
    let mut best_sell: Option<(&str, &PoolKey, U256)> = None;
    for (name, key) in &keys {
        match quote(&registry, &mut cache, key, WETH, USDC, weth_out, &cfg) {
            Some(out) => {
                println!("  {name:<22} -> {} USDC", fmt(out, 6));
                if best_sell.is_none_or(|(_, _, b)| out > b) {
                    best_sell = Some((*name, key, out));
                }
            }
            None => println!("  {name:<22} -> (no quote)"),
        }
    }
    let (sell_name, _sell_key, usdc_back) =
        best_sell.ok_or_else(|| anyhow!("no venue quoted leg 2"))?;
    println!("  => best sell: {sell_name} ({} USDC)\n", fmt(usdc_back, 6));

    // --- 6. Round-trip P&L. ---
    println!("Round trip: buy WETH on {buy_name}, sell on {sell_name}");
    if usdc_back >= usdc_in {
        let profit = usdc_back - usdc_in;
        println!(
            "  GROSS PROFIT: +{} USDC (before gas) — act only if it clears gas + risk.",
            fmt(profit, 6)
        );
    } else {
        let loss = usdc_in - usdc_back;
        println!(
            "  Net: -{} USDC (round-trip fees exceed the spread — no arb here, as expected\n  \
             at a historical block in an efficient market). The detection machinery is the point.",
            fmt(loss, 6)
        );
    }

    Ok(())
}

/// Quote `amount_in` of `token_in` -> `token_out` on one registered pool, fully
/// offline, by dispatching through the registry to the right adapter. Returns
/// `None` if the pool/adapter is missing or the quote reverts.
fn quote(
    registry: &AdapterRegistry,
    cache: &mut EvmCache,
    key: &PoolKey,
    token_in: Address,
    token_out: Address,
    amount_in: U256,
    cfg: &SimConfig,
) -> Option<U256> {
    let pool = registry.pool(key)?;
    let adapter = registry.adapter(key.protocol())?;
    adapter
        .simulate_swap(pool, cache, token_in, token_out, amount_in, cfg)
        .ok()
        .map(|q| q.amount_out)
}

/// Pretty-print a raw token amount with `decimals` (display only).
fn fmt(raw: U256, decimals: u32) -> String {
    let scale = U256::from(10u64).pow(U256::from(decimals));
    let whole = raw / scale;
    let frac = raw % scale;
    // Zero-pad the fractional part to `decimals` digits, then show up to 6.
    let frac_digits = frac.to_string();
    let pad = (decimals as usize).saturating_sub(frac_digits.len());
    let frac_full = format!("{}{frac_digits}", "0".repeat(pad));
    let frac_short = &frac_full[..frac_full.len().min(6)];
    format!("{whole}.{frac_short}")
}
