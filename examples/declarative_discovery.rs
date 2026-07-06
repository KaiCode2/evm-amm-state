//! Declarative pool discovery, end to end, with **no pasted pool addresses**.
//!
//! The whole point of `PoolDiscovery` is that you name *tokens and factories*,
//! not pools. Here we:
//!   1. Build a [`FactoryConfig`] naming the canonical mainnet Uniswap V3 factory
//!      and Curve's mainnet MetaRegistry (via `CurveFactoryConfig::mainnet()`).
//!   2. Wire those factories to the registered adapters with
//!      [`PoolDiscovery::for_registry`].
//!   3. Resolve every pool joining any pair of a token basket
//!      (`PoolQuery::basket([WETH, USDC, USDT])`) — the V3 factory in one batched
//!      `read_storage_slots`, the Curve MetaRegistry via its ViewCall — and print
//!      each discovered pool.
//!   4. Bootstrap the discovered registrations with
//!      [`AdapterRegistry::cold_start_many`], register them on an
//!      [`AmmSyncEngine`], and quote one swap fully offline.
//!
//! From cold-start onward the flow is identical to the other examples
//! (`arbitrage_cross_dex`, `factory_discovery_live`); the discovery step is what
//! differs — pools are found, not hand-listed.
//!
//! Env-gated: set `E2E_RPC_URL` to an Ethereum HTTP archive endpoint. Without it
//! the example prints a notice and exits cleanly (so it stays runnable in CI).
//!
//! ```text
//! E2E_RPC_URL=https://<archive-node> cargo run --example declarative_discovery
//! ```

use std::env;
use std::sync::Arc;

use alloy_eips::BlockId;
use alloy_network::AnyNetwork;
use alloy_primitives::{Address, U256, address};
use alloy_provider::{Provider, RootProvider};
use anyhow::{Context, Result};
use evm_amm_state::adapters::{
    AdapterRegistry, AmmSyncEngine, ColdStartOutcome, ColdStartPolicy,
    ConcentratedLiquidityAdapter, CurveAdapter, CurveFactoryConfig, DiscoveredPool, FactoryConfig,
    PoolDiscovery, PoolKey, PoolQuery, PoolRegistration, ProtocolMetadata, SimConfig,
};
use evm_fork_cache::cache::EvmCache;

// Canonical Ethereum-mainnet Uniswap V3 factory and QuoterV2. Curve's mainnet
// MetaRegistry is pinned by `CurveFactoryConfig::mainnet()`, so it needs no
// literal here. These are the only chain constants — no pool addresses.
const UNISWAP_V3_FACTORY: Address = address!("1F98431c8aD98523631AE4a59f267346ea31F984");
const V3_QUOTER_V2: Address = address!("61fFE014bA17989E743c5F6cB21bF9697530B21e");

const WETH: Address = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
const USDC: Address = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
const USDT: Address = address!("dAC17F958D2ee523a2206206994597C13D831ec7");

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let Ok(rpc_url) = env::var("E2E_RPC_URL") else {
        println!(
            "declarative_discovery: set E2E_RPC_URL to an Ethereum archive RPC endpoint; skipping."
        );
        return Ok(());
    };

    // --- fork a cold cache at a recent pinned block ---
    let provider: Arc<RootProvider<AnyNetwork>> = Arc::new(
        RootProvider::<AnyNetwork>::connect(&rpc_url)
            .await
            .context("connect E2E_RPC_URL")?,
    );
    let latest = provider.get_block_number().await.context("latest block")?;
    let pinned = latest.saturating_sub(8);
    let mut cache = EvmCache::builder(provider.clone())
        .block(BlockId::number(pinned))
        .build()
        .await;

    // --- 1. name factories, not pools ---
    let registry = registry_with_adapters()?;
    let discovery = PoolDiscovery::for_registry(
        &registry,
        FactoryConfig::default()
            .with_uniswap_v3_factory(UNISWAP_V3_FACTORY)
            .with_curve(CurveFactoryConfig::mainnet()),
    );

    // --- 2. resolve every pool joining any pair of the basket ---
    // One declarative query. The Uniswap V3 factory resolves all C(3,2) pairs ×
    // fee tiers in a single batched `read_storage_slots`; the Curve MetaRegistry
    // resolves the same pairs through its ViewCall. No pool address was typed.
    let basket = [WETH, USDC, USDT];
    let discovered = discovery
        .find(&mut cache, PoolQuery::basket(basket))
        .context("declarative basket discovery")?;

    println!(
        "declarative_discovery: pinned block {pinned}, basket [WETH, USDC, USDT] -> {} pools",
        discovered.len()
    );
    for pool in &discovered {
        print_discovered_pool(pool);
    }
    if discovered.is_empty() {
        println!("no pools discovered for the basket; nothing to bootstrap.");
        return Ok(());
    }

    // --- 3. bootstrap discovered registrations in one shot ---
    // `cold_start_many` seeds + verifies one-shot-eligible code in one batch,
    // bundles hydration into a single `eth_call`, and falls back per pool where a
    // fast path cannot run — the same bootstrap the other examples use.
    let mut ready: Vec<PoolRegistration> = discovered
        .into_iter()
        .map(|pool| pool.registration)
        .collect();
    let outcomes = registry
        .cold_start_many(
            &mut ready,
            &mut cache,
            provider.as_ref(),
            ColdStartPolicy::Eager,
        )
        .await
        .context("bootstrap discovered pools")?;
    let ready_count = outcomes
        .iter()
        .filter(|o| matches!(o, ColdStartOutcome::Ready(_)))
        .count();
    println!(
        "bootstrapped {ready_count}/{} discovered pools to Ready",
        ready.len()
    );

    // --- 4. register + quote one swap, fully offline ---
    let mut engine = AmmSyncEngine::new(registry)?;
    engine.register_pools(ready)?;

    // Quote 1 USDC -> WETH on the first ready pool that can (offline; the pool's
    // own on-chain quote entrypoint executed in revm — no reimplemented math).
    let cfg = SimConfig::default().with_v3_quoter(V3_QUOTER_V2);
    let one_usdc = U256::from(1_000_000_u64); // USDC has 6 decimals
    let registry = engine.registry();
    let quoted = registry
        .pools()
        .find_map(|pool| quote(registry, &mut cache, &pool.key, USDC, WETH, one_usdc, &cfg));
    match quoted {
        Some((key, amount_out)) => {
            println!("offline quote on {key:?}: 1 USDC -> {amount_out} WETH (raw)");
        }
        None => {
            println!(
                "no discovered pool quoted USDC->WETH (cold-start/quote works exactly as in the other examples)"
            );
        }
    }
    Ok(())
}

fn registry_with_adapters() -> Result<AdapterRegistry> {
    let mut registry = AdapterRegistry::new();
    registry.register_adapter(Arc::new(ConcentratedLiquidityAdapter::default()))?;
    registry.register_adapter(Arc::new(CurveAdapter::default()))?;
    Ok(registry)
}

/// Quote `token_in -> token_out` on one pool, returning its key + output (or
/// `None` if that pool cannot serve the pair). Mirrors the arbitrage examples.
fn quote(
    registry: &AdapterRegistry,
    cache: &mut EvmCache,
    key: &PoolKey,
    token_in: Address,
    token_out: Address,
    amount_in: U256,
    cfg: &SimConfig,
) -> Option<(PoolKey, U256)> {
    let pool = registry.pool(key)?;
    let adapter = registry.adapter(key.protocol())?;
    adapter
        .simulate_swap(pool, cache, token_in, token_out, amount_in, cfg)
        .ok()
        .map(|quote| (key.clone(), quote.amount_out))
}

fn print_discovered_pool(pool: &DiscoveredPool) {
    match &pool.registration.metadata {
        ProtocolMetadata::UniswapV3(metadata) => println!(
            "  {:?} from {:?}: token0={:?}, token1={:?}, fee={:?}, tick_spacing={:?}",
            pool.key,
            pool.source,
            metadata.token0,
            metadata.token1,
            metadata.fee,
            metadata.tick_spacing
        ),
        ProtocolMetadata::Curve(metadata) => println!(
            "  {:?} from {:?}: coins={:?}, variant={:?}",
            pool.key, pool.source, metadata.coins, metadata.variant
        ),
        metadata => println!(
            "  {:?} from {:?}: metadata={metadata:?}",
            pool.key, pool.source
        ),
    }
}
