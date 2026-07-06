//! Gated (`#[ignore]`, RPC) parity tests for Curve plain-pool discovery via the
//! MetaRegistry — MANAGER RUNS THESE. They pin the real on-chain MetaRegistry
//! constant the offline `discovery_curve.rs` deliberately mocks, and prove the
//! ViewCall discovery path against a live chain.
//!
//! What is verified against Ethereum mainnet:
//!   1. **Canonical MetaRegistry** — `CurveFactoryConfig::mainnet()` pins Curve's
//!      MetaRegistry `0xF98B45FA17DE75FB1aD0e7aFD971b0ca00e379fC`. A DAI/USDC
//!      query resolves the well-known 3pool
//!      (`0xbEbc44782C7dB0a1A60Cb6fe97d0b483032FF1C7`) with its FULL coin set
//!      {DAI, USDC, USDT} and the `StableSwap` variant (asset_type != 4).
//!   2. **Metapool exclusion** — every pool the discovery path returns for the
//!      DAI/USDC query is a PLAIN pool: the live MetaRegistry `is_meta(pool)`
//!      view reports `false` for each. (The MetaRegistry over-returns metapools
//!      for a stable pair; discovery filters them.)
//!
//! Not run in CI (no network). Build-checked via
//! `cargo build --tests --test discovery_curve_rpc --features curve`. To run:
//! ```text
//! E2E_RPC_URL=<eth-mainnet-archive-url> \
//!   cargo test --test discovery_curve_rpc --features curve -- --ignored
//! ```
//!
//! ## What these tests pin
//!
//! `CURVE_MAINNET_META_REGISTRY` in `factory.rs` is a well-known public constant
//! (safe to pin offline). If `curve_mainnet_discovers_3pool` FAILS to find 3pool
//! with {DAI, USDC, USDT} + StableSwap, the MetaRegistry address or the read ABI
//! (`find_pools_for_coins` / `get_coins` / `get_pool_asset_type`) is wrong.

#![cfg(feature = "curve")]

use std::sync::Arc;

use alloy_eips::{BlockId, BlockNumberOrTag};
use alloy_network::{AnyNetwork, TransactionBuilder};
use alloy_primitives::{Address, Bytes, address};
use alloy_provider::{Provider, RootProvider};
use alloy_rpc_types_eth::TransactionRequest;
use alloy_sol_types::{SolCall, sol};
use anyhow::{Context, Result};

use evm_amm_state::adapters::{
    CurveFactory, CurveFactoryConfig, CurveVariant, PoolDiscovery, PoolFactory, PoolKey, PoolQuery,
    ProtocolId, ProtocolMetadata,
};
use evm_fork_cache::cache::EvmCache;

sol! {
    /// The MetaRegistry `is_meta` view, used here as an independent ground-truth
    /// cross-check that every discovered pool is a plain (non-meta) pool.
    function is_meta(address pool) external view returns (bool meta);
}

// --- Curve mainnet constants (mirrored from tests/reactive_curve_ws_e2e.rs). ---
// The canonical Curve MetaRegistry on Ethereum mainnet.
const META_REGISTRY: Address = address!("F98B45FA17DE75FB1aD0e7aFD971b0ca00e379fC");
// 3pool (StableSwap, DAI/USDC/USDT).
const THREEPOOL: Address = address!("bEbc44782C7dB0a1A60Cb6fe97d0b483032FF1C7");
const DAI: Address = address!("6B175474E89094C44Da98b954EedeAC495271d0F");
const USDC: Address = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
const USDT: Address = address!("dAC17F958D2ee523a2206206994597C13D831ec7");
// A fork block well after the MetaRegistry and 3pool were deployed.
const FORK_BLOCK: u64 = 20_000_000;

fn rpc_url() -> Option<String> {
    std::env::var("E2E_RPC_URL").ok()
}

async fn provider(url: &str) -> Result<Arc<RootProvider<AnyNetwork>>> {
    Ok(Arc::new(
        RootProvider::<AnyNetwork>::connect(url)
            .await
            .context("connect RPC")?,
    ))
}

async fn cache(url: &str) -> Result<EvmCache> {
    Ok(EvmCache::at_block(
        provider(url).await?,
        BlockId::Number(BlockNumberOrTag::Number(FORK_BLOCK)),
    )
    .await)
}

/// The mainnet MetaRegistry (via the `mainnet()` preset) resolves a DAI/USDC
/// query to 3pool, carrying the full {DAI, USDC, USDT} coin set and the
/// `StableSwap` variant. Proves the pinned MetaRegistry address + read ABI.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Ethereum mainnet RPC (E2E_RPC_URL); run with --ignored"]
async fn curve_mainnet_discovers_3pool() -> Result<()> {
    let Some(url) = rpc_url() else {
        eprintln!("no E2E_RPC_URL; skipping");
        return Ok(());
    };
    // The preset must pin the canonical MetaRegistry.
    assert_eq!(
        CurveFactoryConfig::mainnet().meta_registry,
        META_REGISTRY,
        "mainnet() preset must pin the canonical MetaRegistry"
    );

    let mut cache = cache(&url).await?;
    let discovery = PoolDiscovery::new([
        Box::new(CurveFactory::new(CurveFactoryConfig::mainnet())) as Box<dyn PoolFactory>,
    ]);

    let found = discovery.find(&mut cache, PoolQuery::pair(DAI, USDC).on(ProtocolId::Curve))?;
    assert!(
        !found.is_empty(),
        "MetaRegistry found no DAI/USDC plain pool — wrong registry address or read ABI"
    );

    let threepool = found
        .iter()
        .find(|p| p.key == PoolKey::Curve(THREEPOOL))
        .expect("3pool must be among the discovered DAI/USDC plain pools");

    let ProtocolMetadata::Curve(md) = &threepool.registration.metadata else {
        panic!("expected Curve metadata");
    };
    // Full multi-token coin set, not just the queried pair.
    assert!(
        md.coins.contains(&DAI) && md.coins.contains(&USDC) && md.coins.contains(&USDT),
        "3pool coin set must be the full {{DAI, USDC, USDT}}, got {:?}",
        md.coins
    );
    // asset_type != 4 => StableSwap (int128 get_dy).
    assert_eq!(
        md.variant,
        CurveVariant::StableSwap,
        "3pool is a StableSwap (USD) pool"
    );
    Ok(())
}

/// Metapool exclusion: every pool the DAI/USDC discovery returns is a PLAIN pool.
/// Cross-checked against the live MetaRegistry `is_meta(pool)` view — the filter
/// must have dropped any metapool the registry over-returned for the stable pair.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Ethereum mainnet RPC (E2E_RPC_URL); run with --ignored"]
async fn curve_mainnet_excludes_metapools() -> Result<()> {
    let Some(url) = rpc_url() else {
        eprintln!("no E2E_RPC_URL; skipping");
        return Ok(());
    };
    let provider = provider(&url).await?;
    let mut cache = cache(&url).await?;
    let discovery = PoolDiscovery::new([
        Box::new(CurveFactory::new(CurveFactoryConfig::mainnet())) as Box<dyn PoolFactory>,
    ]);

    let found = discovery.find(&mut cache, PoolQuery::pair(DAI, USDC).on(ProtocolId::Curve))?;
    assert!(!found.is_empty(), "expected at least 3pool for DAI/USDC");

    for pool in &found {
        let PoolKey::Curve(address) = pool.key else {
            panic!("discovered pool must be keyed Curve");
        };
        let out = eth_call(
            &provider,
            META_REGISTRY,
            Bytes::from(is_metaCall { pool: address }.abi_encode()),
        )
        .await?;
        let meta = is_metaCall::abi_decode_returns(&out).context("is_meta")?;
        assert!(
            !meta,
            "discovered pool {address:?} is a metapool — the plain-pool filter failed"
        );
    }
    Ok(())
}

async fn eth_call(
    provider: &RootProvider<AnyNetwork>,
    target: Address,
    calldata: Bytes,
) -> Result<Bytes> {
    let tx = TransactionRequest::default()
        .with_to(target)
        .with_input(calldata);
    provider
        .call(tx.into())
        .block(BlockId::Number(BlockNumberOrTag::Number(FORK_BLOCK)))
        .await
        .context("eth_call")
}
