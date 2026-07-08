//! Manually seed known AMM runtime bytecodes into `EvmCache` and verify them.
//!
//! Run with:
//! E2E_RPC_URL=<https-mainnet-rpc> cargo run --example verified_bytecode_seed

use std::sync::Arc;

use alloy_eips::{BlockId, BlockNumberOrTag};
use alloy_network::AnyNetwork;
use alloy_primitives::address;
use alloy_provider::{Provider, RootProvider};
use anyhow::{Context, Result, bail, ensure};
use evm_amm_state::adapters::{
    V3ImmutablePatchValues, uniswap_v2_pair_code_seed, uniswap_v3_code_seed,
    uniswap_v3_max_liquidity_per_tick,
};
use evm_fork_cache::cache::{CodeSeedState, CodeVerifyReport, EvmCache};

const UNISWAP_V2_USDC_WETH: alloy_primitives::Address =
    address!("B4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc");
const UNISWAP_V3_USDC_WETH_500: alloy_primitives::Address =
    address!("88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640");
const CANONICAL_UNISWAP_V3_FACTORY: alloy_primitives::Address =
    address!("1F98431c8aD98523631AE4a59f267346ea31F984");
const USDC: alloy_primitives::Address = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
const WETH: alloy_primitives::Address = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let Ok(url) = std::env::var("E2E_RPC_URL") else {
        eprintln!(
            "E2E_RPC_URL unset; skipping live bytecode seed verification \
             (set an Ethereum HTTPS endpoint to run)."
        );
        return Ok(());
    };

    let provider = Arc::new(
        RootProvider::<AnyNetwork>::connect(&url)
            .await
            .context("connect E2E_RPC_URL")?,
    );
    let latest = provider.get_block_number().await?;
    let pinned = latest.saturating_sub(8);
    let block = BlockId::Number(BlockNumberOrTag::Number(pinned));
    let mut cache = EvmCache::builder(provider).block(block).build().await;

    let v2_seed = uniswap_v2_pair_code_seed(UNISWAP_V2_USDC_WETH);
    let v2_len = v2_seed.runtime_bytecode.len();
    let v2_hash = v2_seed.code_hash;
    cache.seed_account_code(v2_seed.address, v2_seed.runtime_bytecode)?;

    let v3_tick_spacing = 10;
    let mut v3_immutables = V3ImmutablePatchValues::default()
        .with_pool_address(UNISWAP_V3_USDC_WETH_500)
        .with_factory(CANONICAL_UNISWAP_V3_FACTORY)
        .with_token0(USDC)
        .with_token1(WETH)
        .with_fee(500)
        .with_tick_spacing(v3_tick_spacing);
    v3_immutables.max_liquidity_per_tick = uniswap_v3_max_liquidity_per_tick(v3_tick_spacing);
    let v3_seed = uniswap_v3_code_seed(UNISWAP_V3_USDC_WETH_500, &v3_immutables)
        .context("render Uniswap V3 code seed from explicit immutable values")?;
    let v3_len = v3_seed.runtime_bytecode.len();
    let v3_hash = v3_seed.code_hash;
    cache.seed_account_code(v3_seed.address, v3_seed.runtime_bytecode)?;

    ensure_pending(&cache, UNISWAP_V2_USDC_WETH, v2_hash)?;
    ensure_pending(&cache, UNISWAP_V3_USDC_WETH_500, v3_hash)?;

    let first = cache.verify_code_seeds()?;
    ensure_clean_report(&first)?;
    ensure!(
        first.verified.contains(&UNISWAP_V2_USDC_WETH)
            && first.verified.contains(&UNISWAP_V3_USDC_WETH_500),
        "expected both AMM code seeds to verify, got {:?}",
        first.verified
    );

    ensure_verified(&cache, UNISWAP_V2_USDC_WETH, v2_hash)?;
    ensure_verified(&cache, UNISWAP_V3_USDC_WETH_500, v3_hash)?;

    let second = cache.verify_code_seeds()?;
    ensure!(
        second.verified.is_empty(),
        "verified code seeds must not be reverified; second report: {second:?}"
    );
    ensure_clean_report(&second)?;

    println!("# Verified AMM bytecode seeds");
    println!("- rpc: E2E_RPC_URL (redacted)");
    println!("- pinned block: {pinned}");
    println!("- Uniswap V2 USDC/WETH: bytes={v2_len} hash={v2_hash:?}");
    println!("- Uniswap V3 USDC/WETH 0.05%: bytes={v3_len} hash={v3_hash:?}");
    println!(
        "- first verify_code_seeds: verified={} mismatched={} not_deployed={} codeless={} unverifiable={}",
        first.verified.len(),
        first.mismatched.len(),
        first.not_deployed.len(),
        first.codeless.len(),
        first.unverifiable.len()
    );
    println!(
        "- second verify_code_seeds: verified={} (already verified seeds skipped)",
        second.verified.len()
    );

    Ok(())
}

fn ensure_pending(
    cache: &EvmCache,
    address: alloy_primitives::Address,
    expected_hash: alloy_primitives::B256,
) -> Result<()> {
    match cache.code_seed_state(&address) {
        Some(CodeSeedState::Pending { code_hash }) if *code_hash == expected_hash => Ok(()),
        other => bail!("expected {address:?} to be Pending with {expected_hash:?}, got {other:?}"),
    }
}

fn ensure_verified(
    cache: &EvmCache,
    address: alloy_primitives::Address,
    expected_hash: alloy_primitives::B256,
) -> Result<()> {
    match cache.code_seed_state(&address) {
        Some(CodeSeedState::Verified { code_hash, .. }) if *code_hash == expected_hash => Ok(()),
        other => bail!("expected {address:?} to be Verified with {expected_hash:?}, got {other:?}"),
    }
}

fn ensure_clean_report(report: &CodeVerifyReport) -> Result<()> {
    ensure!(
        report.mismatched.is_empty(),
        "mismatched: {:?}",
        report.mismatched
    );
    ensure!(
        report.not_deployed.is_empty(),
        "not deployed: {:?}",
        report.not_deployed
    );
    ensure!(
        report.codeless.is_empty(),
        "codeless: {:?}",
        report.codeless
    );
    ensure!(
        report.unverifiable.is_empty(),
        "unverifiable: {:?}",
        report.unverifiable
    );
    Ok(())
}
