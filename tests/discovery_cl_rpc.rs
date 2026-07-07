//! Gated (`#[ignore]`, RPC) parity tests for the concentrated-liquidity presets
//! — MANAGER RUNS THESE. They pin the real on-chain constants the offline
//! `discovery_cl.rs` deliberately does not depend on.
//!
//! Two things are verified against a live chain, per fork:
//!   1. **Storage base slots** — the `getPool[t0][t1][key]` slot our `derive`
//!      helper computes must hold the SAME pool address the factory's public
//!      `getPool(...)` getter returns (`eth_getStorageAt` == `eth_call`). This is
//!      the check that catches a wrong `get_pool_base_slot`.
//!   2. **Discovery end-to-end** — `ConcentratedLiquidityFactory` (via the preset)
//!      resolves the same pool through the batched read path.
//!
//! Not run in CI (no network). Build-checked via
//! `cargo build --tests --test discovery_cl_rpc`. To run:
//! ```text
//! # Pancake V3 (Ethereum mainnet):
//! E2E_RPC_URL=<eth-archive-url> cargo test --test discovery_cl_rpc -- --ignored
//! # Slipstream (Base): E2E_BASE_RPC_URL, or E2E_RPC_URL on an Alchemy url
//! # (its `eth-mainnet` host is swapped to `base-mainnet`).
//! ```
//!
//! ## What these tests pin
//!
//! - **Pancake V3**: the preset uses Pancake's OWN factory layout — `get_pool`
//!   base slot 2 and `feeAmountTickSpacing` base slot 1 (not the Uniswap 5 / 4) —
//!   with `verify_derivations` ON and the CREATE2 deployer + init-code hash
//!   pinned. `pancake_get_pool_base_slot_matches_getter` confirms that base slot
//!   against the live factory getter and `pancake_create2_matches_getter`
//!   confirms the CREATE2 derivation; a regression there means the constants in
//!   `ClFactorySpec::pancake_v3` are wrong.
//! - **Slipstream**: the preset is discovery-only (no `create2`, no `quoter` — its
//!   quoter takes a different, tickSpacing-keyed ABI, so quoting rides a
//!   caller-supplied compatible quoter). `slipstream_get_pool_base_slot_matches_getter`
//!   confirms the `getPool` base slot + the tickSpacing salt encoding against
//!   Aerodrome's live CLFactory on Base.

#![cfg(feature = "uniswap-v3")]

use std::sync::Arc;

use alloy_eips::{BlockId, BlockNumberOrTag};
use alloy_network::{AnyNetwork, TransactionBuilder};
use alloy_primitives::{Address, Bytes, U256, address, aliases::I24};
use alloy_provider::{Provider, RootProvider};
use alloy_rpc_types_eth::TransactionRequest;
use alloy_sol_types::{SolCall, sol};
use anyhow::{Context, Result};

use evm_amm_state::adapters::factory::derive;
use evm_amm_state::adapters::{
    ClFactorySpec, ConcentratedLiquidityFactory, PoolDiscovery, PoolFactory, PoolQuery, ProtocolId,
};
use evm_fork_cache::cache::EvmCache;

sol! {
    /// Uniswap/Pancake-style fee-keyed factory getter.
    function getPool(address tokenA, address tokenB, uint24 fee) returns (address pool);
}

// Slipstream's getPool is overloaded with an `int24 tickSpacing` key. Kept in its
// own module so its `getPool(address,address,int24)` selector doesn't collide with
// the `uint24` variant above. (Slipstream defines no `getPoolBySpacing`; that name
// reverts — the real getter is just `getPool` with an int24 arg.)
mod slipstream_abi {
    alloy_sol_types::sol! {
        function getPool(address tokenA, address tokenB, int24 tickSpacing) returns (address pool);
    }
}

// --- Pancake V3 (Ethereum mainnet) ---
const PANCAKE_V3_FACTORY: Address = address!("0BFbCF9fa4f9C56B0F40a671Ad40E0805A091865");
const ETH_USDC: Address = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
const ETH_WETH: Address = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
// A fork block well after Pancake V3 launched on Ethereum.
const ETH_FORK_BLOCK: u64 = 20_000_000;

// --- Slipstream / Aerodrome (Base) ---
// Aerodrome Slipstream CLFactory on Base (verified on-chain: owner()/getPool
// respond; the previously-listed 0xeC8E5342… address reverts every call).
const SLIPSTREAM_FACTORY: Address = address!("5e7BB104d84c7CB9B682AaC2F3d509f5F406809A");
const BASE_WETH: Address = address!("4200000000000000000000000000000000000006");
const BASE_USDC: Address = address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913");
const BASE_FORK_BLOCK: u64 = 47_700_000;
// Aerodrome deploys WETH/USDC across several tick spacings; CL100 is the
// canonical volatile pool. The parity test scans the preset's spacing table and
// requires at least one to resolve.
const SLIPSTREAM_WETH_USDC_SPACING: i32 = 100;

fn eth_rpc_url() -> Option<String> {
    std::env::var("E2E_RPC_URL").ok()
}

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

async fn eth_call_addr(
    provider: &RootProvider<AnyNetwork>,
    target: Address,
    calldata: Bytes,
    block: u64,
) -> Result<Address> {
    let tx = TransactionRequest::default()
        .with_to(target)
        .with_input(calldata);
    let out = provider
        .call(tx.into())
        .block(BlockId::Number(BlockNumberOrTag::Number(block)))
        .await
        .context("eth_call")?;
    // A returned address is the low 20 bytes of the 32-byte word.
    let word = U256::from_be_slice(&out);
    Ok(Address::from_slice(&word.to_be_bytes::<32>()[12..]))
}

/// Read the pool address the factory's `getPool` STORAGE slot holds, via
/// `eth_getStorageAt` at the pinned block — independent of the getter.
async fn storage_addr(
    provider: &RootProvider<AnyNetwork>,
    factory: Address,
    slot: U256,
    block: u64,
) -> Result<Address> {
    let word = provider
        .get_storage_at(factory, slot)
        .block_id(BlockId::Number(BlockNumberOrTag::Number(block)))
        .await
        .context("eth_getStorageAt")?;
    Ok(Address::from_slice(&word.to_be_bytes::<32>()[12..]))
}

/// Pancake V3: the fee-keyed `getPool` base slot (preset slot 2 — Pancake's own
/// layout, not the Uniswap 5) must hold the same pool the factory's
/// `getPool(t0,t1,fee)` getter returns — confirming the shipped
/// `ClFactorySpec::pancake_v3` constant, which already runs with
/// `verify_derivations` on.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires E2E_RPC_URL archive node; run with --ignored"]
async fn pancake_get_pool_base_slot_matches_getter() -> Result<()> {
    let Some(url) = eth_rpc_url() else {
        eprintln!("E2E_RPC_URL unset; skipping");
        return Ok(());
    };
    let provider = provider(&url).await?;
    let spec = ClFactorySpec::pancake_v3(PANCAKE_V3_FACTORY);
    let (t0, t1) = derive::sort_tokens(ETH_USDC, ETH_WETH);

    // Find a fee tier for which the getter returns a real pool.
    let mut checked = 0usize;
    for &fee in &[100u32, 500, 2_500, 10_000] {
        let getter = eth_call_addr(
            &provider,
            PANCAKE_V3_FACTORY,
            Bytes::from(
                getPoolCall {
                    tokenA: t0,
                    tokenB: t1,
                    fee: alloy_primitives::aliases::U24::from(fee),
                }
                .abi_encode(),
            ),
            ETH_FORK_BLOCK,
        )
        .await?;
        if getter == Address::ZERO {
            continue;
        }
        checked += 1;
        let slot = derive::v3_get_pool_slot(spec.get_pool_base_slot, t0, t1, fee);
        let from_storage =
            storage_addr(&provider, PANCAKE_V3_FACTORY, slot, ETH_FORK_BLOCK).await?;
        assert_eq!(
            from_storage, getter,
            "Pancake getPool base slot {} is WRONG for fee {fee}: storage={from_storage:?} getter={getter:?}. \
             The shipped ClFactorySpec::pancake_v3 get_pool_base_slot has regressed.",
            spec.get_pool_base_slot
        );
    }
    assert!(
        checked > 0,
        "no Pancake USDC/WETH pool found at block {ETH_FORK_BLOCK}; pick a live pair"
    );
    Ok(())
}

/// Pancake V3: the pinned CREATE2 deployer + init-code hash must reproduce the
/// real pool address (independent of the storage layout).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires E2E_RPC_URL archive node; run with --ignored"]
async fn pancake_create2_matches_getter() -> Result<()> {
    let Some(url) = eth_rpc_url() else {
        eprintln!("E2E_RPC_URL unset; skipping");
        return Ok(());
    };
    let provider = provider(&url).await?;
    let spec = ClFactorySpec::pancake_v3(PANCAKE_V3_FACTORY);
    let create2 = spec.create2.expect("pancake preset pins create2");
    let deployer = create2.deployer.expect("pancake preset pins a deployer");
    let (t0, t1) = derive::sort_tokens(ETH_USDC, ETH_WETH);

    let mut checked = 0usize;
    for &fee in &[100u32, 500, 2_500, 10_000] {
        let getter = eth_call_addr(
            &provider,
            PANCAKE_V3_FACTORY,
            Bytes::from(
                getPoolCall {
                    tokenA: t0,
                    tokenB: t1,
                    fee: alloy_primitives::aliases::U24::from(fee),
                }
                .abi_encode(),
            ),
            ETH_FORK_BLOCK,
        )
        .await?;
        if getter == Address::ZERO {
            continue;
        }
        checked += 1;
        let derived = derive::v3_pool_address(deployer, create2.init_code_hash, t0, t1, fee);
        assert_eq!(
            derived, getter,
            "Pancake CREATE2 (deployer {deployer:?}, init hash {:?}) does not reproduce the \
             getter pool for fee {fee}: derived={derived:?} getter={getter:?}.",
            create2.init_code_hash
        );
    }
    assert!(
        checked > 0,
        "no Pancake USDC/WETH pool found to cross-check"
    );
    Ok(())
}

/// Pancake V3 end-to-end: the preset resolves the live USDC/WETH pool through the
/// batched discovery path (proves both the base slot and the driver wiring).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires E2E_RPC_URL archive node; run with --ignored"]
async fn pancake_discovery_resolves_live_pool() -> Result<()> {
    let Some(url) = eth_rpc_url() else {
        eprintln!("E2E_RPC_URL unset; skipping");
        return Ok(());
    };
    let mut cache = EvmCache::at_block(
        provider(&url).await?,
        BlockId::Number(BlockNumberOrTag::Number(ETH_FORK_BLOCK)),
    )
    .await;

    let discovery = PoolDiscovery::new([Box::new(ConcentratedLiquidityFactory::new(
        ClFactorySpec::pancake_v3(PANCAKE_V3_FACTORY),
    )) as Box<dyn PoolFactory>]);
    let found = discovery.find(
        &mut cache,
        PoolQuery::pair(ETH_USDC, ETH_WETH).on(ProtocolId::PancakeV3),
    )?;
    assert!(
        !found.is_empty(),
        "Pancake preset found no USDC/WETH pool — likely a wrong get_pool_base_slot"
    );
    for pool in &found {
        assert!(
            matches!(pool.key, evm_amm_state::adapters::PoolKey::PancakeV3(_)),
            "discovered pool must be keyed PancakeV3"
        );
    }
    Ok(())
}

/// Slipstream (Base): the tickSpacing-keyed `getPool` base slot + spacing salt
/// must hold the same pool Aerodrome's `getPool(t0,t1,spacing)` getter returns.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Base RPC (E2E_BASE_RPC_URL / E2E_RPC_URL on Alchemy); run with --ignored"]
async fn slipstream_get_pool_base_slot_matches_getter() -> Result<()> {
    let Some(url) = base_rpc_url() else {
        eprintln!("no Base RPC; skipping");
        return Ok(());
    };
    let provider = provider(&url).await?;
    let spec = ClFactorySpec::slipstream(SLIPSTREAM_FACTORY);
    let (t0, t1) = derive::sort_tokens(BASE_WETH, BASE_USDC);

    let getter = eth_call_addr(
        &provider,
        SLIPSTREAM_FACTORY,
        Bytes::from(
            slipstream_abi::getPoolCall {
                tokenA: t0,
                tokenB: t1,
                tickSpacing: I24::try_from(SLIPSTREAM_WETH_USDC_SPACING).unwrap(),
            }
            .abi_encode(),
        ),
        BASE_FORK_BLOCK,
    )
    .await?;
    assert_ne!(
        getter,
        Address::ZERO,
        "no Aerodrome WETH/USDC CL{SLIPSTREAM_WETH_USDC_SPACING} pool at block {BASE_FORK_BLOCK}"
    );

    let slot = derive::v3_get_pool_slot_by_spacing(
        spec.get_pool_base_slot,
        t0,
        t1,
        SLIPSTREAM_WETH_USDC_SPACING,
    );
    let from_storage = storage_addr(&provider, SLIPSTREAM_FACTORY, slot, BASE_FORK_BLOCK).await?;
    assert_eq!(
        from_storage, getter,
        "Slipstream getPool base slot {} (spacing {SLIPSTREAM_WETH_USDC_SPACING}) is WRONG: \
         storage={from_storage:?} getter={getter:?}. Fix ClFactorySpec::slipstream's \
         get_pool_base_slot.",
        spec.get_pool_base_slot
    );
    Ok(())
}
