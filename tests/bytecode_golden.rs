//! Golden bytecode-seed tests: pin the embedded runtime artifacts and the V3
//! immutable-patch offsets to **chain-truth** code hashes.
//!
//! Each expected hash is the on-chain `EXTCODEHASH` (keccak256 of deployed
//! runtime code) of a canonical mainnet pool, captured with
//! `cast code <pool> | cast keccak`. Rendering our embedded template for that
//! pool's immutables must reproduce the exact hash. This is what catches a
//! corrupted artifact or a wrong patch offset **offline**, with no RPC — the
//! live `verified_bytecode_seed` example proves the same thing against a live
//! node when `E2E_RPC_URL` is set; this test file itself needs no env at all.
//!
//! The V3 pools span tickSpacings 1 / 10 / 60 (fees 0.01% / 0.05% / 0.3%) with
//! two distinct token pairs, so a wrong offset on `token0`, `token1`, `fee`,
//! `tick_spacing`, or `max_liquidity_per_tick` cannot pass by coincidence.

use alloy_primitives::{Address, B256, address, b256};
use evm_amm_state::adapters::{
    V3ImmutablePatchValues, uniswap_v2_pair_runtime_code_hash, uniswap_v3_code_seed,
    uniswap_v3_max_liquidity_per_tick,
};

const CANONICAL_V3_FACTORY: Address = address!("1F98431c8aD98523631AE4a59f267346ea31F984");
const USDC: Address = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
const WETH: Address = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
const DAI: Address = address!("6B175474E89094C44Da98b954EedeAC495271d0F");

// Chain-truth EXTCODEHASH values (Ethereum mainnet).
const V2_PAIR_CODE_HASH: B256 =
    b256!("5b83bdbcc56b2e630f2807bbadd2b0c21619108066b92a58de081261089e9ce5");
const V3_USDC_WETH_500_CODE_HASH: B256 =
    b256!("a981b66c747a3d9fa29d7e200d5faaa2826960523d0e5a0df8148e8868c480b4");
const V3_DAI_USDC_100_CODE_HASH: B256 =
    b256!("745c067e705970688ce6589a8f7d4512df75e9203fb8e2f1a09a07ab11dcec7b");
const V3_USDC_WETH_3000_CODE_HASH: B256 =
    b256!("f2b8b58f95b1471751302e520a0e7c410ce9846ed46020be253dbd25fbb6da11");

#[test]
fn v2_pair_runtime_matches_chain_code_hash() {
    assert_eq!(
        uniswap_v2_pair_runtime_code_hash(),
        V2_PAIR_CODE_HASH,
        "embedded Uniswap V2 pair runtime must hash to the canonical on-chain code hash"
    );
}

/// Render the embedded V3 template for `pool`'s immutables and assert its hash.
fn assert_v3_render(
    pool: Address,
    token0: Address,
    token1: Address,
    fee: u32,
    tick_spacing: i32,
    expected: B256,
) {
    let mut immutables = V3ImmutablePatchValues::default()
        .with_pool_address(pool)
        .with_factory(CANONICAL_V3_FACTORY)
        .with_token0(token0)
        .with_token1(token1)
        .with_fee(fee)
        .with_tick_spacing(tick_spacing);
    immutables.max_liquidity_per_tick = uniswap_v3_max_liquidity_per_tick(tick_spacing);
    let seed = uniswap_v3_code_seed(pool, &immutables).expect("render V3 template");
    assert_eq!(
        seed.code_hash, expected,
        "rendered V3 runtime for {pool:?} (fee={fee}, tickSpacing={tick_spacing}) \
         must match the on-chain code hash"
    );
}

#[test]
fn v3_usdc_weth_500_matches_chain_code_hash() {
    assert_v3_render(
        address!("88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640"),
        USDC,
        WETH,
        500,
        10,
        V3_USDC_WETH_500_CODE_HASH,
    );
}

#[test]
fn v3_dai_usdc_100_matches_chain_code_hash() {
    assert_v3_render(
        address!("5777d92f208679DB4b9778590Fa3CAB3aC9e2168"),
        DAI,
        USDC,
        100,
        1,
        V3_DAI_USDC_100_CODE_HASH,
    );
}

#[test]
fn v3_usdc_weth_3000_matches_chain_code_hash() {
    assert_v3_render(
        address!("8ad599c3A0ff1De082011EFDDc58f1908eb6e6D8"),
        USDC,
        WETH,
        3000,
        60,
        V3_USDC_WETH_3000_CODE_HASH,
    );
}
