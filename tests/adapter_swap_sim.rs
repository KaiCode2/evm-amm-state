//! WS2 swap-simulation harness tests (offline) + Balancer reactive integration.
//!
//! Every test runs FULLY OFFLINE over a mocked provider: a deterministic
//! mock-quote contract is installed at the quote target (fixture style of
//! `tests/fixtures/MockBalancerVault.sol`), its return value is derived from a
//! warmed "quote" slot, and `simulate_swap` is asserted to return it without any
//! RPC (`asserter.read_q().is_empty()`). A reverting target → `SimError::Reverted`.
//!
//! The Balancer reactive test cold-starts a combined mock vault, ingests a
//! `Swap` log through the reactive runtime, and asserts the discovered balance
//! slots are refreshed so a subsequent `simulate_swap` reflects the change.

use std::collections::HashMap;
use std::sync::Arc;

use alloy_eips::BlockId;
use alloy_network::Ethereum;
use alloy_primitives::{Address, B256, Bytes, Log as PrimitiveLog, U256, hex, keccak256};
use alloy_provider::{RootProvider, network::AnyNetwork};
use alloy_rpc_client::RpcClient;
use alloy_rpc_types_eth::Log as RpcLog;
use alloy_transport::mock::Asserter;
use anyhow::{Result, anyhow};

use evm_amm_state::adapters::storage::SolidlyStorageLayout;
use evm_amm_state::adapters::{
    AdapterRegistry, AmmAdapter, AmmReactiveHandler, BalancerV2Adapter, BalancerV2Metadata,
    ColdStartOutcome, ColdStartPolicy, CurveAdapter, CurveMetadata, CurveVariant, PoolKey,
    PoolRegistration, PoolStatus, ProtocolMetadata, SimConfig, SimError, SolidlyV2Adapter,
    SolidlyV2Metadata, UniswapV2Adapter, UniswapV2Metadata, UniswapV3Adapter, V3Metadata,
};
use evm_fork_cache::cache::{EvmCache, StorageBatchFetchFn};
use evm_fork_cache::reactive::{
    BlockRef, ChainStatus, InputSource, ReactiveConfig, ReactiveContext, ReactiveInput,
    ReactiveInputBatch, ReactiveInputRecord, ReactiveRuntime,
};
use revm::state::{AccountInfo, Bytecode};

// --- offline cache scaffolding (mirrors cold_start_adoption.rs) ---

async fn setup_cache_with_asserter() -> Result<(EvmCache, Asserter)> {
    let asserter = Asserter::new();
    let client = RpcClient::mocked(asserter.clone());
    let provider = RootProvider::<AnyNetwork>::new(client);
    Ok((EvmCache::new(Arc::new(provider)).await, asserter))
}

fn install_default_account(cache: &mut EvmCache, addr: Address) {
    cache
        .db_mut()
        .insert_account_info(addr, AccountInfo::default());
}

/// Install raw runtime bytecode (a compiled mock fixture) at `addr`.
fn install_runtime(cache: &mut EvmCache, addr: Address, runtime: &str) {
    let code = Bytecode::new_raw(Bytes::from(
        hex::decode(runtime.trim()).expect("valid mock runtime hex"),
    ));
    let code_hash = code.hash_slow();
    cache.db_mut().insert_account_info(
        addr,
        AccountInfo {
            balance: U256::ZERO,
            nonce: 1,
            code: Some(code),
            code_hash,
            account_id: None,
        },
    );
}

/// A fetcher returning `Err` for `fail` slots and `Ok(value-or-ZERO)` otherwise.
fn fetcher(
    values: HashMap<(Address, U256), U256>,
    fail: Vec<(Address, U256)>,
) -> StorageBatchFetchFn {
    let fail: std::collections::HashSet<(Address, U256)> = fail.into_iter().collect();
    Arc::new(
        move |requests: Vec<(Address, U256)>, _block: Option<BlockId>| {
            requests
                .into_iter()
                .map(|(address, slot)| {
                    if fail.contains(&(address, slot)) {
                        (address, slot, Err(anyhow!("archive miss")))
                    } else {
                        (
                            address,
                            slot,
                            Ok(values.get(&(address, slot)).copied().unwrap_or_default()),
                        )
                    }
                })
                .collect()
        },
    )
}

fn token_slot_word(addr: Address) -> U256 {
    U256::from_be_slice(addr.as_slice())
}

const REVERT_RUNTIME: &str = include_str!("fixtures/mock_balancer_vault_revert_runtime.hex");

// --- Uniswap V2 offline harness ---

#[tokio::test(flavor = "multi_thread")]
async fn v2_simulate_swap_returns_router_quote_offline() -> Result<()> {
    let router = Address::repeat_byte(0xa1);
    let token_in = Address::repeat_byte(0x01);
    let token_out = Address::repeat_byte(0x02);
    let expected_out = U256::from(4_242_u64);

    let (mut cache, asserter) = setup_cache_with_asserter().await?;
    install_default_account(&mut cache, Address::ZERO);
    install_runtime(
        &mut cache,
        router,
        include_str!("fixtures/mock_v2_router_runtime.hex"),
    );
    // The mock router returns `sload(0)` as the output amount; seed it.
    cache
        .db_mut()
        .insert_account_storage(router, U256::ZERO, expected_out)?;

    let adapter = UniswapV2Adapter::default();
    let registration = PoolRegistration::new(PoolKey::UniswapV2(Address::repeat_byte(0x11)))
        .with_metadata(ProtocolMetadata::UniswapV2(UniswapV2Metadata {
            token0: Some(token_in),
            token1: Some(token_out),
            fee_bps: Some(30),
        }));
    let config = SimConfig::default().with_v2_router(router);

    let quote = adapter
        .simulate_swap(
            &registration,
            &mut cache,
            token_in,
            token_out,
            U256::from(1_000_u64),
            &config,
        )
        .expect("v2 quote should succeed");

    assert_eq!(quote.amount_out, expected_out);
    assert!(
        asserter.read_q().is_empty(),
        "swap sim must be fully offline (no RPC)"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn v2_simulate_swap_reverting_target_is_reverted() -> Result<()> {
    let router = Address::repeat_byte(0xa2);
    let token_in = Address::repeat_byte(0x01);
    let token_out = Address::repeat_byte(0x02);

    let (mut cache, asserter) = setup_cache_with_asserter().await?;
    install_default_account(&mut cache, Address::ZERO);
    install_runtime(&mut cache, router, REVERT_RUNTIME);

    let adapter = UniswapV2Adapter::default();
    let registration = PoolRegistration::new(PoolKey::UniswapV2(Address::repeat_byte(0x11)));
    let config = SimConfig::default().with_v2_router(router);

    let err = adapter
        .simulate_swap(
            &registration,
            &mut cache,
            token_in,
            token_out,
            U256::from(1_000_u64),
            &config,
        )
        .expect_err("reverting router must error");
    assert_eq!(err, SimError::Reverted);
    assert!(asserter.read_q().is_empty(), "must be fully offline");
    Ok(())
}

// --- Uniswap V3 offline harness ---

#[tokio::test(flavor = "multi_thread")]
async fn v3_simulate_swap_returns_quoter_amount_offline() -> Result<()> {
    let quoter = Address::repeat_byte(0xb1);
    let token_in = Address::repeat_byte(0x01);
    let token_out = Address::repeat_byte(0x02);
    let expected_out = U256::from(9_999_u64);

    let (mut cache, asserter) = setup_cache_with_asserter().await?;
    install_default_account(&mut cache, Address::ZERO);
    install_runtime(
        &mut cache,
        quoter,
        include_str!("fixtures/mock_v3_quoter_runtime.hex"),
    );
    cache
        .db_mut()
        .insert_account_storage(quoter, U256::ZERO, expected_out)?;

    let adapter = UniswapV3Adapter::default();
    let registration = PoolRegistration::new(PoolKey::UniswapV3(Address::repeat_byte(0x21)))
        .with_metadata(ProtocolMetadata::UniswapV3(V3Metadata {
            token0: Some(token_in),
            token1: Some(token_out),
            fee: Some(500),
            tick_spacing: Some(10),
            ..Default::default()
        }));
    let config = SimConfig::default().with_v3_quoter(quoter);

    let quote = adapter
        .simulate_swap(
            &registration,
            &mut cache,
            token_in,
            token_out,
            U256::from(1_000_u64),
            &config,
        )
        .expect("v3 quote should succeed");

    assert_eq!(quote.amount_out, expected_out);
    assert!(asserter.read_q().is_empty(), "must be fully offline");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn v3_simulate_swap_reverting_target_is_reverted() -> Result<()> {
    let quoter = Address::repeat_byte(0xb2);
    let token_in = Address::repeat_byte(0x01);
    let token_out = Address::repeat_byte(0x02);

    let (mut cache, asserter) = setup_cache_with_asserter().await?;
    install_default_account(&mut cache, Address::ZERO);
    install_runtime(&mut cache, quoter, REVERT_RUNTIME);

    let adapter = UniswapV3Adapter::default();
    let registration = PoolRegistration::new(PoolKey::UniswapV3(Address::repeat_byte(0x21)))
        .with_metadata(ProtocolMetadata::UniswapV3(V3Metadata {
            fee: Some(500),
            ..Default::default()
        }));
    let config = SimConfig::default().with_v3_quoter(quoter);

    let err = adapter
        .simulate_swap(
            &registration,
            &mut cache,
            token_in,
            token_out,
            U256::from(1_000_u64),
            &config,
        )
        .expect_err("reverting quoter must error");
    assert_eq!(err, SimError::Reverted);
    assert!(asserter.read_q().is_empty(), "must be fully offline");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn v3_simulate_swap_missing_fee_is_missing_metadata() -> Result<()> {
    let quoter = Address::repeat_byte(0xb3);
    let (mut cache, _asserter) = setup_cache_with_asserter().await?;
    install_default_account(&mut cache, Address::ZERO);
    install_runtime(
        &mut cache,
        quoter,
        include_str!("fixtures/mock_v3_quoter_runtime.hex"),
    );

    let adapter = UniswapV3Adapter::default();
    // No `fee` in metadata -> the quote cannot be built.
    let registration = PoolRegistration::new(PoolKey::UniswapV3(Address::repeat_byte(0x21)))
        .with_metadata(ProtocolMetadata::UniswapV3(V3Metadata::default()));
    let config = SimConfig::default().with_v3_quoter(quoter);

    let err = adapter
        .simulate_swap(
            &registration,
            &mut cache,
            Address::repeat_byte(0x01),
            Address::repeat_byte(0x02),
            U256::from(1_000_u64),
            &config,
        )
        .expect_err("missing fee must error");
    assert_eq!(err, SimError::MissingMetadata("V3 fee"));
    Ok(())
}

// --- Balancer V2 offline harness ---

/// Install the combined `MockBalancerVaultQuote` stub and seed slots 0..=4.
/// `queryBatchSwap` returns balance0 (slot 2) as the negated tokenOut delta.
fn install_balancer_quote_vault(
    cache: &mut EvmCache,
    vault: Address,
    balance0: U256,
) -> Result<()> {
    install_runtime(
        cache,
        vault,
        include_str!("fixtures/mock_balancer_vault_quote_runtime.hex"),
    );
    let token0 = Address::repeat_byte(0xc0);
    let token1 = Address::repeat_byte(0xc1);
    cache
        .db_mut()
        .insert_account_storage(vault, U256::from(0), token_slot_word(token0))?;
    cache
        .db_mut()
        .insert_account_storage(vault, U256::from(1), token_slot_word(token1))?;
    cache
        .db_mut()
        .insert_account_storage(vault, U256::from(2), balance0)?;
    cache
        .db_mut()
        .insert_account_storage(vault, U256::from(3), U256::from(2_u64))?;
    cache
        .db_mut()
        .insert_account_storage(vault, U256::from(4), U256::from(7_u64))?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn balancer_simulate_swap_returns_vault_quote_offline() -> Result<()> {
    let vault = Address::repeat_byte(0xd1);
    let mut pid = [0u8; 32];
    pid[..20].fill(0x33);
    let pool_id = B256::from(pid);
    let token_in = Address::repeat_byte(0xc0);
    let token_out = Address::repeat_byte(0xc1);
    let expected_out = U256::from(5_000_u64);

    let (mut cache, asserter) = setup_cache_with_asserter().await?;
    install_default_account(&mut cache, Address::ZERO);
    install_balancer_quote_vault(&mut cache, vault, expected_out)?;

    let adapter = BalancerV2Adapter::default();
    let registration = PoolRegistration::new(PoolKey::BalancerV2(pool_id))
        .with_state_address(vault)
        .with_metadata(ProtocolMetadata::BalancerV2(BalancerV2Metadata {
            vault: Some(vault),
            ..Default::default()
        }));
    let config = SimConfig::default();

    let quote = adapter
        .simulate_swap(
            &registration,
            &mut cache,
            token_in,
            token_out,
            U256::from(1_000_u64),
            &config,
        )
        .expect("balancer quote should succeed");

    assert_eq!(quote.amount_out, expected_out);
    assert!(asserter.read_q().is_empty(), "must be fully offline");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn balancer_simulate_swap_reverting_vault_is_reverted() -> Result<()> {
    let vault = Address::repeat_byte(0xd2);
    let pool_id = B256::repeat_byte(0x34);

    let (mut cache, asserter) = setup_cache_with_asserter().await?;
    install_default_account(&mut cache, Address::ZERO);
    install_runtime(&mut cache, vault, REVERT_RUNTIME);

    let adapter = BalancerV2Adapter::default();
    let registration = PoolRegistration::new(PoolKey::BalancerV2(pool_id))
        .with_state_address(vault)
        .with_metadata(ProtocolMetadata::BalancerV2(BalancerV2Metadata {
            vault: Some(vault),
            ..Default::default()
        }));
    let config = SimConfig::default();

    let err = adapter
        .simulate_swap(
            &registration,
            &mut cache,
            Address::repeat_byte(0xc0),
            Address::repeat_byte(0xc1),
            U256::from(1_000_u64),
            &config,
        )
        .expect_err("reverting vault must error");
    assert_eq!(err, SimError::Reverted);
    assert!(asserter.read_q().is_empty(), "must be fully offline");
    Ok(())
}

// --- Balancer V2 reactive integration: cold-start -> Swap refresh -> re-quote ---

fn balancer_swap_topic() -> B256 {
    keccak256("Swap(bytes32,address,address,uint256,uint256)")
}

fn topic_address(address: Address) -> B256 {
    let mut bytes = [0u8; 32];
    bytes[12..].copy_from_slice(address.as_slice());
    B256::from(bytes)
}

fn word(value: U256) -> Vec<u8> {
    value.to_be_bytes::<32>().to_vec()
}

fn rpc_log(address: Address, topics: Vec<B256>, data: Vec<u8>, block_number: u64) -> RpcLog {
    RpcLog {
        inner: PrimitiveLog::new_unchecked(address, topics, Bytes::from(data)),
        block_hash: Some(B256::repeat_byte(block_number as u8)),
        block_number: Some(block_number),
        block_timestamp: Some(1_700_000_000 + block_number),
        transaction_hash: Some(B256::repeat_byte(0x01)),
        transaction_index: Some(0),
        log_index: Some(0),
        removed: false,
    }
}

fn included_context(block_number: u64) -> ReactiveContext {
    let block = BlockRef {
        number: block_number,
        hash: B256::repeat_byte(block_number as u8),
        parent_hash: Some(B256::repeat_byte(block_number.saturating_sub(1) as u8)),
        timestamp: Some(1_700_000_000 + block_number),
    };
    ReactiveContext {
        chain_id: Some(1),
        source: InputSource::Synthetic,
        chain_status: ChainStatus::Included {
            block: block.clone(),
            confirmations: 0,
        },
        block: Some(block),
        transaction_index: Some(0),
        log_index: Some(0),
    }
}

fn batch(input: ReactiveInput<Ethereum>, context: ReactiveContext) -> ReactiveInputBatch<Ethereum> {
    ReactiveInputBatch::new(vec![ReactiveInputRecord::new(input, context)])
}

/// Cold-start a combined mock vault, ingest a `Swap`, and assert the discovered
/// balance slot is refreshed so a subsequent `simulate_swap` reflects the change.
///
/// Before this work the `Swap` produced no cache mutation (the old routing-only
/// decode), so the re-quote would still see the stale balance.
#[tokio::test(flavor = "multi_thread")]
async fn balancer_reactive_swap_refreshes_balances_for_resim() -> Result<()> {
    let vault = Address::repeat_byte(0xe1);
    let mut pid = [0u8; 32];
    pid[..20].fill(0x44);
    let pool_id = B256::from(pid);
    let token0 = Address::repeat_byte(0xc0);
    let token1 = Address::repeat_byte(0xc1);

    let stale_balance0 = U256::from(1_000_u64);
    let fresh_balance0 = U256::from(7_777_u64);

    let (mut cache, asserter) = setup_cache_with_asserter().await?;
    install_default_account(&mut cache, Address::ZERO);
    // Seed the vault with the STALE balance0 at slot 2.
    install_balancer_quote_vault(&mut cache, vault, stale_balance0)?;
    // The verify/resync fetcher returns the discovered slots; balance slot 2
    // now returns the FRESH balance (simulating the post-swap on-chain state).
    cache.set_storage_batch_fetcher(fetcher(
        HashMap::from([
            ((vault, U256::from(0)), token_slot_word(token0)),
            ((vault, U256::from(1)), token_slot_word(token1)),
            ((vault, U256::from(2)), fresh_balance0),
            ((vault, U256::from(3)), U256::from(2_u64)),
            ((vault, U256::from(4)), U256::from(7_u64)),
        ]),
        Vec::new(),
    ));

    // 1. Cold-start: discover -> verify warms the balance slots and persists
    //    them on the metadata.
    let mut cold_registry = AdapterRegistry::new();
    cold_registry
        .register_adapter(Arc::new(BalancerV2Adapter::default()))
        .unwrap();
    let mut registration = PoolRegistration::new(PoolKey::BalancerV2(pool_id))
        .with_state_address(vault)
        .with_metadata(ProtocolMetadata::BalancerV2(BalancerV2Metadata {
            vault: Some(vault),
            ..Default::default()
        }));
    let outcome =
        cold_registry.cold_start(&mut registration, &mut cache, ColdStartPolicy::Eager)?;
    assert!(
        matches!(outcome, ColdStartOutcome::Ready(_)),
        "cold start should be Ready, got {outcome:?}"
    );
    assert_eq!(registration.status, PoolStatus::Ready);

    // The cold-start verify round already refreshed slot 2 to the fetcher's
    // value (it returns the fresh balance for every read), so re-seed the STALE
    // value to prove the *reactive Swap* is what refreshes it below.
    cache
        .db_mut()
        .insert_account_storage(vault, U256::from(2), stale_balance0)?;
    assert_eq!(
        cache.cached_storage_value(vault, U256::from(2)),
        Some(stale_balance0),
        "re-seeded stale balance0 before the reactive swap"
    );

    // The discovered balance slots must be persisted on the metadata so the
    // reactive decode can reach them.
    let balance_slots = match &registration.metadata {
        ProtocolMetadata::BalancerV2(m) => {
            assert!(
                m.balance_slots.contains(&U256::from(2)),
                "balance slot 2 must be persisted, got {:?}",
                m.balance_slots
            );
            m.balance_slots.clone()
        }
        other => panic!("expected BalancerV2 metadata, got {other:?}"),
    };
    assert!(!balance_slots.is_empty());

    // 2. Ingest a Swap log through the reactive runtime; the adapter emits a
    //    VerifySlots refresh that the runtime lowers into an executed resync,
    //    re-reading slot 2 to the fresh balance.
    let mut registry = AdapterRegistry::new();
    registry
        .register_adapter(Arc::new(BalancerV2Adapter::default()))
        .unwrap();
    let adapter = BalancerV2Adapter::default();
    let sources = adapter.event_sources(&registration);
    registration = registration.with_event_sources(sources);
    registry.register_pool(registration.clone()).unwrap();

    let mut runtime = ReactiveRuntime::<Ethereum>::new(ReactiveConfig::default());
    runtime.register_handler(Arc::new(AmmReactiveHandler::new(registry)))?;

    let mut data = word(U256::from(1_000_u64)); // amountIn
    data.extend(word(U256::from(900_u64))); // amountOut
    let log = rpc_log(
        vault,
        vec![
            balancer_swap_topic(),
            pool_id,
            topic_address(token0),
            topic_address(token1),
        ],
        data,
        100,
    );

    let report = runtime.ingest_batch_with_resync(
        &mut cache,
        batch(ReactiveInput::Log(log), included_context(100)),
    )?;
    assert_eq!(report.applied.len(), 1, "swap should produce one effect");

    // The reactive resync refreshed the discovered balance slot to the fresh value.
    assert_eq!(
        cache.cached_storage_value(vault, U256::from(2)),
        Some(fresh_balance0),
        "the reactive Swap must refresh balance slot 2 to the fresh value"
    );

    // 3. A subsequent simulate_swap now reflects the refreshed balance.
    let config = SimConfig::default();
    let quote = adapter
        .simulate_swap(
            &registration,
            &mut cache,
            token0,
            token1,
            U256::from(1_000_u64),
            &config,
        )
        .expect("post-swap quote should succeed");
    assert_eq!(
        quote.amount_out, fresh_balance0,
        "the re-simulated quote must reflect the refreshed balance"
    );

    assert!(asserter.read_q().is_empty(), "must be fully offline");
    Ok(())
}

// --- Solidly V2 offline harness ---

// Solidly's simulate_swap executes the POOL's own getAmountOut(amountIn, tokenIn)
// against the warmed pool — no router/quoter, no SimConfig target, no layout.
#[tokio::test(flavor = "multi_thread")]
async fn solidly_simulate_swap_returns_pool_quote_offline() -> Result<()> {
    let pool = Address::repeat_byte(0x71);
    let token_in = Address::repeat_byte(0x01);
    let token_out = Address::repeat_byte(0x02);
    let expected_out = U256::from(7_777_u64);

    let (mut cache, asserter) = setup_cache_with_asserter().await?;
    install_default_account(&mut cache, Address::ZERO);
    // Mock pool: getAmountOut returns sload(0); seed it.
    install_runtime(
        &mut cache,
        pool,
        include_str!("fixtures/mock_solidly_pool_runtime.hex"),
    );
    cache
        .db_mut()
        .insert_account_storage(pool, U256::ZERO, expected_out)?;

    let adapter = SolidlyV2Adapter::default();
    let registration = PoolRegistration::new(PoolKey::SolidlyV2(pool))
        .with_state_address(pool)
        .with_metadata(ProtocolMetadata::SolidlyV2(SolidlyV2Metadata {
            token0: Some(token_in),
            token1: Some(token_out),
            stable: Some(false),
            storage_layout: Some(SolidlyStorageLayout::new(
                U256::from(10_u64),
                U256::from(11_u64),
                U256::from(12_u64),
                U256::from(13_u64),
            )),
        }));

    let quote = adapter
        .simulate_swap(
            &registration,
            &mut cache,
            token_in,
            token_out,
            U256::from(1_000_u64),
            &SimConfig::default(),
        )
        .expect("solidly quote should succeed");

    assert_eq!(quote.amount_out, expected_out);
    assert!(
        asserter.read_q().is_empty(),
        "swap sim must be fully offline (no RPC)"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn solidly_simulate_swap_reverting_pool_is_reverted() -> Result<()> {
    let pool = Address::repeat_byte(0x72);

    let (mut cache, asserter) = setup_cache_with_asserter().await?;
    install_default_account(&mut cache, Address::ZERO);
    install_runtime(&mut cache, pool, REVERT_RUNTIME);

    let adapter = SolidlyV2Adapter::default();
    let registration = PoolRegistration::new(PoolKey::SolidlyV2(pool))
        .with_state_address(pool)
        .with_metadata(ProtocolMetadata::SolidlyV2(SolidlyV2Metadata::default()));

    let err = adapter
        .simulate_swap(
            &registration,
            &mut cache,
            Address::repeat_byte(0x01),
            Address::repeat_byte(0x02),
            U256::from(1_000_u64),
            &SimConfig::default(),
        )
        .expect_err("reverting pool must error");
    assert_eq!(err, SimError::Reverted);
    assert!(asserter.read_q().is_empty(), "must be fully offline");
    Ok(())
}

// --- Curve StableSwap offline harness ---

/// `simulate_swap` maps token_in/token_out to coin indices and quotes via the
/// pool's `get_dy`. The mock pool returns slot 0 for any call, so seeding slot 0
/// pins the expected output and proves the quote is fully offline.
#[tokio::test(flavor = "multi_thread")]
async fn curve_simulate_swap_returns_pool_quote_offline() -> Result<()> {
    let pool = Address::repeat_byte(0xc1);
    let dai = Address::repeat_byte(0x01);
    let usdc = Address::repeat_byte(0x02);
    let usdt = Address::repeat_byte(0x03);
    let expected_out = U256::from(999_900_u64);

    let (mut cache, asserter) = setup_cache_with_asserter().await?;
    install_default_account(&mut cache, Address::ZERO);
    install_runtime(
        &mut cache,
        pool,
        include_str!("fixtures/mock_curve_pool_runtime.hex"),
    );
    cache
        .db_mut()
        .insert_account_storage(pool, U256::ZERO, expected_out)?;

    let adapter = CurveAdapter::default();
    let registration = PoolRegistration::new(PoolKey::Curve(pool))
        .with_state_address(pool)
        .with_metadata(ProtocolMetadata::Curve(CurveMetadata {
            coins: vec![dai, usdc, usdt],
            discovered_slots: vec![U256::ZERO],
            variant: CurveVariant::StableSwap,
        }));

    // Swap DAI (index 0) -> USDC (index 1).
    let quote = adapter
        .simulate_swap(
            &registration,
            &mut cache,
            dai,
            usdc,
            U256::from(1_000_000_000_000_000_000_u128),
            &SimConfig::default(),
        )
        .expect("curve quote should succeed");

    assert_eq!(quote.amount_out, expected_out);
    assert!(
        asserter.read_q().is_empty(),
        "swap sim must be fully offline (no RPC)"
    );
    Ok(())
}

/// A token that is not one of the pool's `coins` has no index, so the quote
/// cannot be built — this is a clean error, never a panic or a wrong index.
#[tokio::test(flavor = "multi_thread")]
async fn curve_simulate_swap_token_not_in_pool_is_error() -> Result<()> {
    let pool = Address::repeat_byte(0xc2);
    let dai = Address::repeat_byte(0x01);
    let usdc = Address::repeat_byte(0x02);
    let stranger = Address::repeat_byte(0x09);

    let (mut cache, _asserter) = setup_cache_with_asserter().await?;
    install_default_account(&mut cache, Address::ZERO);
    install_runtime(
        &mut cache,
        pool,
        include_str!("fixtures/mock_curve_pool_runtime.hex"),
    );

    let adapter = CurveAdapter::default();
    let registration = PoolRegistration::new(PoolKey::Curve(pool))
        .with_state_address(pool)
        .with_metadata(ProtocolMetadata::Curve(CurveMetadata {
            coins: vec![dai, usdc],
            discovered_slots: vec![U256::ZERO],
            variant: CurveVariant::StableSwap,
        }));

    let err = adapter
        .simulate_swap(
            &registration,
            &mut cache,
            stranger,
            usdc,
            U256::from(1_000_u64),
            &SimConfig::default(),
        )
        .expect_err("token outside the pool must error");
    // Specific variant: the call must never be built/run (never Reverted).
    assert_eq!(err, SimError::MissingMetadata("Curve token not in pool"));
    Ok(())
}

/// Empty `coins` (cold-start never configured them) → simulate_swap errors
/// rather than guessing indices.
#[tokio::test(flavor = "multi_thread")]
async fn curve_simulate_swap_without_coins_is_error() -> Result<()> {
    let pool = Address::repeat_byte(0xc3);

    let (mut cache, _asserter) = setup_cache_with_asserter().await?;
    install_default_account(&mut cache, Address::ZERO);
    install_runtime(
        &mut cache,
        pool,
        include_str!("fixtures/mock_curve_pool_runtime.hex"),
    );

    let adapter = CurveAdapter::default();
    let registration = PoolRegistration::new(PoolKey::Curve(pool))
        .with_state_address(pool)
        .with_metadata(ProtocolMetadata::Curve(CurveMetadata::default()));

    let err = adapter
        .simulate_swap(
            &registration,
            &mut cache,
            Address::repeat_byte(0x01),
            Address::repeat_byte(0x02),
            U256::from(1_000_u64),
            &SimConfig::default(),
        )
        .expect_err("missing coins must error");
    assert_eq!(err, SimError::MissingMetadata("Curve coins"));
    Ok(())
}

/// A self-swap (token_in == token_out) is a clean adapter error, never built or
/// run against the pool (would otherwise revert in-EVM).
#[tokio::test(flavor = "multi_thread")]
async fn curve_simulate_swap_self_swap_is_error() -> Result<()> {
    let pool = Address::repeat_byte(0xc4);
    let dai = Address::repeat_byte(0x01);
    let usdc = Address::repeat_byte(0x02);

    let (mut cache, asserter) = setup_cache_with_asserter().await?;
    install_default_account(&mut cache, Address::ZERO);
    install_runtime(
        &mut cache,
        pool,
        include_str!("fixtures/mock_curve_pool_runtime.hex"),
    );

    let adapter = CurveAdapter::default();
    let registration = PoolRegistration::new(PoolKey::Curve(pool))
        .with_state_address(pool)
        .with_metadata(ProtocolMetadata::Curve(CurveMetadata {
            coins: vec![dai, usdc],
            discovered_slots: vec![U256::ZERO],
            variant: CurveVariant::StableSwap,
        }));

    let err = adapter
        .simulate_swap(
            &registration,
            &mut cache,
            dai,
            dai,
            U256::from(1_000_u64),
            &SimConfig::default(),
        )
        .expect_err("self-swap must error");
    assert_eq!(err, SimError::Custom("Curve token_in == token_out".into()));
    assert!(asserter.read_q().is_empty(), "must not touch the backend");
    Ok(())
}

/// CryptoSwap (Curve v2) offline sim: the variant uses `get_dy(uint256,uint256,
/// uint256)`. The selector-agnostic mock returns `sload(0)` for ANY call (so it
/// serves the uint256 ABI too — the mock cannot distinguish ABIs; the RPC parity
/// test gates the real uint256 ABI). This proves the CryptoSwap branch builds +
/// runs + decodes a quote, distinct from the StableSwap path.
#[tokio::test(flavor = "multi_thread")]
async fn curve_cryptoswap_simulate_swap_returns_pool_quote_offline() -> Result<()> {
    let pool = Address::repeat_byte(0xc6);
    let usdt = Address::repeat_byte(0x01);
    let wbtc = Address::repeat_byte(0x02);
    let weth = Address::repeat_byte(0x03);
    let expected_out = U256::from(147_348_u64);

    let (mut cache, asserter) = setup_cache_with_asserter().await?;
    install_default_account(&mut cache, Address::ZERO);
    install_runtime(
        &mut cache,
        pool,
        include_str!("fixtures/mock_curve_pool_runtime.hex"),
    );
    cache
        .db_mut()
        .insert_account_storage(pool, U256::ZERO, expected_out)?;

    let adapter = CurveAdapter::default();
    let registration = PoolRegistration::new(PoolKey::Curve(pool))
        .with_state_address(pool)
        .with_metadata(ProtocolMetadata::Curve(CurveMetadata {
            coins: vec![usdt, wbtc, weth],
            discovered_slots: vec![U256::ZERO],
            variant: CurveVariant::CryptoSwap,
        }));

    // Swap USDT (index 0) -> WBTC (index 1).
    let quote = adapter
        .simulate_swap(
            &registration,
            &mut cache,
            usdt,
            wbtc,
            U256::from(100_000_000_u64),
            &SimConfig::default(),
        )
        .expect("curve cryptoswap quote should succeed");

    assert_eq!(quote.amount_out, expected_out);
    assert!(
        asserter.read_q().is_empty(),
        "swap sim must be fully offline (no RPC)"
    );
    Ok(())
}

/// A reverting Curve pool surfaces `SimError::Reverted` (sibling-consistent with
/// the V2/V3/Balancer/Solidly reverting-target tests).
#[tokio::test(flavor = "multi_thread")]
async fn curve_simulate_swap_reverting_pool_is_reverted() -> Result<()> {
    let pool = Address::repeat_byte(0xc5);
    let dai = Address::repeat_byte(0x01);
    let usdc = Address::repeat_byte(0x02);

    let (mut cache, asserter) = setup_cache_with_asserter().await?;
    install_default_account(&mut cache, Address::ZERO);
    install_runtime(&mut cache, pool, REVERT_RUNTIME);

    let adapter = CurveAdapter::default();
    let registration = PoolRegistration::new(PoolKey::Curve(pool))
        .with_state_address(pool)
        .with_metadata(ProtocolMetadata::Curve(CurveMetadata {
            coins: vec![dai, usdc],
            discovered_slots: vec![U256::ZERO],
            variant: CurveVariant::StableSwap,
        }));

    let err = adapter
        .simulate_swap(
            &registration,
            &mut cache,
            dai,
            usdc,
            U256::from(1_000_u64),
            &SimConfig::default(),
        )
        .expect_err("reverting pool must error");
    assert_eq!(err, SimError::Reverted);
    assert!(asserter.read_q().is_empty(), "must be fully offline");
    Ok(())
}
