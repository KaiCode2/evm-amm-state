//! WS2 RPC parity test (env-gated, `#[ignore]`) — MANAGER RUNS THIS.
//!
//! Given `E2E_RPC_URL` (an archive node), this forks mainnet at a PINNED block,
//! cold-starts a known pool, runs `simulate_swap`, and asserts the result equals
//! the SAME quote executed via the provider's `eth_call` at the same block (the
//! on-chain ground truth). An exact match is expected: identical bytecode +
//! identical state at the pinned height.
//!
//! Not run in CI (no network). Build-checked via
//! `cargo build --tests --test adapter_swap_sim_rpc`. To run:
//! ```text
//! E2E_RPC_URL=<archive-url> cargo test --test adapter_swap_sim_rpc -- --ignored
//! ```
//!
//! ## Pinned block + pool addresses
//!
//! - Fork block: **20_000_000** (Ethereum mainnet, 2024-05-21). All pools below
//!   were deployed and active well before this height.
//! - Uniswap V3: USDC/WETH 0.05% pool `0x88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640`
//!   (fee 500, tick spacing 10). QuoterV2 `0x61fFE014bA17989E743c5F6cB21bF9697530B21e`.
//! - Uniswap V2: USDC/WETH pair `0xB4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc`.
//!   Router02 `0x7a250d5630B4cF539739dF2C5dAcb4c659F2488D`.
//! - Balancer V2: 80BAL/20WETH weighted pool, poolId
//!   `0x5c6ee304399dbdb9c8ef030ab642b10820db8f56000200000000000000000014`,
//!   vault `0xBA12222222228d8Ba445958a75a0704d566BF2C8`.
//! - Token amounts use a 1e6 USDC / 1e18 WETH-scaled input as noted per test.
//!
//! The Solidly V2 parity test forks **Base** (not Ethereum), block
//! `47_700_000`, against the Aerodrome WETH/USDC volatile pool
//! `0xcDAC0d6c6C59727a65F871236188350531885C43`. It uses a Base RPC url —
//! `E2E_BASE_RPC_URL`, or `E2E_RPC_URL` with the Alchemy `eth-mainnet` host
//! swapped to `base-mainnet`:
//! ```text
//! E2E_RPC_URL=<alchemy-archive-url> cargo test --test adapter_swap_sim_rpc -- --ignored
//! ```

use std::sync::Arc;

use alloy_eips::{BlockId, BlockNumberOrTag};
use alloy_network::{AnyNetwork, TransactionBuilder};
use alloy_primitives::{Address, B256, Bytes, U256, address, b256};
use alloy_provider::{Provider, RootProvider};
use alloy_rpc_types_eth::TransactionRequest;
use alloy_sol_types::SolCall;
use anyhow::{Context, Result, anyhow};

use evm_amm_state::adapters::sim::{
    BatchSwapStep, CurveCryptoSwap, FundManagement, QuoteExactInputSingleParams, get_dyCall,
    getAmountOutCall, getAmountsOutCall, queryBatchSwapCall, quoteExactInputSingleCall,
};
use evm_amm_state::adapters::storage::SolidlyStorageLayout;
use evm_amm_state::adapters::{
    AdapterRegistry, AmmAdapter, BalancerV2Adapter, BalancerV2Metadata, ColdStartPolicy,
    CurveAdapter, CurveMetadata, CurveVariant, PoolKey, PoolRegistration, ProtocolMetadata,
    SimConfig, SolidlyV2Adapter, SolidlyV2Metadata, UniswapV2Adapter, UniswapV2Metadata,
    UniswapV3Adapter, V3Metadata,
};
use evm_fork_cache::cache::EvmCache;

alloy_sol_types::sol! {
    /// Solidly pool reserve view fns — used only to cross-check the empirical
    /// storage-slot layout against the live pool's authoritative reserves.
    function reserve0() returns (uint256);
    function reserve1() returns (uint256);
}

const FORK_BLOCK: u64 = 20_000_000;

const USDC: Address = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
const WETH: Address = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");

const V3_QUOTER_V2: Address = address!("61fFE014bA17989E743c5F6cB21bF9697530B21e");
const V3_USDC_WETH_005: Address = address!("88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640");

const V2_ROUTER_02: Address = address!("7a250d5630B4cF539739dF2C5dAcb4c659F2488D");
const V2_USDC_WETH_PAIR: Address = address!("B4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc");

const BALANCER_VAULT: Address = address!("BA12222222228d8Ba445958a75a0704d566BF2C8");
const BALANCER_BAL_WETH_POOL_ID: B256 =
    b256!("5c6ee304399dbdb9c8ef030ab642b10820db8f56000200000000000000000014");
const BAL: Address = address!("ba100000625a3754423978a60c9317c58a424e3D");

// --- Curve StableSwap (3pool on Ethereum mainnet, at FORK_BLOCK) ---
const CURVE_3POOL: Address = address!("bEbc44782C7dB0a1A60Cb6fe97d0b483032FF1C7");
const DAI: Address = address!("6B175474E89094C44Da98b954EedeAC495271d0F");
const USDT: Address = address!("dAC17F958D2ee523a2206206994597C13D831ec7");

// --- Curve CryptoSwap / Curve v2 (tricrypto2 on Ethereum mainnet, at FORK_BLOCK) ---
// USDT/WBTC/WETH; get_dy uses uint256 indices (int128 reverts on this pool).
const TRICRYPTO2: Address = address!("D51a44d3FaE010294C616388b506AcdA1bfAAE46");
const WBTC: Address = address!("2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599");

// --- Solidly V2 (Aerodrome on Base) ---
//
// Base mainnet fork block. Aerodrome WETH/USDC volatile pool, discovered from
// the PoolFactory and verified at this height (see the empirical layout scan
// baked into `AERO_*_SLOT` below).
const SOLIDLY_FORK_BLOCK: u64 = 47_700_000;
const BASE_WETH: Address = address!("4200000000000000000000000000000000000006");
const BASE_USDC: Address = address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913");
// factory.getPool(WETH, USDC, stable=false) at the fork block.
const AERODROME_WETH_USDC: Address = address!("cDAC0d6c6C59727a65F871236188350531885C43");
// Storage layout verified empirically at the fork block by matching
// eth_getStorageAt against the pool's token0()/token1()/reserve0()/reserve1():
// token0 -> slot 13, token1 -> slot 14, reserve0 -> slot 20, reserve1 -> slot 21.
const AERO_RESERVE0_SLOT: u64 = 20;
const AERO_RESERVE1_SLOT: u64 = 21;
const AERO_TOKEN0_SLOT: u64 = 13;
const AERO_TOKEN1_SLOT: u64 = 14;

fn rpc_url() -> Option<String> {
    std::env::var("E2E_RPC_URL").ok()
}

/// Base RPC url for the Solidly parity test: an explicit `E2E_BASE_RPC_URL` if
/// set, otherwise derived from `E2E_RPC_URL` by swapping the Alchemy
/// `eth-mainnet` host segment for `base-mainnet` (Aerodrome lives on Base, not
/// Ethereum mainnet). Returns `None` if neither is available.
fn base_rpc_url() -> Option<String> {
    if let Ok(url) = std::env::var("E2E_BASE_RPC_URL") {
        return Some(url);
    }
    std::env::var("E2E_RPC_URL")
        .ok()
        .map(|url| url.replace("eth-mainnet", "base-mainnet"))
}

async fn fork_cache(url: &str, block: u64) -> Result<EvmCache> {
    let provider = RootProvider::<AnyNetwork>::connect(url)
        .await
        .context("connect RPC url")?;
    Ok(EvmCache::at_block(
        Arc::new(provider),
        BlockId::Number(BlockNumberOrTag::Number(block)),
    )
    .await)
}

/// Execute `calldata` against `target` via the provider's `eth_call` at the
/// pinned `block` — the on-chain ground truth.
async fn eth_call_at(url: &str, target: Address, calldata: Bytes, block: u64) -> Result<Bytes> {
    let provider = RootProvider::<AnyNetwork>::connect(url).await?;
    let tx = TransactionRequest::default()
        .with_to(target)
        .with_input(calldata);
    let out = provider
        .call(tx.into())
        .block(BlockId::Number(BlockNumberOrTag::Number(block)))
        .await
        .context("eth_call at fork block")?;
    Ok(out)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires E2E_RPC_URL archive node; run with --ignored"]
async fn v3_simulate_swap_matches_eth_call() -> Result<()> {
    let Some(url) = rpc_url() else {
        eprintln!("E2E_RPC_URL unset; skipping");
        return Ok(());
    };

    // 1 USDC in (6 decimals).
    let amount_in = U256::from(1_000_000_u64);

    let mut cache = fork_cache(&url, FORK_BLOCK).await?;
    let registry = {
        let mut r = AdapterRegistry::new();
        r.register_adapter(Arc::new(UniswapV3Adapter::default()))?;
        r
    };
    let mut registration = PoolRegistration::new(PoolKey::UniswapV3(V3_USDC_WETH_005))
        .with_state_address(V3_USDC_WETH_005)
        .with_metadata(ProtocolMetadata::UniswapV3(V3Metadata {
            token0: Some(USDC),
            token1: Some(WETH),
            fee: Some(500),
            tick_spacing: Some(10),
            storage_layout: Some(evm_amm_state::adapters::storage::V3StorageLayout::uniswap(
                10,
            )),
        }));
    registry.cold_start(&mut registration, &mut cache, ColdStartPolicy::Eager)?;

    let config = SimConfig::default().with_v3_quoter(V3_QUOTER_V2);
    let adapter = UniswapV3Adapter::default();
    let sim = adapter
        .simulate_swap(&registration, &mut cache, USDC, WETH, amount_in, &config)
        .map_err(|e| anyhow!("v3 sim failed: {e}"))?;

    // Ground truth: the SAME QuoterV2 call via eth_call at the fork block.
    let calldata = Bytes::from(
        quoteExactInputSingleCall {
            params: QuoteExactInputSingleParams {
                tokenIn: USDC,
                tokenOut: WETH,
                amountIn: amount_in,
                fee: alloy_primitives::aliases::U24::from(500u32),
                sqrtPriceLimitX96: U256::ZERO.to(),
            },
        }
        .abi_encode(),
    );
    let out = eth_call_at(&url, V3_QUOTER_V2, calldata, FORK_BLOCK).await?;
    let truth = quoteExactInputSingleCall::abi_decode_returns_validate(&out)?;

    assert_eq!(
        sim.amount_out, truth.amountOut,
        "V3 sim amount_out must match eth_call QuoterV2"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires E2E_RPC_URL archive node; run with --ignored"]
async fn v2_simulate_swap_matches_eth_call() -> Result<()> {
    let Some(url) = rpc_url() else {
        eprintln!("E2E_RPC_URL unset; skipping");
        return Ok(());
    };

    let amount_in = U256::from(1_000_000_u64); // 1 USDC

    let mut cache = fork_cache(&url, FORK_BLOCK).await?;
    let registry = {
        let mut r = AdapterRegistry::new();
        r.register_adapter(Arc::new(UniswapV2Adapter::default()))?;
        r
    };
    let mut registration = PoolRegistration::new(PoolKey::UniswapV2(V2_USDC_WETH_PAIR))
        .with_state_address(V2_USDC_WETH_PAIR)
        .with_metadata(ProtocolMetadata::UniswapV2(UniswapV2Metadata {
            token0: Some(USDC),
            token1: Some(WETH),
            fee_bps: Some(30),
        }));
    registry.cold_start(&mut registration, &mut cache, ColdStartPolicy::Eager)?;

    let config = SimConfig::default().with_v2_router(V2_ROUTER_02);
    let adapter = UniswapV2Adapter::default();
    let sim = adapter
        .simulate_swap(&registration, &mut cache, USDC, WETH, amount_in, &config)
        .map_err(|e| anyhow!("v2 sim failed: {e}"))?;

    let calldata = Bytes::from(
        getAmountsOutCall {
            amountIn: amount_in,
            path: vec![USDC, WETH],
        }
        .abi_encode(),
    );
    let out = eth_call_at(&url, V2_ROUTER_02, calldata, FORK_BLOCK).await?;
    let amounts = getAmountsOutCall::abi_decode_returns_validate(&out)?;

    assert_eq!(
        sim.amount_out,
        *amounts.last().expect("non-empty amounts"),
        "V2 sim amount_out must match eth_call Router02 getAmountsOut"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires E2E_RPC_URL archive node; run with --ignored"]
async fn balancer_simulate_swap_matches_eth_call() -> Result<()> {
    let Some(url) = rpc_url() else {
        eprintln!("E2E_RPC_URL unset; skipping");
        return Ok(());
    };

    let amount_in = U256::from(1_000_000_000_000_000_000_u64); // 1 BAL (18 decimals)

    let mut cache = fork_cache(&url, FORK_BLOCK).await?;
    let registry = {
        let mut r = AdapterRegistry::new();
        r.register_adapter(Arc::new(BalancerV2Adapter::default()))?;
        r
    };
    let mut registration = PoolRegistration::new(PoolKey::BalancerV2(BALANCER_BAL_WETH_POOL_ID))
        .with_state_address(BALANCER_VAULT)
        .with_metadata(ProtocolMetadata::BalancerV2(BalancerV2Metadata {
            vault: Some(BALANCER_VAULT),
            ..Default::default()
        }));
    registry.cold_start(&mut registration, &mut cache, ColdStartPolicy::Eager)?;

    let config = SimConfig::default();
    let adapter = BalancerV2Adapter::default();
    let sim = adapter
        .simulate_swap(&registration, &mut cache, BAL, WETH, amount_in, &config)
        .map_err(|e| anyhow!("balancer sim failed: {e}"))?;

    let calldata = Bytes::from(
        queryBatchSwapCall {
            kind: 0,
            swaps: vec![BatchSwapStep {
                poolId: BALANCER_BAL_WETH_POOL_ID,
                assetInIndex: U256::ZERO,
                assetOutIndex: U256::from(1),
                amount: amount_in,
                userData: Bytes::new(),
            }],
            assets: vec![BAL, WETH],
            funds: FundManagement {
                sender: Address::ZERO,
                fromInternalBalance: false,
                recipient: Address::ZERO,
                toInternalBalance: false,
            },
        }
        .abi_encode(),
    );
    let out = eth_call_at(&url, BALANCER_VAULT, calldata, FORK_BLOCK).await?;
    let deltas = queryBatchSwapCall::abi_decode_returns_validate(&out)?;
    let truth_out = U256::from(deltas[1].unsigned_abs());

    assert_eq!(
        sim.amount_out, truth_out,
        "Balancer sim amount_out must match eth_call Vault queryBatchSwap"
    );
    Ok(())
}

/// Solidly V2 (Aerodrome on Base) parity — forks Base at a pinned block,
/// cold-starts a real Aerodrome WETH/USDC volatile pool, and asserts:
///   1. cold-start decodes the real `token0`/`token1` from the configured token
///      slots (proves `AERO_TOKEN0_SLOT`/`AERO_TOKEN1_SLOT`),
///   2. the configured reserve slots hold the pool's authoritative
///      `reserve0()`/`reserve1()` (proves `AERO_RESERVE0_SLOT`/`AERO_RESERVE1_SLOT`),
///   3. `simulate_swap` (the pool's `getAmountOut`) equals the SAME call via
///      `eth_call` at the fork block (the on-chain ground truth).
///
/// Together these validate the real `getAmountOut`/`Sync` ABIs *and* the
/// `SolidlyStorageLayout` against a live deployment — the thing the offline
/// `sload(0)` mock cannot exercise.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Base RPC (E2E_BASE_RPC_URL, or E2E_RPC_URL on Alchemy); run with --ignored"]
async fn solidly_simulate_swap_matches_eth_call() -> Result<()> {
    let Some(url) = base_rpc_url() else {
        eprintln!("no Base RPC (E2E_BASE_RPC_URL / E2E_RPC_URL); skipping");
        return Ok(());
    };

    // 0.001 WETH in (18 decimals); WETH -> USDC.
    let amount_in = U256::from(1_000_000_000_000_000_u64);

    let layout = SolidlyStorageLayout::new(
        U256::from(AERO_RESERVE0_SLOT),
        U256::from(AERO_RESERVE1_SLOT),
        U256::from(AERO_TOKEN0_SLOT),
        U256::from(AERO_TOKEN1_SLOT),
    );

    let mut cache = fork_cache(&url, SOLIDLY_FORK_BLOCK).await?;
    let registry = {
        let mut r = AdapterRegistry::new();
        r.register_adapter(Arc::new(SolidlyV2Adapter::default()))?;
        r
    };
    let mut registration = PoolRegistration::new(PoolKey::SolidlyV2(AERODROME_WETH_USDC))
        .with_state_address(AERODROME_WETH_USDC)
        .with_metadata(ProtocolMetadata::SolidlyV2(SolidlyV2Metadata {
            token0: None,
            token1: None,
            stable: Some(false),
            storage_layout: Some(layout),
        }));
    registry.cold_start(&mut registration, &mut cache, ColdStartPolicy::Eager)?;

    // (1) Cold-start decoded the real tokens from the configured token slots.
    let ProtocolMetadata::SolidlyV2(meta) = &registration.metadata else {
        return Err(anyhow!("expected Solidly metadata after cold-start"));
    };
    assert_eq!(
        meta.token0,
        Some(BASE_WETH),
        "token0 decoded from slot {AERO_TOKEN0_SLOT} must be WETH"
    );
    assert_eq!(
        meta.token1,
        Some(BASE_USDC),
        "token1 decoded from slot {AERO_TOKEN1_SLOT} must be USDC"
    );

    // (2) The configured reserve slots hold the pool's authoritative reserves.
    let provider = RootProvider::<AnyNetwork>::connect(&url).await?;
    let bid = BlockId::Number(BlockNumberOrTag::Number(SOLIDLY_FORK_BLOCK));
    let slot0 = provider
        .get_storage_at(AERODROME_WETH_USDC, U256::from(AERO_RESERVE0_SLOT))
        .block_id(bid)
        .await?;
    let slot1 = provider
        .get_storage_at(AERODROME_WETH_USDC, U256::from(AERO_RESERVE1_SLOT))
        .block_id(bid)
        .await?;
    let r0 = reserve0Call::abi_decode_returns_validate(
        &eth_call_at(
            &url,
            AERODROME_WETH_USDC,
            Bytes::from(reserve0Call {}.abi_encode()),
            SOLIDLY_FORK_BLOCK,
        )
        .await?,
    )?;
    let r1 = reserve1Call::abi_decode_returns_validate(
        &eth_call_at(
            &url,
            AERODROME_WETH_USDC,
            Bytes::from(reserve1Call {}.abi_encode()),
            SOLIDLY_FORK_BLOCK,
        )
        .await?,
    )?;
    assert_eq!(slot0, r0, "slot {AERO_RESERVE0_SLOT} must hold reserve0()");
    assert_eq!(slot1, r1, "slot {AERO_RESERVE1_SLOT} must hold reserve1()");

    // (3) simulate_swap == eth_call getAmountOut ground truth.
    let config = SimConfig::default();
    let adapter = SolidlyV2Adapter::default();
    let sim = adapter
        .simulate_swap(
            &registration,
            &mut cache,
            BASE_WETH,
            BASE_USDC,
            amount_in,
            &config,
        )
        .map_err(|e| anyhow!("solidly sim failed: {e}"))?;

    let out = eth_call_at(
        &url,
        AERODROME_WETH_USDC,
        Bytes::from(
            getAmountOutCall {
                amountIn: amount_in,
                tokenIn: BASE_WETH,
            }
            .abi_encode(),
        ),
        SOLIDLY_FORK_BLOCK,
    )
    .await?;
    let truth = getAmountOutCall::abi_decode_returns_validate(&out)?;

    assert!(truth > U256::ZERO, "ground-truth quote should be non-zero");
    assert_eq!(
        sim.amount_out, truth,
        "Solidly sim amount_out must match eth_call getAmountOut"
    );
    Ok(())
}

/// Curve StableSwap parity — forks mainnet at FORK_BLOCK, cold-starts the live
/// 3pool (DAI/USDC/USDT), and asserts:
///   1. cold-start discovered + persisted a non-empty `get_dy` read-set
///      (`discovered_slots`),
///   2. `simulate_swap(DAI, USDC, 1 DAI)` (the pool's `get_dy`) equals the SAME
///      call via `eth_call` at the fork block (on-chain ground truth).
///
/// Validates the real `get_dy` ABI + the discover-based cold-start against a
/// live deployment (probe ground truth at this block: 999900).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires E2E_RPC_URL archive node; run with --ignored"]
async fn curve_simulate_swap_matches_eth_call() -> Result<()> {
    let Some(url) = rpc_url() else {
        eprintln!("E2E_RPC_URL unset; skipping");
        return Ok(());
    };

    // 1 DAI in (18 decimals); DAI (coin 0) -> USDC (coin 1).
    let amount_in = U256::from(1_000_000_000_000_000_000_u128);

    let mut cache = fork_cache(&url, FORK_BLOCK).await?;
    let registry = {
        let mut r = AdapterRegistry::new();
        r.register_adapter(Arc::new(CurveAdapter::default()))?;
        r
    };
    let mut registration = PoolRegistration::new(PoolKey::Curve(CURVE_3POOL))
        .with_state_address(CURVE_3POOL)
        .with_metadata(ProtocolMetadata::Curve(CurveMetadata {
            coins: vec![DAI, USDC, USDT],
            discovered_slots: Vec::new(),
            variant: CurveVariant::StableSwap,
        }));
    registry.cold_start(&mut registration, &mut cache, ColdStartPolicy::Eager)?;

    // (1) Cold-start discovered + persisted the get_dy read-set.
    let ProtocolMetadata::Curve(meta) = &registration.metadata else {
        return Err(anyhow!("expected Curve metadata after cold-start"));
    };
    assert!(
        !meta.discovered_slots.is_empty(),
        "cold-start should discover the get_dy read-set"
    );
    assert_eq!(meta.coins, vec![DAI, USDC, USDT], "coins preserved");

    // (2) simulate_swap == eth_call get_dy ground truth.
    let adapter = CurveAdapter::default();
    let sim = adapter
        .simulate_swap(
            &registration,
            &mut cache,
            DAI,
            USDC,
            amount_in,
            &SimConfig::default(),
        )
        .map_err(|e| anyhow!("curve sim failed: {e}"))?;

    let out = eth_call_at(
        &url,
        CURVE_3POOL,
        Bytes::from(
            get_dyCall {
                i: 0i128,
                j: 1i128,
                dx: amount_in,
            }
            .abi_encode(),
        ),
        FORK_BLOCK,
    )
    .await?;
    let truth = get_dyCall::abi_decode_returns_validate(&out)?;

    assert!(truth > U256::ZERO, "ground-truth quote should be non-zero");
    assert_eq!(
        sim.amount_out, truth,
        "Curve sim amount_out must match eth_call get_dy"
    );
    Ok(())
}

/// Curve CryptoSwap (Curve v2) parity — forks mainnet at FORK_BLOCK, cold-starts
/// the live tricrypto2 pool (USDT/WBTC/WETH, uint256-index `get_dy`), and asserts:
///   1. cold-start discovered + persisted a non-empty `get_dy` read-set
///      (`discovered_slots`) and preserved `variant: CryptoSwap`,
///   2. `simulate_swap(USDT, WBTC, 100e6)` (the pool's uint256-index `get_dy`)
///      equals the SAME call via `eth_call` at the fork block (on-chain ground
///      truth). Probe ground truth at this block: 147348 (WBTC sats).
///
/// Validates the real CryptoSwap (uint256-index) `get_dy` ABI + the variant-aware
/// discover cold-start against a live deployment — the thing the offline
/// selector-agnostic mock cannot exercise (it cannot distinguish ABIs).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires E2E_RPC_URL archive node; run with --ignored"]
async fn curve_cryptoswap_simulate_swap_matches_eth_call() -> Result<()> {
    let Some(url) = rpc_url() else {
        eprintln!("E2E_RPC_URL unset; skipping");
        return Ok(());
    };

    // 100 USDT in (6 decimals); USDT (coin 0) -> WBTC (coin 1).
    let amount_in = U256::from(100_000_000_u64);

    let mut cache = fork_cache(&url, FORK_BLOCK).await?;
    let registry = {
        let mut r = AdapterRegistry::new();
        r.register_adapter(Arc::new(CurveAdapter::default()))?;
        r
    };
    let mut registration = PoolRegistration::new(PoolKey::Curve(TRICRYPTO2))
        .with_state_address(TRICRYPTO2)
        .with_metadata(ProtocolMetadata::Curve(CurveMetadata {
            coins: vec![USDT, WBTC, WETH],
            discovered_slots: Vec::new(),
            variant: CurveVariant::CryptoSwap,
        }));
    registry.cold_start(&mut registration, &mut cache, ColdStartPolicy::Eager)?;

    // (1) Cold-start discovered + persisted the get_dy read-set and the variant.
    let ProtocolMetadata::Curve(meta) = &registration.metadata else {
        return Err(anyhow!("expected Curve metadata after cold-start"));
    };
    assert!(
        !meta.discovered_slots.is_empty(),
        "cold-start should discover the CryptoSwap get_dy read-set"
    );
    assert_eq!(
        meta.variant,
        CurveVariant::CryptoSwap,
        "cold-start must preserve the CryptoSwap variant"
    );
    assert_eq!(meta.coins, vec![USDT, WBTC, WETH], "coins preserved");

    // (2) simulate_swap == eth_call get_dy(uint256,uint256,uint256) ground truth.
    let adapter = CurveAdapter::default();
    let sim = adapter
        .simulate_swap(
            &registration,
            &mut cache,
            USDT,
            WBTC,
            amount_in,
            &SimConfig::default(),
        )
        .map_err(|e| anyhow!("curve cryptoswap sim failed: {e}"))?;

    let out = eth_call_at(
        &url,
        TRICRYPTO2,
        Bytes::from(
            CurveCryptoSwap::get_dyCall {
                i: U256::ZERO,
                j: U256::from(1),
                dx: amount_in,
            }
            .abi_encode(),
        ),
        FORK_BLOCK,
    )
    .await?;
    let truth = CurveCryptoSwap::get_dyCall::abi_decode_returns_validate(&out)?;

    assert!(truth > U256::ZERO, "ground-truth quote should be non-zero");
    assert_eq!(
        sim.amount_out, truth,
        "Curve CryptoSwap sim amount_out must match eth_call get_dy"
    );
    Ok(())
}
