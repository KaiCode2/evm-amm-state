//! Deterministic coverage of the Aero (Dromos) volatile Solidly variant
//! against a mock pool: `POOL_TYPE()` identity at cold start, total-fee quote
//! selectors, exact-output quotes, the transaction context, and the legacy
//! variant's unchanged behaviour.

use std::{collections::HashMap, sync::Arc};

use alloy_eips::BlockId;
use alloy_primitives::{Address, Bytes, U256, address, hex};
use alloy_provider::{RootProvider, network::AnyNetwork};
use alloy_rpc_client::RpcClient;
use alloy_transport::mock::Asserter;
use anyhow::Result;
use evm_amm_state::adapters::storage::SolidlyStorageLayout;
use evm_amm_state::adapters::{
    AdapterRegistry, AmmAdapter, ColdStartOutcome, ColdStartPolicy, PoolKey, PoolRegistration,
    PoolStatus, ProtocolMetadata, SimConfig, SimError, SolidlyPoolVariant, SolidlyV2Adapter,
    SolidlyV2Metadata, UnsupportedReason,
};
use evm_fork_cache::cache::{EvmCache, StorageBatchFetchFn};
use revm::state::{AccountInfo, Bytecode};

const POOL: Address = address!("00000000000000000000000000000000000000a1");
const TOKEN0: Address = address!("0000000000000000000000000000000000000001");
const TOKEN1: Address = address!("0000000000000000000000000000000000000002");

async fn cache() -> (EvmCache, Asserter) {
    let asserter = Asserter::new();
    let client = RpcClient::mocked(asserter.clone());
    let provider = RootProvider::<AnyNetwork>::new(client);
    let mut cache = EvmCache::new(Arc::new(provider)).await;
    cache.set_chain_id(10);
    cache
        .db_mut()
        .insert_account_info(Address::ZERO, AccountInfo::default());
    (cache, asserter)
}

fn install(cache: &mut EvmCache, address: Address, runtime: &str) {
    let code = Bytecode::new_raw(Bytes::from(hex::decode(runtime.trim()).unwrap()));
    let code_hash = code.hash_slow();
    cache.db_mut().insert_account_info(
        address,
        AccountInfo {
            balance: U256::ZERO,
            nonce: 1,
            code: Some(code),
            code_hash,
            account_id: None,
        },
    );
}

fn fetcher(values: HashMap<(Address, U256), U256>) -> StorageBatchFetchFn {
    Arc::new(move |requests: Vec<(Address, U256)>, _block: BlockId| {
        requests
            .into_iter()
            .map(|(address, slot)| {
                (
                    address,
                    slot,
                    Ok(values.get(&(address, slot)).copied().unwrap_or_default()),
                )
            })
            .collect()
    })
}

fn address_word(address: Address) -> U256 {
    U256::from_be_slice(address.as_slice())
}

fn aero_registration() -> PoolRegistration {
    PoolRegistration::new(PoolKey::SolidlyV2(POOL))
        .with_state_address(POOL)
        .with_metadata(ProtocolMetadata::SolidlyV2(
            SolidlyV2Metadata::default()
                .with_token0(TOKEN0)
                .with_token1(TOKEN1)
                .with_aero_volatile(),
        ))
}

fn registry() -> AdapterRegistry {
    let mut registry = AdapterRegistry::new().with_code_seeding(false);
    registry
        .register_adapter(Arc::new(SolidlyV2Adapter::default()))
        .unwrap();
    registry
}

fn pool_values() -> HashMap<(Address, U256), U256> {
    let layout = SolidlyStorageLayout::aero_volatile();
    HashMap::from([
        ((POOL, layout.reserve0_slot), U256::from(17_u64)),
        ((POOL, layout.reserve1_slot), U256::from(426_u64)),
        ((POOL, layout.token0_slot), address_word(TOKEN0)),
        ((POOL, layout.token1_slot), address_word(TOKEN1)),
    ])
}

#[tokio::test(flavor = "multi_thread")]
async fn aero_volatile_cold_start_proves_pool_type_and_quotes_with_total_fee() -> Result<()> {
    let (mut cache, asserter) = cache().await;
    install(
        &mut cache,
        POOL,
        include_str!("fixtures/mock_aero_volatile_pool_runtime.hex"),
    );
    cache.set_storage_batch_fetcher(fetcher(pool_values()));
    cache
        .db_mut()
        .insert_account_storage(POOL, U256::ZERO, U256::from(111_u64))?;
    cache
        .db_mut()
        .insert_account_storage(POOL, U256::from(1), U256::from(222_u64))?;
    cache
        .db_mut()
        .insert_account_storage(POOL, U256::from(2), U256::from(333_u64))?;

    let registry = registry();
    let mut registration = aero_registration();
    let outcome = registry.cold_start(&mut registration, &mut cache, ColdStartPolicy::Eager)?;
    assert!(matches!(outcome, ColdStartOutcome::Ready(_)), "{outcome:?}");
    assert_eq!(registration.status, PoolStatus::Ready);
    let ProtocolMetadata::SolidlyV2(metadata) = &registration.metadata else {
        panic!("solidly metadata");
    };
    assert_eq!(metadata.variant, SolidlyPoolVariant::AeroVolatile);
    assert_eq!(metadata.token0, Some(TOKEN0));
    assert_eq!(metadata.token1, Some(TOKEN1));
    assert_eq!(
        metadata.storage_layout,
        Some(SolidlyStorageLayout::aero_volatile())
    );

    let adapter = SolidlyV2Adapter::default();
    let quote = adapter.simulate_swap(
        &registration,
        &mut cache,
        TOKEN1,
        TOKEN0,
        U256::from(1),
        &SimConfig::default(),
    )?;
    assert_eq!(
        quote.amount_out,
        U256::from(222_u64),
        "the Aero variant must quote through getAmountOutWithTotalFee"
    );
    let priced = SimConfig::default().with_gas_price(1_100_000_308);
    let quote = adapter.simulate_swap(
        &registration,
        &mut cache,
        TOKEN1,
        TOKEN0,
        U256::from(1),
        &priced,
    )?;
    assert_eq!(
        quote.amount_out,
        U256::from(222_u64 + 1_100_000_308),
        "tx.gasprice must reach the pool's fee path"
    );
    let exact_output = adapter.simulate_swap_exact_output(
        &registration,
        &mut cache,
        TOKEN0,
        TOKEN1,
        U256::from(1),
        &priced,
    )?;
    assert_eq!(
        exact_output.amount_in,
        U256::from(333_u64 + 1_100_000_308),
        "exact output must quote through getAmountInWithTotalFee"
    );
    assert!(asserter.read_q().is_empty(), "quotes must be fully offline");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn legacy_variant_keeps_get_amount_out_and_has_no_exact_output() -> Result<()> {
    let (mut cache, _asserter) = cache().await;
    install(
        &mut cache,
        POOL,
        include_str!("fixtures/mock_aero_volatile_pool_runtime.hex"),
    );
    cache
        .db_mut()
        .insert_account_storage(POOL, U256::ZERO, U256::from(111_u64))?;
    cache
        .db_mut()
        .insert_account_storage(POOL, U256::from(1), U256::from(222_u64))?;
    let registration = PoolRegistration::new(PoolKey::SolidlyV2(POOL))
        .with_state_address(POOL)
        .with_metadata(ProtocolMetadata::SolidlyV2(
            SolidlyV2Metadata::default()
                .with_token0(TOKEN0)
                .with_token1(TOKEN1)
                .with_stable(false)
                .with_storage_layout(SolidlyStorageLayout::aero_volatile()),
        ));
    let adapter = SolidlyV2Adapter::default();
    let quote = adapter.simulate_swap(
        &registration,
        &mut cache,
        TOKEN1,
        TOKEN0,
        U256::from(1),
        &SimConfig::default().with_gas_price(5),
    )?;
    assert_eq!(
        quote.amount_out,
        U256::from(111_u64),
        "a legacy registration keeps quoting through getAmountOut"
    );
    assert!(matches!(
        adapter.simulate_swap_exact_output(
            &registration,
            &mut cache,
            TOKEN0,
            TOKEN1,
            U256::from(1),
            &SimConfig::default(),
        ),
        Err(SimError::Unsupported(_))
    ));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn aero_variant_on_a_legacy_pool_fails_cold_start_closed() -> Result<()> {
    // The legacy Solidly mock has no `POOL_TYPE()`; the identity call reverts.
    let (mut cache, _asserter) = cache().await;
    install(
        &mut cache,
        POOL,
        include_str!("fixtures/mock_solidly_pool_runtime.hex"),
    );
    cache.set_storage_batch_fetcher(fetcher(pool_values()));
    let registry = registry();
    let mut registration = aero_registration();
    let outcome = registry.cold_start(&mut registration, &mut cache, ColdStartPolicy::Eager)?;
    match outcome {
        ColdStartOutcome::Unsupported(UnsupportedReason::Custom(reason)) => {
            assert!(reason.contains("POOL_TYPE"), "{reason}");
        }
        other => panic!("expected a fail-closed identity verdict, got {other:?}"),
    }
    assert_eq!(registration.status, PoolStatus::Degraded);
    Ok(())
}

#[test]
fn cached_mev_threshold_cannot_replace_an_exact_block_fetch() {
    use evm_amm_state::adapters::{
        CallOutcome, ColdStartCallResult, ColdStartResults, ColdStartRunReport, RepairAction,
        SlotFetch, SlotOutcome, StateView,
    };

    // The previous block charged the low fee. Even a resident nonzero word
    // cannot prove the Optimism MEV threshold at the candidate block.
    struct PriorState;
    impl StateView for PriorState {
        fn storage(&self, _: Address, _: U256) -> Option<U256> {
            Some(U256::from(1))
        }
    }
    let fee_cell = (
        address!("5de55ebe5b789bd309809c7969334c2c16e64eaf"),
        U256::from(2),
    );
    let adapter = SolidlyV2Adapter::default();
    let reviewed_pool = address!("c71b571aa9f06bfa096c24e9df5c45fc4fab4151");
    for unavailable in [
        Some(SlotFetch::FetchFailed {
            reason: "archive read failed".into(),
        }),
        Some(SlotFetch::NotAttempted),
        None,
    ] {
        let mut pool = PoolRegistration::new(PoolKey::SolidlyV2(reviewed_pool)).with_metadata(
            ProtocolMetadata::SolidlyV2(SolidlyV2Metadata::default().with_aero_volatile()),
        );
        let mut planner = adapter
            .cold_start_planner(&pool, ColdStartPolicy::Strict)
            .unwrap();
        let plan = planner.initial_plan(&PriorState);
        let mut results = ColdStartResults::default();
        for (address, slot) in plan.verify {
            let fetch = if (address, slot) == fee_cell {
                unavailable.clone()
            } else {
                Some(SlotFetch::Value(U256::from(1)))
            };
            if let Some(fetch) = fetch {
                results.fetched.push(SlotOutcome::new(address, slot, fetch));
            }
        }
        results.discovered.push(ColdStartCallResult::new(
            CallOutcome::Success {
                output: Bytes::from(alloy_primitives::FixedBytes::<32>::right_padding_from(
                    b"V2_VOLATILE",
                )),
                gas_used: 0,
            },
            Default::default(),
        ));
        planner.on_results(&results, &PriorState);
        let outcome = planner.finish(&mut pool, &ColdStartRunReport::default());
        assert!(
            matches!(outcome, ColdStartOutcome::NeedsRepair(_, RepairAction::VerifySlots(ref cells)) if cells == &[fee_cell]),
            "{outcome:?}"
        );
        assert_eq!(pool.status, PoolStatus::Degraded);
    }
}
