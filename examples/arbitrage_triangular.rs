//! End-to-end **triangular arbitrage** demo.
//!
//! A triangular arb walks a *cycle* of pools that starts and ends in the same
//! token, feeding each leg's output into the next. If the product of the
//! exchange rates around the loop exceeds 1 (net of fees), the cycle is
//! profitable. This example walks a real three-protocol loop:
//!
//! ```text
//!   USDC --(Curve 3pool, StableSwap)--> USDT
//!   USDT --(Curve tricrypto2, CryptoSwap)--> WETH
//!   WETH --(Uniswap V3 0.05%)--> USDC
//! ```
//!
//! Every hop is the pool's own on-chain quote entrypoint (`get_dy` / `QuoterV2`)
//! executed in revm against one shared warmed cache — no reimplemented math, no
//! RPC in the loop. The output of each leg is the exact input to the next, so
//! the final USDC vs the initial USDC is the true cycle P&L.
//!
//! As with any historical-block demo against an efficient market, the loop is
//! typically a small net loss (fees around the triangle). The point is the
//! mechanism: chaining quotes across protocols to price a cycle exactly. A
//! searcher would also sweep many cycles and sizes and fire only when one clears
//! gas.
//!
//! Run (env-gated; needs an archive RPC to warm state):
//! ```text
//! E2E_RPC_URL=<archive-url> cargo run --example arbitrage_triangular
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
    CurveVariant, PoolKey, PoolRegistration, ProtocolMetadata, SimConfig, V3Metadata,
};
use evm_fork_cache::cache::EvmCache;

const FORK_BLOCK: u64 = 20_000_000;

const USDC: Address = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
const WETH: Address = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
const DAI: Address = address!("6B175474E89094C44Da98b954EedeAC495271d0F");
const USDT: Address = address!("dAC17F958D2ee523a2206206994597C13D831ec7");
const WBTC: Address = address!("2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599");

const V3_QUOTER_V2: Address = address!("61fFE014bA17989E743c5F6cB21bF9697530B21e");
const V3_USDC_WETH_005: Address = address!("88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640");
const CURVE_3POOL: Address = address!("bEbc44782C7dB0a1A60Cb6fe97d0b483032FF1C7");
const TRICRYPTO2: Address = address!("D51a44d3FaE010294C616388b506AcdA1bfAAE46");

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let Ok(url) = std::env::var("E2E_RPC_URL") else {
        eprintln!(
            "E2E_RPC_URL unset — this example warms real pools from an archive node.\n\
             Run with: E2E_RPC_URL=<archive-url> cargo run --example arbitrage_triangular"
        );
        return Ok(());
    };

    // --- Adapters + one shared cache. ---
    let mut registry = AdapterRegistry::new();
    registry.register_adapter(Arc::new(CurveAdapter::default()))?;
    registry.register_adapter(Arc::new(ConcentratedLiquidityAdapter::default()))?;

    let provider = RootProvider::<AnyNetwork>::connect(&url).await?;
    let mut cache = EvmCache::at_block(
        Arc::new(provider),
        BlockId::Number(BlockNumberOrTag::Number(FORK_BLOCK)),
    )
    .await;

    // --- Cold-start the three legs of the cycle. ---
    let legs = vec![
        PoolRegistration::new(PoolKey::Curve(CURVE_3POOL))
            .with_state_address(CURVE_3POOL)
            .with_metadata(ProtocolMetadata::Curve(
                CurveMetadata::default()
                    .with_coins(vec![DAI, USDC, USDT])
                    .with_discovered_slots(Vec::new())
                    .with_variant(CurveVariant::StableSwap),
            )),
        PoolRegistration::new(PoolKey::Curve(TRICRYPTO2))
            .with_state_address(TRICRYPTO2)
            .with_metadata(ProtocolMetadata::Curve(
                CurveMetadata::default()
                    .with_coins(vec![USDT, WBTC, WETH])
                    .with_discovered_slots(Vec::new())
                    .with_variant(CurveVariant::CryptoSwap),
            )),
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
    ];
    let pool_3pool = PoolKey::Curve(CURVE_3POOL);
    let pool_tricrypto = PoolKey::Curve(TRICRYPTO2);
    let pool_v3 = PoolKey::UniswapV3(V3_USDC_WETH_005);
    for mut reg in legs {
        registry.cold_start(&mut reg, &mut cache, ColdStartPolicy::Eager)?;
        registry.register_pool(reg)?;
    }
    println!("Cold-started the 3-leg cycle at block {FORK_BLOCK}.\n");

    let cfg = SimConfig::default().with_v3_quoter(V3_QUOTER_V2);

    // --- Walk the cycle: each leg's output is the next leg's input. ---
    let start_usdc = U256::from(100_000_000_000_u64); // 100,000 USDC
    println!("Start: {} USDC", fmt(start_usdc, 6));

    let usdt = quote(
        &registry,
        &mut cache,
        &pool_3pool,
        USDC,
        USDT,
        start_usdc,
        &cfg,
    )
    .ok_or_else(|| anyhow!("leg 1 (3pool USDC->USDT) failed"))?;
    println!("  leg 1  3pool       USDC -> USDT : {} USDT", fmt(usdt, 6));

    let weth = quote(
        &registry,
        &mut cache,
        &pool_tricrypto,
        USDT,
        WETH,
        usdt,
        &cfg,
    )
    .ok_or_else(|| anyhow!("leg 2 (tricrypto2 USDT->WETH) failed"))?;
    println!(
        "  leg 2  tricrypto2   USDT -> WETH : {} WETH",
        fmt(weth, 18)
    );

    let end_usdc = quote(&registry, &mut cache, &pool_v3, WETH, USDC, weth, &cfg)
        .ok_or_else(|| anyhow!("leg 3 (V3 WETH->USDC) failed"))?;
    println!(
        "  leg 3  Uniswap V3   WETH -> USDC : {} USDC\n",
        fmt(end_usdc, 6)
    );

    // --- Cycle P&L. ---
    if end_usdc >= start_usdc {
        let profit = end_usdc - start_usdc;
        println!(
            "Cycle GROSS PROFIT: +{} USDC (before gas). A searcher fires when this clears gas.",
            fmt(profit, 6)
        );
    } else {
        let loss = start_usdc - end_usdc;
        println!(
            "Cycle net: -{} USDC — the loop's fees exceed any dislocation (expected at a\n\
             historical block). The mechanism — chaining exact cross-protocol quotes — is the point.",
            fmt(loss, 6)
        );
    }

    Ok(())
}

/// Quote one registered pool offline via registry dispatch.
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
    let frac_digits = frac.to_string();
    let pad = (decimals as usize).saturating_sub(frac_digits.len());
    let frac_full = format!("{}{frac_digits}", "0".repeat(pad));
    let frac_short = &frac_full[..frac_full.len().min(6)];
    format!("{whole}.{frac_short}")
}
