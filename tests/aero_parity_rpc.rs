//! Live Aero (Dromos) quote parity (env-gated, `#[ignore]`).
//!
//! Forks each chain at the hash-pinned block of the migration design
//! evidence, cold-starts the reviewed Aero pool with its reviewed runtime
//! identities and warm cells, and proves that the crate's offline quotes equal
//! the chain's own answers:
//!
//! - Ethereum 26292020 (`0xc7d9bbfa…`) and Base 53205395 (`0x82167d87…`):
//!   `simulate_swap` / `simulate_swap_exact_output` through the Metaquoter
//!   equal `eth_call` of the same Metaquoter functions, for 0.5 / 1 / 2 / 5
//!   mooBIFI-equivalent in both directions, under a zero and a non-zero
//!   priority fee, and reproduce the exact wei of the migration design's
//!   pinned batteries (`outputs/aero-migration-design/onchain/*.json`).
//! - Optimism 158800699 (`0x69f98edb…`): the Aero volatile pool's
//!   `getAmountOutWithTotalFee` / `getAmountInWithTotalFee` quotes equal
//!   `eth_call` under the same `tx.gasprice`, and reproduce the MEV-tax
//!   ladder (30 bps up to a 1 gwei tip, 10% cap from 1.1 gwei).
//! - Every learned quote read set replays against an RPC-disconnected
//!   snapshot (offline closure).
//!
//! ```text
//! E2E_RPC_URL=<alchemy archive url> cargo test --test aero_parity_rpc -- --ignored --nocapture
//! ```
//! Base and Optimism urls come from `E2E_BASE_RPC_URL` / `E2E_OPTIMISM_RPC_URL`,
//! or from `E2E_RPC_URL` with the Alchemy `eth-mainnet` host swapped.

use std::sync::Arc;

use alloy_eips::{BlockId, BlockNumberOrTag};
use alloy_network::{AnyNetwork, TransactionBuilder};
use alloy_primitives::{Address, B256, Bytes, U256, address, b256};
use alloy_provider::{Provider, RootProvider};
use alloy_rpc_types_eth::TransactionRequest;
use alloy_sol_types::SolCall;
use anyhow::{Context, Result, anyhow, ensure};

use evm_amm_state::adapters::{
    AdapterRegistry, AmmAdapter, ColdStartOutcome, ColdStartPolicy, ConcentratedLiquidityAdapter,
    PoolKey, PoolRegistration, PoolStatus, ProtocolMetadata, QuoteWarmup, SimConfig,
    SlipstreamRuntimeFamily, SolidlyV2Adapter, SolidlyV2Metadata, V3Metadata, V3StorageLayout,
};
use evm_fork_cache::cache::EvmCache;

alloy_sol_types::sol! {
    interface IMetaquoter {
        function quoteExactInputSingleV3(address pool, address tokenIn, uint256 amountIn, uint160 sqrtPriceLimitX96)
            returns (uint256 amountOut, uint160 sqrtPriceX96After, uint32 initializedTicksCrossed);
        function quoteExactOutputSingleV3(address pool, address tokenIn, uint256 amountOut, uint160 sqrtPriceLimitX96)
            returns (uint256 amountIn, uint160 sqrtPriceX96After, uint32 initializedTicksCrossed);
    }
    interface IAeroPool {
        function getAmountOutWithTotalFee(uint256 amountIn, address tokenIn) returns (uint256);
        function getAmountInWithTotalFee(uint256 amountOut, address tokenOut) returns (uint256);
        function getAmountOut(uint256 amountIn, address tokenIn) returns (uint256);
    }
}

const METAQUOTER: Address = address!("1dDfeC5058180C207F55BB77D95AcDd0d8227868");
/// The fresh EOA the design evidence quoted from.
const EOA: Address = address!("1111111111111111111111111111111111111111");

const ETH_BLOCK: u64 = 26_292_020;
const ETH_BLOCK_HASH: B256 =
    b256!("c7d9bbfa9eeec36423d72a7b193ec991d0e92ca56d0a6a1d3714de36965d44af");
const ETH_BASEFEE: u128 = 198_438_672;
const ETH_POOL: Address = address!("ff35aa95669b5668AC6727FdcFc0D5582cdd78D1");
const BIFI: Address = address!("B1F1ee126e9c96231Cc3d3fAD7C08b4cf873b1f1");
const ETH_WETH: Address = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");

const BASE_BLOCK: u64 = 53_205_395;
const BASE_BLOCK_HASH: B256 =
    b256!("82167d8751d4e3731d238bb99c86a45cd84722760cbf77912a8b020dd23a40df");
const BASE_BASEFEE: u128 = 5_000_000;
const BASE_POOL: Address = address!("EEBE78d734dC4bAC027061cEE9bc12C5803fE4f8");
const L2_WETH: Address = address!("4200000000000000000000000000000000000006");
const MOO_BIFI: Address = address!("c55E93C62874D8100dBd2DfE307EDc1036ad5434");

const OP_BLOCK: u64 = 158_800_699;
const OP_BLOCK_HASH: B256 =
    b256!("69f98edbc36470a6954f5e1f2522c698666f30cb812775c206c2f6b8869b0666");
const OP_BASEFEE: u128 = 308;
const OP_POOL: Address = address!("C71B571aA9F06BFA096c24e9Df5C45fC4fAB4151");

/// `(basis size, sell out in wei, buy in in wei)` per chain: the exact
/// amounts and Metaquoter results of the pinned evidence batteries
/// (`eth-26292020.json`, `base-53205395.json`; 0.5 / 1 / 2 / 5 mooBIFI
/// equivalent, Ethereum sizes converted through the mooBIFI price per share).
const ETH_SIZES: [(u128, u128, u128); 4] = [
    (
        690_047_236_617_973_577,
        20_349_243_649_135_738,
        20_478_437_098_313_086,
    ),
    (
        1_380_094_473_235_947_155,
        40_691_993_054_574_829,
        40_963_433_529_432_166,
    ),
    (
        2_760_188_946_471_894_310,
        81_358_021_565_806_650,
        81_953_117_000_024_938,
    ),
    (
        6_900_472_366_179_735_775,
        203_200_568_111_708_279,
        205_079_919_621_477_266,
    ),
];
const BASE_SIZES: [(u128, u128, u128); 4] = [
    (
        500_000_000_000_000_000,
        20_181_052_702_440_415,
        20_596_596_375_698_618,
    ),
    (
        1_000_000_000_000_000_000,
        40_325_910_697_446_965,
        41_198_083_936_617_026,
    ),
    (
        2_000_000_000_000_000_000,
        80_462_819_893_011_695,
        82_415_739_584_985_993,
    ),
    (
        5_000_000_000_000_000_000,
        199_664_897_618_382_759,
        206_186_276_367_188_710,
    ),
];
/// `(size, sell out, buy in)` in wei.
type LadderRow = (u128, u128, u128);
/// One tip-ladder step: `(tip wei, rows)`.
type LadderStep = (u128, [LadderRow; 4]);
/// Exact wei values from `opt-158800699-mev-context.json`, per tip ladder step.
const OP_LADDER: [LadderStep; 5] = [
    (
        10_000_001,
        [
            (
                500_000_000_000_000_000,
                20_184_656_781_363_703,
                20_353_865_536_958_055,
            ),
            (
                1_000_000_000_000_000_000,
                40_322_280_955_022_039,
                40_755_523_726_579_833,
            ),
            (
                2_000_000_000_000_000_000,
                80_457_087_499_432_798,
                81_702_892_976_485_759,
            ),
            (
                5_000_000_000_000_000_000,
                199_749_644_769_283_573,
                205_709_716_099_742_842,
            ),
        ],
    ),
    (
        1_000_000_000,
        [
            (
                500_000_000_000_000_000,
                20_184_656_781_363_703,
                20_353_865_536_958_055,
            ),
            (
                1_000_000_000_000_000_000,
                40_322_280_955_022_039,
                40_755_523_726_579_833,
            ),
            (
                2_000_000_000_000_000_000,
                80_457_087_499_432_798,
                81_702_892_976_485_759,
            ),
            (
                5_000_000_000_000_000_000,
                199_749_644_769_283_573,
                205_709_716_099_742_842,
            ),
        ],
    ),
    (
        1_050_000_000,
        [
            (
                500_000_000_000_000_000,
                19_173_508_713_744_541,
                21_428_515_248_518_670,
            ),
            (
                1_000_000_000_000_000_000,
                38_304_576_500_753_719,
                42_907_346_521_013_826,
            ),
            (
                2_000_000_000_000_000_000,
                76_439_951_712_441_506,
                86_016_667_684_853_539,
            ),
            (
                5_000_000_000_000_000_000,
                189_842_006_910_909_666,
                216_570_841_553_794_734,
            ),
        ],
    ),
    (
        1_100_000_000,
        [
            (
                500_000_000_000_000_000,
                18_162_242_335_329_048,
                22_622_969_833_162_966,
            ),
            (
                1_000_000_000_000_000_000,
                36_286_400_372_856_044,
                45_299_060_373_913_148,
            ),
            (
                2_000_000_000_000_000_000,
                72_420_941_704_298_102,
                90_811_353_731_946_824,
            ),
            (
                5_000_000_000_000_000_000,
                179_922_884_959_945_091,
                228_642_794_817_662_891,
            ),
        ],
    ),
    (
        2_000_000_000,
        [
            (
                500_000_000_000_000_000,
                18_162_242_335_329_048,
                22_622_969_833_162_966,
            ),
            (
                1_000_000_000_000_000_000,
                36_286_400_372_856_044,
                45_299_060_373_913_148,
            ),
            (
                2_000_000_000_000_000_000,
                72_420_941_704_298_102,
                90_811_353_731_946_824,
            ),
            (
                5_000_000_000_000_000_000,
                179_922_884_959_945_091,
                228_642_794_817_662_891,
            ),
        ],
    ),
];

#[path = "support/aero_rpc.rs"]
mod aero_rpc;
use aero_rpc::{derived_url, eth_url};

fn base_url() -> Result<String> {
    derived_url("E2E_BASE_RPC_URL", "base-mainnet")
}

fn optimism_url() -> Result<String> {
    derived_url("E2E_OPTIMISM_RPC_URL", "opt-mainnet")
}

async fn provider(url: &str) -> Result<Arc<RootProvider<AnyNetwork>>> {
    Ok(Arc::new(
        RootProvider::<AnyNetwork>::connect(url)
            .await
            .context("connect RPC url")?,
    ))
}

async fn pinned_cache(
    provider: &Arc<RootProvider<AnyNetwork>>,
    block: u64,
    expected_hash: B256,
) -> Result<EvmCache> {
    let header = provider
        .get_block_by_number(BlockNumberOrTag::Number(block))
        .await?
        .ok_or_else(|| anyhow!("block {block} missing"))?;
    ensure!(
        header.header.hash == expected_hash,
        "block {block} hash {} does not match the pinned evidence {expected_hash}",
        header.header.hash
    );
    Ok(EvmCache::at_block(
        Arc::clone(provider),
        BlockId::Number(BlockNumberOrTag::Number(block)),
    )
    .await)
}

async fn eth_call(
    provider: &RootProvider<AnyNetwork>,
    from: Address,
    target: Address,
    calldata: Bytes,
    gas_price: u128,
    block: u64,
) -> Result<Bytes> {
    let tx = TransactionRequest::default()
        .with_from(from)
        .with_to(target)
        .with_input(calldata)
        .with_gas_price(gas_price);
    provider
        .call(tx.into())
        .block(BlockId::Number(BlockNumberOrTag::Number(block)))
        .await
        .context("eth_call at the pinned block")
}

fn aero_cl_registration(
    pool: Address,
    spacing: i32,
    token0: Address,
    token1: Address,
) -> PoolRegistration {
    PoolRegistration::new(PoolKey::Slipstream(pool))
        .with_state_address(pool)
        .with_metadata(ProtocolMetadata::Slipstream(
            V3Metadata::default()
                .with_token0(token0)
                .with_token1(token1)
                .with_tick_spacing(spacing)
                .with_storage_layout(V3StorageLayout::aero_cl(spacing))
                .with_metaquoter(METAQUOTER),
        ))
}

#[allow(clippy::too_many_arguments)]
async fn cl_parity(
    url: &str,
    chain_id: u64,
    block: u64,
    block_hash: B256,
    basefee: u128,
    pool: Address,
    spacing: i32,
    family: SlipstreamRuntimeFamily,
    basis: Address,
    quote: Address,
    sizes: [(u128, u128, u128); 4],
) -> Result<()> {
    let provider = provider(url).await?;
    let mut cache = pinned_cache(&provider, block, block_hash).await?;
    ensure!(
        cache.chain_id() == chain_id,
        "chain id {}",
        cache.chain_id()
    );
    let (token0, token1) = if basis < quote {
        (basis, quote)
    } else {
        (quote, basis)
    };
    let config = SimConfig::default().with_tx_origin(EOA);
    let mut registry = AdapterRegistry::new().with_sim_config(config);
    registry.register_adapter(Arc::new(ConcentratedLiquidityAdapter::default()))?;
    let mut pools = vec![aero_cl_registration(pool, spacing, token0, token1)];
    let outcomes = registry
        .cold_start_many(
            &mut pools,
            &mut cache,
            provider.as_ref(),
            ColdStartPolicy::Eager,
        )
        .await?;
    ensure!(
        matches!(outcomes[0], ColdStartOutcome::Ready(_)),
        "cold start must be ready: {:?}",
        outcomes[0]
    );
    ensure!(pools[0].status == PoolStatus::Ready);
    let registration = pools.remove(0);
    ensure!(
        ConcentratedLiquidityAdapter::aero_cl_runtime_family(&registration) == Some(family),
        "registration must classify as {family:?}"
    );
    ConcentratedLiquidityAdapter::validate_aero_cl_runtime(family, &cache.snapshot())
        .map_err(|error| anyhow!("reviewed runtime identity: {error}"))?;
    let adapter = ConcentratedLiquidityAdapter::default();
    for (address, slot) in adapter.verified_storage_targets(&registration) {
        ensure!(
            cache.cached_storage_value(address, slot).is_some(),
            "reviewed cell {address}:{slot:#x} must be resident after cold start"
        );
    }
    for (address, hash) in adapter.reviewed_code_identities(&registration) {
        ensure!(
            cache.snapshot().account_code_hash(address) == Some(hash),
            "pinned runtime {address} must be resident with hash {hash}"
        );
    }
    eprintln!("[{chain_id}] block {block} {block_hash:#x}: cold start ready, identities verified");

    let contexts = [
        ("tip 0", SimConfig::default().with_tx_origin(EOA)),
        (
            "tip 1 gwei",
            SimConfig::default()
                .with_tx_origin(EOA)
                .with_effective_gas_price(basefee, 1_000_000_000),
        ),
    ];
    for (label, config) in &contexts {
        let gas_price = config.gas_price.unwrap_or(0);
        for (size, evidence_sell, evidence_buy) in sizes {
            let amount = U256::from(size);
            // Sell basis for quote, exact input.
            let sell = adapter
                .simulate_swap(&registration, &mut cache, basis, quote, amount, config)
                .map_err(|error| anyhow!("offline sell quote: {error}"))?;
            let truth = eth_call(
                &provider,
                EOA,
                METAQUOTER,
                Bytes::from(
                    IMetaquoter::quoteExactInputSingleV3Call {
                        pool,
                        tokenIn: basis,
                        amountIn: amount,
                        sqrtPriceLimitX96: U256::ZERO.to(),
                    }
                    .abi_encode(),
                ),
                gas_price,
                block,
            )
            .await?;
            let truth =
                IMetaquoter::quoteExactInputSingleV3Call::abi_decode_returns_validate(&truth)?;
            ensure!(
                sell.amount_out == truth.amountOut,
                "[{chain_id} {label}] sell {size}: offline {} vs eth_call {}",
                sell.amount_out,
                truth.amountOut
            );
            ensure!(
                sell.amount_out == U256::from(evidence_sell),
                "[{chain_id} {label}] sell {size}: {} does not match the pinned evidence {evidence_sell}",
                sell.amount_out
            );
            // Buy basis with quote, exact output.
            let buy = adapter
                .simulate_swap_exact_output(&registration, &mut cache, quote, basis, amount, config)
                .map_err(|error| anyhow!("offline buy quote: {error}"))?;
            let truth = eth_call(
                &provider,
                EOA,
                METAQUOTER,
                Bytes::from(
                    IMetaquoter::quoteExactOutputSingleV3Call {
                        pool,
                        tokenIn: quote,
                        amountOut: amount,
                        sqrtPriceLimitX96: U256::ZERO.to(),
                    }
                    .abi_encode(),
                ),
                gas_price,
                block,
            )
            .await?;
            let truth =
                IMetaquoter::quoteExactOutputSingleV3Call::abi_decode_returns_validate(&truth)?;
            ensure!(
                buy.amount_in == truth.amountIn,
                "[{chain_id} {label}] buy {size}: offline {} vs eth_call {}",
                buy.amount_in,
                truth.amountIn
            );
            ensure!(
                buy.amount_in == U256::from(evidence_buy),
                "[{chain_id} {label}] buy {size}: {} does not match the pinned evidence {evidence_buy}",
                buy.amount_in
            );
            eprintln!(
                "[{chain_id} {label}] size {size}: sell -> {} buy <- {} (eth_call parity ok)",
                sell.amount_out, buy.amount_in
            );
        }
    }

    // Offline closure: the learned read set replays on a disconnected snapshot.
    registry.register_pool(registration.clone())?;
    let key = registration.key.clone();
    let report = registry
        .warm_quote_read_sets(
            &mut cache,
            [
                QuoteWarmup::exact_input(key.clone(), basis, quote, U256::from(sizes[1].0)),
                QuoteWarmup::exact_input(
                    key.clone(),
                    quote,
                    basis,
                    U256::from(sizes[1].0) / U256::from(20),
                ),
            ],
        )
        .map_err(|error| anyhow!("quote read-set warming: {error}"))?;
    ensure!(report.entries().len() == 2);
    registry
        .validate_quote_read_sets(cache.snapshot(), [&key])
        .map_err(|error| anyhow!("offline replay of learned read sets: {error}"))?;
    eprintln!("[{chain_id}] learned read sets replay offline");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires an Ethereum archive RPC (E2E_RPC_URL); run with --ignored"]
async fn ethereum_aero_cl_metaquoter_parity() -> Result<()> {
    let url = eth_url()?;
    cl_parity(
        &url,
        1,
        ETH_BLOCK,
        ETH_BLOCK_HASH,
        ETH_BASEFEE,
        ETH_POOL,
        50,
        SlipstreamRuntimeFamily::AeroEthereumBifi,
        BIFI,
        ETH_WETH,
        ETH_SIZES,
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires a Base archive RPC (E2E_BASE_RPC_URL / E2E_RPC_URL on Alchemy); run with --ignored"]
async fn base_aero_cl_metaquoter_parity() -> Result<()> {
    let url = base_url()?;
    cl_parity(
        &url,
        8_453,
        BASE_BLOCK,
        BASE_BLOCK_HASH,
        BASE_BASEFEE,
        BASE_POOL,
        100,
        SlipstreamRuntimeFamily::AeroBaseMooBifi,
        MOO_BIFI,
        L2_WETH,
        BASE_SIZES,
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires an Optimism archive RPC (E2E_OPTIMISM_RPC_URL / E2E_RPC_URL on Alchemy); run with --ignored"]
async fn optimism_aero_volatile_total_fee_parity() -> Result<()> {
    let url = optimism_url()?;
    let provider = provider(&url).await?;
    let mut cache = pinned_cache(&provider, OP_BLOCK, OP_BLOCK_HASH).await?;
    ensure!(cache.chain_id() == 10);
    let mut registry = AdapterRegistry::new();
    registry.register_adapter(Arc::new(SolidlyV2Adapter::default()))?;
    let mut pools = vec![
        PoolRegistration::new(PoolKey::SolidlyV2(OP_POOL))
            .with_state_address(OP_POOL)
            .with_metadata(ProtocolMetadata::SolidlyV2(
                SolidlyV2Metadata::default()
                    .with_token0(L2_WETH)
                    .with_token1(MOO_BIFI)
                    .with_aero_volatile(),
            )),
    ];
    let outcomes = registry
        .cold_start_many(
            &mut pools,
            &mut cache,
            provider.as_ref(),
            ColdStartPolicy::Eager,
        )
        .await?;
    ensure!(
        matches!(outcomes[0], ColdStartOutcome::Ready(_)),
        "cold start must be ready (POOL_TYPE verified, fee cells warm): {:?}",
        outcomes[0]
    );
    let registration = pools.remove(0);
    let adapter = SolidlyV2Adapter::default();
    for (address, slot) in adapter.verified_storage_targets(&registration) {
        ensure!(
            cache.cached_storage_value(address, slot).is_some(),
            "reviewed cell {address}:{slot:#x} must be resident"
        );
    }
    for (address, hash) in adapter.reviewed_code_identities(&registration) {
        ensure!(
            cache.snapshot().account_code_hash(address) == Some(hash),
            "pinned runtime {address} must be resident with hash {hash}"
        );
    }
    eprintln!("[10] block {OP_BLOCK} {OP_BLOCK_HASH:#x}: cold start ready, identities verified");

    for (tip, rows) in OP_LADDER {
        let config = SimConfig::default()
            .with_tx_origin(EOA)
            .with_effective_gas_price(OP_BASEFEE, tip);
        let gas_price = OP_BASEFEE + tip;
        for (size, expected_sell, expected_buy) in rows {
            let amount = U256::from(size);
            let sell = adapter
                .simulate_swap(
                    &registration,
                    &mut cache,
                    MOO_BIFI,
                    L2_WETH,
                    amount,
                    &config,
                )
                .map_err(|error| anyhow!("offline sell quote: {error}"))?;
            ensure!(
                sell.amount_out == U256::from(expected_sell),
                "[10 tip {tip}] sell {size}: {} vs pinned evidence {expected_sell}",
                sell.amount_out
            );
            let truth = eth_call(
                &provider,
                EOA,
                OP_POOL,
                Bytes::from(
                    IAeroPool::getAmountOutWithTotalFeeCall {
                        amountIn: amount,
                        tokenIn: MOO_BIFI,
                    }
                    .abi_encode(),
                ),
                gas_price,
                OP_BLOCK,
            )
            .await?;
            let truth =
                IAeroPool::getAmountOutWithTotalFeeCall::abi_decode_returns_validate(&truth)?;
            ensure!(
                sell.amount_out == truth,
                "[10 tip {tip}] sell {size}: eth_call parity"
            );
            let buy = adapter
                .simulate_swap_exact_output(
                    &registration,
                    &mut cache,
                    L2_WETH,
                    MOO_BIFI,
                    amount,
                    &config,
                )
                .map_err(|error| anyhow!("offline buy quote: {error}"))?;
            ensure!(
                buy.amount_in == U256::from(expected_buy),
                "[10 tip {tip}] buy {size}: {} vs pinned evidence {expected_buy}",
                buy.amount_in
            );
            let truth = eth_call(
                &provider,
                EOA,
                OP_POOL,
                Bytes::from(
                    IAeroPool::getAmountInWithTotalFeeCall {
                        amountOut: amount,
                        tokenOut: MOO_BIFI,
                    }
                    .abi_encode(),
                ),
                gas_price,
                OP_BLOCK,
            )
            .await?;
            let truth =
                IAeroPool::getAmountInWithTotalFeeCall::abi_decode_returns_validate(&truth)?;
            ensure!(
                buy.amount_in == truth,
                "[10 tip {tip}] buy {size}: eth_call parity"
            );
            eprintln!(
                "[10 tip {tip}] size {size}: sell -> {} buy <- {} (pinned + eth_call parity ok)",
                sell.amount_out, buy.amount_in
            );
        }
    }

    // The legacy variant on the same pool quotes the base fee only, which is
    // exactly the under-quote the Aero variant exists to prevent.
    let legacy = PoolRegistration::new(PoolKey::SolidlyV2(OP_POOL))
        .with_state_address(OP_POOL)
        .with_metadata(ProtocolMetadata::SolidlyV2(
            SolidlyV2Metadata::default()
                .with_token0(L2_WETH)
                .with_token1(MOO_BIFI)
                .with_stable(false)
                .with_storage_layout(
                    evm_amm_state::adapters::storage::SolidlyStorageLayout::aero_volatile(),
                ),
        ));
    let base_fee_only = adapter
        .simulate_swap(
            &legacy,
            &mut cache,
            MOO_BIFI,
            L2_WETH,
            U256::from(1_000_000_000_000_000_000_u128),
            &SimConfig::default()
                .with_tx_origin(EOA)
                .with_effective_gas_price(OP_BASEFEE, 2_000_000_000),
        )
        .map_err(|error| anyhow!("legacy getAmountOut: {error}"))?;
    ensure!(
        base_fee_only.amount_out == U256::from(40_322_280_955_022_039_u128),
        "legacy getAmountOut ignores the MEV tax: {}",
        base_fee_only.amount_out
    );

    // Offline closure of the total-fee quote under a taxed tip.
    let taxed = SimConfig::default()
        .with_tx_origin(EOA)
        .with_effective_gas_price(OP_BASEFEE, 1_100_000_000);
    let mut registry = AdapterRegistry::new().with_sim_config(taxed);
    registry.register_adapter(Arc::new(SolidlyV2Adapter::default()))?;
    registry.register_pool(registration.clone())?;
    let key = registration.key.clone();
    registry
        .warm_quote_read_sets(
            &mut cache,
            [QuoteWarmup::exact_input(
                key.clone(),
                MOO_BIFI,
                L2_WETH,
                U256::from(1_000_000_000_000_000_000_u128),
            )],
        )
        .map_err(|error| anyhow!("quote read-set warming: {error}"))?;
    registry
        .validate_quote_read_sets(cache.snapshot(), [&key])
        .map_err(|error| anyhow!("offline replay of the taxed quote: {error}"))?;
    eprintln!("[10] taxed total-fee quote replays offline");
    Ok(())
}
