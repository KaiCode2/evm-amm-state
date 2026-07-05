//! Benchmark: declarative token-basket pool discovery vs naive per-pair scans.
//!
//! Given a basket of tokens, `find(PoolQuery::basket(..))` expands all
//! `C(n,2)` pairs (× all V3 fee tiers), collects every factory mapping slot, and
//! resolves them in ONE batched `read_storage_slots` (a single bulk `eth_call`
//! on `EvmCache`). The naive baseline instead calls `find(PoolQuery::pair(..)
//! .on(protocol))` per pair per protocol, one round-trip each.
//!
//! Each strategy runs against its OWN cold cache pinned to the same block, so
//! the comparison is round-trips-vs-round-trips, not warm-cache hits.
//!
//! ```text
//! E2E_RPC_URL=https://... cargo run --release --example token_basket_bench
//! ```

use std::env;
use std::sync::Arc;
use std::time::Instant;

use alloy_eips::BlockId;
use alloy_network::AnyNetwork;
use alloy_primitives::{Address, address};
use alloy_provider::{Provider, RootProvider};
use anyhow::{Context, Result};
use evm_amm_state::adapters::factory::derive;
use evm_amm_state::adapters::{
    AdapterRegistry, ConcentratedLiquidityAdapter, FactoryConfig, PoolDiscovery, PoolQuery,
    ProtocolId, UniswapV2Adapter, UniswapV2FactoryConfig, UniswapV3FactoryConfig,
};
use evm_fork_cache::cache::EvmCache;

const UNISWAP_V2_FACTORY: Address = address!("5C69bEe701ef814a2B6a3EDD4B1652CB9cc5aA6f");
const UNISWAP_V3_FACTORY: Address = address!("1F98431c8aD98523631AE4a59f267346ea31F984");

const WETH: Address = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
const USDC: Address = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
const USDT: Address = address!("dAC17F958D2ee523a2206206994597C13D831ec7");
const DAI: Address = address!("6B175474E89094C44Da98b954EedeAC495271d0F");
const WBTC: Address = address!("2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599");

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let Ok(rpc_url) = env::var("E2E_RPC_URL") else {
        println!("token_basket_bench: set E2E_RPC_URL to an Ethereum RPC endpoint; skipping.");
        return Ok(());
    };

    let basket = [WETH, USDC, USDT, DAI, WBTC];
    let pairs = derive::pairs_among(&basket);

    let provider: Arc<RootProvider<AnyNetwork>> = Arc::new(
        RootProvider::<AnyNetwork>::connect(&rpc_url)
            .await
            .context("connect E2E_RPC_URL")?,
    );
    let latest = provider.get_block_number().await.context("latest block")?;
    let pinned = latest.saturating_sub(8);
    let block = BlockId::number(pinned);

    let v2_config = UniswapV2FactoryConfig::uniswap_v2(UNISWAP_V2_FACTORY).with_fee_bps(30);
    let v3_config = UniswapV3FactoryConfig::uniswap_v3(UNISWAP_V3_FACTORY);
    let fee_tiers = v3_config.fee_tiers.len();
    let config = || {
        FactoryConfig::default()
            .with_uniswap_v2(v2_config.clone())
            .with_uniswap_v3(v3_config.clone())
    };

    let registry = registry()?;
    let candidate_slots = pairs.len() /* V2 getPair */
        + fee_tiers /* V3 feeAmountTickSpacing (per fee) */
        + pairs.len() * fee_tiers /* V3 getPool */;

    println!(
        "token_basket_bench: basket={} tokens, pairs={}, V3 fee tiers={}, pinned block={pinned}",
        basket.len(),
        pairs.len(),
        fee_tiers
    );
    println!("candidate mapping slots across both factories: {candidate_slots}");

    // --- Strategy A: naive per-pair scan (cold cache) ---
    let discovery_naive = PoolDiscovery::for_registry(&registry, config());
    let mut cache_naive = EvmCache::builder(provider.clone()).block(block).build().await;
    let naive_start = Instant::now();
    let mut naive_pools = 0usize;
    for (token0, token1) in &pairs {
        naive_pools += discovery_naive
            .find(
                &mut cache_naive,
                PoolQuery::pair(*token0, *token1).on(ProtocolId::UniswapV2),
            )
            .context("naive V2 find")?
            .len();
        naive_pools += discovery_naive
            .find(
                &mut cache_naive,
                PoolQuery::pair(*token0, *token1).on(ProtocolId::UniswapV3),
            )
            .context("naive V3 find")?
            .len();
    }
    let naive_elapsed = naive_start.elapsed();
    // Naive round-trips: one V2 read per pair + one (batched-per-pair) V3 read per pair.
    let naive_round_trips = pairs.len() * 2;

    // --- Strategy B: one-shot token-basket discovery (cold cache) ---
    let discovery_basket = PoolDiscovery::for_registry(&registry, config());
    let mut cache_basket = EvmCache::builder(provider.clone()).block(block).build().await;
    let basket_start = Instant::now();
    let basket_pools = discovery_basket
        .find(&mut cache_basket, PoolQuery::basket(basket.iter().copied()))
        .context("basket find")?;
    let basket_elapsed = basket_start.elapsed();

    println!();
    println!("naive per-pair:   pools={naive_pools:>3}  round_trips={naive_round_trips:>3}  time={naive_elapsed:?}");
    println!(
        "one-shot basket:  pools={:>3}  round_trips=  1  time={basket_elapsed:?}",
        basket_pools.len()
    );
    let speedup = naive_elapsed.as_secs_f64() / basket_elapsed.as_secs_f64().max(f64::MIN_POSITIVE);
    println!(
        "→ {}× fewer round-trips, {speedup:.1}× faster wall-clock",
        naive_round_trips
    );

    println!("\ndiscovered pools (one-shot):");
    for pool in &basket_pools {
        println!("  {:?}  {:?}", pool.key, pool.source);
    }

    Ok(())
}

fn registry() -> Result<AdapterRegistry> {
    let mut registry = AdapterRegistry::new();
    registry.register_adapter(Arc::new(UniswapV2Adapter::default()))?;
    registry.register_adapter(Arc::new(ConcentratedLiquidityAdapter::default()))?;
    Ok(registry)
}
