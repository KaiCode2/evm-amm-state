//! Env-gated (`#[ignore]`) live parity for the `eth_createAccessList` two-shot
//! cold-warming path — MANAGER RUNS THIS.
//!
//! Given `E2E_RPC_URL` (an archive node), this forks mainnet at a pinned block
//! and asserts that [`AdapterRegistry::cold_start_primed`] (which derives an
//! unknown read-set with one `eth_createAccessList`, bulk-loads it, then runs the
//! discover warm) reaches `Ready` and warms **exactly the same read-set** as the
//! plain local-discovery [`AdapterRegistry::cold_start`], for real Curve pools.
//! This proves the remote access-list path integrates with a real provider and is
//! faithful to local discovery. (The *latency* win is demonstrated by
//! `examples/curve_cold_start_phases.rs`; an under-warm would simply fault the
//! missing slots during the warm discover — still correct here, just slower there.)
//!
//! Not run in CI (no network). Build-checked via
//! `cargo build --tests --test access_list_discovery_rpc`. To run:
//! ```text
//! E2E_RPC_URL=<archive-url> cargo test --test access_list_discovery_rpc -- --ignored --nocapture
//! ```

use std::sync::Arc;

use alloy_eips::{BlockId, BlockNumberOrTag};
use alloy_network::AnyNetwork;
use alloy_primitives::{Address, U256, address};
use alloy_provider::RootProvider;
use anyhow::{Context, Result};

use evm_amm_state::adapters::{
    AdapterRegistry, ColdStartOutcome, ColdStartPolicy, CurveAdapter, CurveMetadata, CurveVariant,
    PoolKey, PoolRegistration, ProtocolMetadata,
};
use evm_fork_cache::cache::EvmCache;

// Same pinned block + pools as `adapter_swap_sim_rpc.rs`.
const FORK_BLOCK: u64 = 20_000_000;
const CURVE_3POOL: Address = address!("bEbc44782C7dB0a1A60Cb6fe97d0b483032FF1C7");
const DAI: Address = address!("6B175474E89094C44Da98b954EedeAC495271d0F");
const USDC: Address = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
const USDT: Address = address!("dAC17F958D2ee523a2206206994597C13D831ec7");
const TRICRYPTO2: Address = address!("D51a44d3FaE010294C616388b506AcdA1bfAAE46");
const WBTC: Address = address!("2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599");
const WETH: Address = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");

fn rpc_url() -> Option<String> {
    std::env::var("E2E_RPC_URL").ok()
}

async fn provider(url: &str) -> Result<Arc<RootProvider<AnyNetwork>>> {
    Ok(Arc::new(
        RootProvider::<AnyNetwork>::connect(url)
            .await
            .context("connect RPC url")?,
    ))
}

fn curve_registry() -> AdapterRegistry {
    let mut registry = AdapterRegistry::new();
    registry
        .register_adapter(Arc::new(CurveAdapter::default()))
        .expect("register curve adapter");
    registry
}

fn sorted_discovered_slots(reg: &PoolRegistration) -> Vec<U256> {
    match &reg.metadata {
        ProtocolMetadata::Curve(m) => {
            let mut slots = m.discovered_slots.clone();
            slots.sort_unstable();
            slots
        }
        _ => Vec::new(),
    }
}

/// Cold-start `pool` twice from an unknown read-set — once via plain local
/// discovery, once via `cold_start_primed` (access-list-derived) — and assert both
/// reach `Ready` and warm the identical read-set.
async fn assert_primed_matches_local(
    url: &str,
    pool: Address,
    coins: Vec<Address>,
    variant: CurveVariant,
) -> Result<()> {
    let block = BlockId::Number(BlockNumberOrTag::Number(FORK_BLOCK));
    let registration = || {
        PoolRegistration::new(PoolKey::Curve(pool))
            .with_state_address(pool)
            .with_metadata(ProtocolMetadata::Curve(
                CurveMetadata::default()
                    .with_coins(coins.clone())
                    .with_variant(variant),
            ))
    };

    // Local discovery (the baseline).
    let local_provider = provider(url).await?;
    let mut local_cache = EvmCache::at_block(local_provider, block).await;
    let mut local = registration();
    let local_outcome =
        curve_registry().cold_start(&mut local, &mut local_cache, ColdStartPolicy::Eager)?;
    assert!(
        matches!(local_outcome, ColdStartOutcome::Ready(_)),
        "local cold_start should reach Ready for {pool}, got {local_outcome:?}"
    );

    // Access-list-primed (the two-shot path).
    let primed_provider = provider(url).await?;
    let mut primed_cache = EvmCache::at_block(primed_provider.clone(), block).await;
    let mut primed = registration();
    let primed_outcome = curve_registry()
        .cold_start_primed(
            &mut primed,
            &mut primed_cache,
            primed_provider.as_ref(),
            ColdStartPolicy::Eager,
        )
        .await?;
    assert!(
        matches!(primed_outcome, ColdStartOutcome::Ready(_)),
        "primed cold_start should reach Ready for {pool}, got {primed_outcome:?}"
    );

    let local_slots = sorted_discovered_slots(&local);
    let primed_slots = sorted_discovered_slots(&primed);
    assert!(
        !primed_slots.is_empty(),
        "a non-empty read-set must be discovered for {pool}"
    );
    assert_eq!(
        primed_slots, local_slots,
        "access-list-primed read-set must equal the local-discovery read-set for {pool}"
    );
    eprintln!("{pool}: primed read-set matches local ({} slots)", primed_slots.len());
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires E2E_RPC_URL archive node; run with --ignored"]
async fn access_list_primed_matches_local_stableswap_3pool() -> Result<()> {
    let Some(url) = rpc_url() else {
        eprintln!("E2E_RPC_URL unset; skipping");
        return Ok(());
    };
    assert_primed_matches_local(&url, CURVE_3POOL, vec![DAI, USDC, USDT], CurveVariant::StableSwap)
        .await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires E2E_RPC_URL archive node; run with --ignored"]
async fn access_list_primed_matches_local_cryptoswap_tricrypto2() -> Result<()> {
    let Some(url) = rpc_url() else {
        eprintln!("E2E_RPC_URL unset; skipping");
        return Ok(());
    };
    assert_primed_matches_local(&url, TRICRYPTO2, vec![USDT, WBTC, WETH], CurveVariant::CryptoSwap)
        .await
}
