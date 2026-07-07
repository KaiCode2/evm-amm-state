//! Gated (`#[ignore]`, RPC) parity tests for the Solidly V2 (Aerodrome) preset —
//! MANAGER RUNS THESE. They pin the real on-chain constants the offline
//! `discovery_solidly.rs` deliberately does not depend on.
//!
//! What is verified against a live chain (Base):
//!   1. **`getPool` base slot** — the `getPool[t0][t1][stable]` slot our `derive`
//!      helper computes must hold the SAME pool address the factory's public
//!      `getPool(tokenA, tokenB, bool stable)` getter returns
//!      (`eth_getStorageAt` == `eth_call`), for BOTH the volatile and stable
//!      variants. This is the check that catches a wrong `get_pool_base_slot`.
//!   2. **Storage layout** — the reserve/token slots in the discovered pool's
//!      [`SolidlyStorageLayout`] must match the pool's public getters
//!      (`getReserves()` / `tokens()`), read straight from storage.
//!   3. **Discovery end-to-end** — `SolidlyFactory` (via the Aerodrome preset)
//!      resolves the same pool through the batched read path.
//!
//! Not run in CI (no network). Build-checked via
//! `cargo build --tests --test discovery_solidly_rpc`. To run:
//! ```text
//! # Aerodrome (Base): E2E_BASE_RPC_URL, or E2E_RPC_URL on an Alchemy url
//! # (its `eth-mainnet` host is swapped to `base-mainnet`).
//! E2E_BASE_RPC_URL=<base-archive-url> cargo test --test discovery_solidly_rpc -- --ignored
//! ```
//!
//! ## What these tests pin
//!
//! The `SolidlyFactoryConfig::aerodrome` preset ships the on-chain-confirmed
//! `get_pool_base_slot` (5) and reserve/token storage layout; these gated tests
//! confirm them against Aerodrome's live factory + a real pool on Base.
//! `verify_derivations` stays OFF for this preset because no CREATE2 init-code
//! hash is pinned for Aerodrome pools (unlike the CL presets), so there is no
//! derivation to cross-check — discovery relies on the factory storage read.
//! - `aerodrome_get_pool_base_slot_matches_getter` confirms the `_getPool` base
//!   slot (and the `bool` salt encoding); a regression means
//!   `SOLIDLY_GET_POOL_BASE_SLOT` in `factory.rs` is wrong.
//! - `aerodrome_storage_layout_matches_getters` confirms the reserve/token slots;
//!   a regression means `SOLIDLY_AERODROME_LAYOUT` in `factory.rs` is wrong.

#![cfg(feature = "solidly-v2")]

use std::sync::Arc;

use alloy_eips::{BlockId, BlockNumberOrTag};
use alloy_network::{AnyNetwork, TransactionBuilder};
use alloy_primitives::{Address, Bytes, U256, address};
use alloy_provider::{Provider, RootProvider};
use alloy_rpc_types_eth::TransactionRequest;
use alloy_sol_types::{SolCall, sol};
use anyhow::{Context, Result};

use evm_amm_state::adapters::factory::derive;
use evm_amm_state::adapters::{
    PoolDiscovery, PoolFactory, PoolKey, PoolQuery, ProtocolId, SolidlyFactory,
    SolidlyFactoryConfig,
};
use evm_fork_cache::cache::EvmCache;

sol! {
    /// Aerodrome / Velodrome V2 `PoolFactory` getter.
    function getPool(address tokenA, address tokenB, bool stable) returns (address pool);
    /// Aerodrome / Velodrome V2 pool getters (for the storage-layout cross-check).
    function getReserves() returns (uint256 reserve0, uint256 reserve1, uint256 blockTimestampLast);
    function tokens() returns (address token0, address token1);
}

// --- Aerodrome (Base) ---
// The canonical Aerodrome PoolFactory on Base.
const AERODROME_FACTORY: Address = address!("420DD381b31aEf6683db6B902084cB0FFECe40Da");
const BASE_WETH: Address = address!("4200000000000000000000000000000000000006");
const BASE_USDC: Address = address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913");
// A fork block well after Aerodrome launched on Base.
const BASE_FORK_BLOCK: u64 = 47_700_000;

fn base_rpc_url() -> Option<String> {
    if let Ok(url) = std::env::var("E2E_BASE_RPC_URL") {
        return Some(url);
    }
    std::env::var("E2E_RPC_URL")
        .ok()
        .map(|url| url.replace("eth-mainnet", "base-mainnet"))
}

async fn provider(url: &str) -> Result<Arc<RootProvider<AnyNetwork>>> {
    Ok(Arc::new(
        RootProvider::<AnyNetwork>::connect(url)
            .await
            .context("connect RPC")?,
    ))
}

async fn eth_call(
    provider: &RootProvider<AnyNetwork>,
    target: Address,
    calldata: Bytes,
    block: u64,
) -> Result<Bytes> {
    let tx = TransactionRequest::default()
        .with_to(target)
        .with_input(calldata);
    provider
        .call(tx.into())
        .block(BlockId::Number(BlockNumberOrTag::Number(block)))
        .await
        .context("eth_call")
}

/// Resolve the pool address the factory's `getPool(t0,t1,stable)` getter returns.
async fn getter_pool(
    provider: &RootProvider<AnyNetwork>,
    factory: Address,
    t0: Address,
    t1: Address,
    stable: bool,
    block: u64,
) -> Result<Address> {
    let out = eth_call(
        provider,
        factory,
        Bytes::from(
            getPoolCall {
                tokenA: t0,
                tokenB: t1,
                stable,
            }
            .abi_encode(),
        ),
        block,
    )
    .await?;
    let word = U256::from_be_slice(&out);
    Ok(Address::from_slice(&word.to_be_bytes::<32>()[12..]))
}

/// Read the pool address held at a factory STORAGE slot, via `eth_getStorageAt`
/// at the pinned block — independent of the getter.
async fn storage_addr(
    provider: &RootProvider<AnyNetwork>,
    account: Address,
    slot: U256,
    block: u64,
) -> Result<Address> {
    let word = provider
        .get_storage_at(account, slot)
        .block_id(BlockId::Number(BlockNumberOrTag::Number(block)))
        .await
        .context("eth_getStorageAt")?;
    Ok(Address::from_slice(&word.to_be_bytes::<32>()[12..]))
}

async fn storage_word(
    provider: &RootProvider<AnyNetwork>,
    account: Address,
    slot: U256,
    block: u64,
) -> Result<U256> {
    provider
        .get_storage_at(account, slot)
        .block_id(BlockId::Number(BlockNumberOrTag::Number(block)))
        .await
        .context("eth_getStorageAt")
}

/// Aerodrome: the `getPool[t0][t1][stable]` base slot (preset slot 5) must
/// hold the same pool the factory's `getPool(t0,t1,stable)` getter returns, for
/// BOTH variants that exist. Proves the base slot + `bool` salt encoding on-chain.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Base RPC (E2E_BASE_RPC_URL / E2E_RPC_URL on Alchemy); run with --ignored"]
async fn aerodrome_get_pool_base_slot_matches_getter() -> Result<()> {
    let Some(url) = base_rpc_url() else {
        eprintln!("no Base RPC; skipping");
        return Ok(());
    };
    let provider = provider(&url).await?;
    let config = SolidlyFactoryConfig::aerodrome(AERODROME_FACTORY);
    let (t0, t1) = derive::sort_tokens(BASE_WETH, BASE_USDC);

    let mut checked = 0usize;
    for stable in [false, true] {
        let getter = getter_pool(
            &provider,
            AERODROME_FACTORY,
            t0,
            t1,
            stable,
            BASE_FORK_BLOCK,
        )
        .await?;
        if getter == Address::ZERO {
            continue;
        }
        checked += 1;
        let slot = derive::solidly_get_pool_slot(config.get_pool_base_slot, t0, t1, stable);
        let from_storage =
            storage_addr(&provider, AERODROME_FACTORY, slot, BASE_FORK_BLOCK).await?;
        assert_eq!(
            from_storage, getter,
            "Aerodrome getPool base slot {} is WRONG for stable={stable}: \
             storage={from_storage:?} getter={getter:?}. Fix SOLIDLY_GET_POOL_BASE_SLOT / the \
             shipped aerodrome preset has regressed.",
            config.get_pool_base_slot
        );
    }
    assert!(
        checked > 0,
        "no Aerodrome WETH/USDC pool found at block {BASE_FORK_BLOCK}; pick a live pair"
    );
    Ok(())
}

/// Aerodrome: the preset's [`SolidlyStorageLayout`] must match the
/// pool's public getters — `reserve0`/`reserve1` == `getReserves()` and
/// `token0`/`token1` == `tokens()` — read straight from storage. Pins the layout.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Base RPC (E2E_BASE_RPC_URL / E2E_RPC_URL on Alchemy); run with --ignored"]
async fn aerodrome_storage_layout_matches_getters() -> Result<()> {
    let Some(url) = base_rpc_url() else {
        eprintln!("no Base RPC; skipping");
        return Ok(());
    };
    let provider = provider(&url).await?;
    let config = SolidlyFactoryConfig::aerodrome(AERODROME_FACTORY);
    let layout = config.storage_layout;
    let (t0, t1) = derive::sort_tokens(BASE_WETH, BASE_USDC);

    // Use the volatile pool (the canonical WETH/USDC pool on Aerodrome).
    let pool = getter_pool(&provider, AERODROME_FACTORY, t0, t1, false, BASE_FORK_BLOCK).await?;
    assert_ne!(
        pool,
        Address::ZERO,
        "no Aerodrome volatile WETH/USDC pool at block {BASE_FORK_BLOCK}"
    );

    // Ground truth from the pool's own getters.
    let reserves_out = eth_call(
        &provider,
        pool,
        Bytes::from(getReservesCall {}.abi_encode()),
        BASE_FORK_BLOCK,
    )
    .await?;
    let reserves = getReservesCall::abi_decode_returns(&reserves_out).context("getReserves")?;
    let tokens_out = eth_call(
        &provider,
        pool,
        Bytes::from(tokensCall {}.abi_encode()),
        BASE_FORK_BLOCK,
    )
    .await?;
    let toks = tokensCall::abi_decode_returns(&tokens_out).context("tokens")?;

    let slot_reserve0 =
        storage_word(&provider, pool, layout.reserve0_slot, BASE_FORK_BLOCK).await?;
    let slot_reserve1 =
        storage_word(&provider, pool, layout.reserve1_slot, BASE_FORK_BLOCK).await?;
    let slot_token0 = storage_addr(&provider, pool, layout.token0_slot, BASE_FORK_BLOCK).await?;
    let slot_token1 = storage_addr(&provider, pool, layout.token1_slot, BASE_FORK_BLOCK).await?;

    assert_eq!(
        slot_reserve0, reserves.reserve0,
        "Aerodrome reserve0 slot {} WRONG: storage={slot_reserve0} getter={}. Fix \
         SOLIDLY_AERODROME_LAYOUT has regressed.",
        layout.reserve0_slot, reserves.reserve0
    );
    assert_eq!(
        slot_reserve1, reserves.reserve1,
        "Aerodrome reserve1 slot {} WRONG: storage={slot_reserve1} getter={}. Fix \
         SOLIDLY_AERODROME_LAYOUT has regressed.",
        layout.reserve1_slot, reserves.reserve1
    );
    assert_eq!(
        slot_token0, toks.token0,
        "Aerodrome token0 slot {} WRONG: storage={slot_token0:?} getter={:?}. Fix \
         SOLIDLY_AERODROME_LAYOUT has regressed.",
        layout.token0_slot, toks.token0
    );
    assert_eq!(
        slot_token1, toks.token1,
        "Aerodrome token1 slot {} WRONG: storage={slot_token1:?} getter={:?}. Fix \
         SOLIDLY_AERODROME_LAYOUT has regressed.",
        layout.token1_slot, toks.token1
    );
    Ok(())
}

/// Aerodrome end-to-end: the preset resolves the live WETH/USDC pool(s) through
/// the batched discovery path (proves both the base slot and the driver wiring).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Base RPC (E2E_BASE_RPC_URL / E2E_RPC_URL on Alchemy); run with --ignored"]
async fn aerodrome_discovery_resolves_live_pool() -> Result<()> {
    let Some(url) = base_rpc_url() else {
        eprintln!("no Base RPC; skipping");
        return Ok(());
    };
    let mut cache = EvmCache::at_block(
        provider(&url).await?,
        BlockId::Number(BlockNumberOrTag::Number(BASE_FORK_BLOCK)),
    )
    .await;

    let discovery =
        PoolDiscovery::new([
            Box::new(SolidlyFactory::new(SolidlyFactoryConfig::aerodrome(
                AERODROME_FACTORY,
            ))) as Box<dyn PoolFactory>,
        ]);
    let found = discovery.find(
        &mut cache,
        PoolQuery::pair(BASE_WETH, BASE_USDC).on(ProtocolId::SolidlyV2),
    )?;
    assert!(
        !found.is_empty(),
        "Aerodrome preset found no WETH/USDC pool — likely a wrong get_pool_base_slot"
    );
    for pool in &found {
        assert!(
            matches!(pool.key, PoolKey::SolidlyV2(_)),
            "discovered pool must be keyed SolidlyV2"
        );
    }
    Ok(())
}
