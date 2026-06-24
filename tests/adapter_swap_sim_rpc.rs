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

use std::sync::Arc;

use alloy_eips::{BlockId, BlockNumberOrTag};
use alloy_network::{AnyNetwork, TransactionBuilder};
use alloy_primitives::{Address, B256, Bytes, U256, address, b256};
use alloy_provider::{Provider, RootProvider};
use alloy_rpc_types_eth::TransactionRequest;
use alloy_sol_types::SolCall;
use anyhow::{Context, Result, anyhow};

use evm_amm_state::adapters::sim::{
    BatchSwapStep, FundManagement, QuoteExactInputSingleParams, getAmountsOutCall,
    queryBatchSwapCall, quoteExactInputSingleCall,
};
use evm_amm_state::adapters::{
    AdapterRegistry, AmmAdapter, BalancerV2Adapter, BalancerV2Metadata, ColdStartPolicy, PoolKey,
    PoolRegistration, ProtocolMetadata, SimConfig, UniswapV2Adapter, UniswapV2Metadata,
    UniswapV3Adapter, V3Metadata,
};
use evm_fork_cache::cache::EvmCache;

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

fn rpc_url() -> Option<String> {
    std::env::var("E2E_RPC_URL").ok()
}

async fn fork_cache(url: &str) -> Result<EvmCache> {
    let provider = RootProvider::<AnyNetwork>::connect(url)
        .await
        .context("connect E2E_RPC_URL")?;
    Ok(EvmCache::at_block(
        Arc::new(provider),
        BlockId::Number(BlockNumberOrTag::Number(FORK_BLOCK)),
    )
    .await)
}

/// Execute `calldata` against `target` via the provider's `eth_call` at the
/// pinned fork block — the on-chain ground truth.
async fn eth_call_at_fork(url: &str, target: Address, calldata: Bytes) -> Result<Bytes> {
    let provider = RootProvider::<AnyNetwork>::connect(url).await?;
    let tx = TransactionRequest::default()
        .with_to(target)
        .with_input(calldata);
    let out = provider
        .call(tx.into())
        .block(BlockId::Number(BlockNumberOrTag::Number(FORK_BLOCK)))
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

    let mut cache = fork_cache(&url).await?;
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
    let out = eth_call_at_fork(&url, V3_QUOTER_V2, calldata).await?;
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

    let mut cache = fork_cache(&url).await?;
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
    let out = eth_call_at_fork(&url, V2_ROUTER_02, calldata).await?;
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

    let mut cache = fork_cache(&url).await?;
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
    let out = eth_call_at_fork(&url, BALANCER_VAULT, calldata).await?;
    let deltas = queryBatchSwapCall::abi_decode_returns_validate(&out)?;
    let truth_out = U256::from(deltas[1].unsigned_abs());

    assert_eq!(
        sim.amount_out, truth_out,
        "Balancer sim amount_out must match eth_call Vault queryBatchSwap"
    );
    Ok(())
}
