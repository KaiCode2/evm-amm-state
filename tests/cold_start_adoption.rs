//! Phase A4 slice 1 — MANAGER-AUTHORED acceptance tests for the cold-start
//! adoption (V2/V3 planners over `EvmCache::run_cold_start`).
//!
//! These pin the new `AdapterRegistry::cold_start` contract and the archive-miss
//! improvement (per-slot `SlotFetch` replaces the `cached_storage(..).is_none()`
//! proxy). The implementation agent must make these pass WITHOUT modifying them.
//! All tests run fully offline over a mocked provider + stub fetcher.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use alloy_eips::BlockId;
use alloy_primitives::{Address, U256};
use alloy_provider::{RootProvider, network::AnyNetwork};
use alloy_rpc_client::RpcClient;
use alloy_transport::mock::Asserter;
use anyhow::{Result, anyhow};

use evm_amm_state::adapters::storage::{
    V2_RESERVES_SLOT, V2_TOKEN0_SLOT, V2_TOKEN1_SLOT, V3StorageLayout,
};
use evm_amm_state::adapters::{
    AdapterRegistry, ColdStartOutcome, ColdStartPolicy, DeferredWork, PoolKey, PoolRegistration,
    PoolStatus, ProtocolMetadata, UniswapV2Adapter, UniswapV2Metadata, UniswapV3Adapter,
    V3Metadata,
};
use evm_fork_cache::cache::{EvmCache, StorageBatchFetchFn};

// --- helpers (kept local so this manager file owns its fixtures) ---

async fn setup_cache() -> Result<EvmCache> {
    let asserter = Asserter::new();
    let client = RpcClient::mocked(asserter);
    let provider = RootProvider::<AnyNetwork>::new(client);
    Ok(EvmCache::new(Arc::new(provider)).await)
}

/// A fetcher that returns a hard `Err` for `fail` slots (archive miss) and
/// `Ok(value-or-ZERO)` otherwise.
fn fetcher_with_failures(
    values: HashMap<(Address, U256), U256>,
    fail: Vec<(Address, U256)>,
) -> StorageBatchFetchFn {
    let fail: HashSet<(Address, U256)> = fail.into_iter().collect();
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

fn reserves_slot(reserve0: U256, reserve1: U256, timestamp: U256) -> U256 {
    reserve0 | (reserve1 << 112) | (timestamp << 224)
}

fn v3_slot0_word(sqrt_price: U256, tick: i32, obs_high: U256) -> U256 {
    let tick24 = U256::from((tick as u32) & 0x00FF_FFFF);
    sqrt_price | (tick24 << 160) | (obs_high << 184)
}

fn v2_registry() -> AdapterRegistry {
    let mut registry = AdapterRegistry::new();
    registry
        .register_adapter(Arc::new(UniswapV2Adapter::default()))
        .unwrap();
    registry
}

fn v3_registry() -> AdapterRegistry {
    let mut registry = AdapterRegistry::new();
    registry
        .register_adapter(Arc::new(UniswapV3Adapter::default()))
        .unwrap();
    registry
}

fn repair_of(outcome: &ColdStartOutcome) -> Option<&evm_amm_state::adapters::RepairAction> {
    match outcome {
        ColdStartOutcome::NeedsRepair(_, repair) => Some(repair),
        _ => None,
    }
}

// --- Uniswap V2 ---

#[tokio::test]
async fn v2_cold_start_value_reserves_is_ready() -> Result<()> {
    let pool = Address::repeat_byte(0x11);
    let token0 = Address::repeat_byte(0xa0);
    let token1 = Address::repeat_byte(0xa1);
    let mut cache = setup_cache().await?;
    cache.set_storage_batch_fetcher(fetcher_with_failures(
        HashMap::from([
            ((pool, V2_TOKEN0_SLOT), token_slot_word(token0)),
            ((pool, V2_TOKEN1_SLOT), token_slot_word(token1)),
            (
                (pool, V2_RESERVES_SLOT),
                reserves_slot(U256::from(10_u64), U256::from(20_u64), U256::ZERO),
            ),
        ]),
        Vec::new(),
    ));

    let registry = v2_registry();
    // Config-supplied fee must survive the cold-start (V2 has no on-chain fee).
    let mut registration = PoolRegistration::new(PoolKey::UniswapV2(pool))
        .with_state_address(pool)
        .with_metadata(ProtocolMetadata::UniswapV2(UniswapV2Metadata {
            fee_bps: Some(30),
            ..Default::default()
        }));

    let outcome = registry.cold_start(&mut registration, &mut cache, ColdStartPolicy::Eager)?;

    assert!(
        matches!(outcome, ColdStartOutcome::Ready(_)),
        "got {outcome:?}"
    );
    assert_eq!(registration.status, PoolStatus::Ready);
    assert!(cache.cached_storage_value(pool, V2_RESERVES_SLOT).is_some());
    match registration.metadata {
        ProtocolMetadata::UniswapV2(ref m) => {
            assert_eq!(m.token0, Some(token0));
            assert_eq!(m.token1, Some(token1));
            assert_eq!(m.fee_bps, Some(30), "config fee_bps must be preserved");
        }
        ref other => panic!("expected merged UniswapV2 metadata, got {other:?}"),
    }
    Ok(())
}

#[tokio::test]
async fn v2_cold_start_zero_vs_failed_reserves_are_distinct_repairs() -> Result<()> {
    let pool = Address::repeat_byte(0x12);

    // Case A: reserves slot reads a genuine on-chain ZERO (degenerate pool).
    let mut cache_zero = setup_cache().await?;
    cache_zero.set_storage_batch_fetcher(fetcher_with_failures(HashMap::new(), Vec::new()));
    let registry = v2_registry();
    let mut reg_zero = PoolRegistration::new(PoolKey::UniswapV2(pool)).with_state_address(pool);
    let zero_outcome =
        registry.cold_start(&mut reg_zero, &mut cache_zero, ColdStartPolicy::Eager)?;

    // Case B: reserves slot FAILS to fetch (archive / historical miss).
    let mut cache_fail = setup_cache().await?;
    cache_fail.set_storage_batch_fetcher(fetcher_with_failures(
        HashMap::new(),
        vec![(pool, V2_RESERVES_SLOT)],
    ));
    let mut reg_fail = PoolRegistration::new(PoolKey::UniswapV2(pool)).with_state_address(pool);
    let fail_outcome =
        registry.cold_start(&mut reg_fail, &mut cache_fail, ColdStartPolicy::Eager)?;

    // Both surface a repair, but they must be DISTINGUISHABLE — the whole point
    // of replacing the is_none() proxy with the per-slot SlotFetch classification.
    assert!(
        matches!(zero_outcome, ColdStartOutcome::NeedsRepair(_, _)),
        "genuine-zero reserves should need repair, got {zero_outcome:?}"
    );
    assert!(
        matches!(fail_outcome, ColdStartOutcome::NeedsRepair(_, _)),
        "archive-miss reserves should need repair, got {fail_outcome:?}"
    );
    assert_ne!(
        repair_of(&zero_outcome),
        repair_of(&fail_outcome),
        "a genuine zero and an archive miss must produce different repairs"
    );
    Ok(())
}

#[tokio::test]
async fn v2_cold_start_lazy_defers_exactly_what_eager_warms() -> Result<()> {
    let pool = Address::repeat_byte(0x13);
    let token0 = Address::repeat_byte(0xb0);
    let token1 = Address::repeat_byte(0xb1);
    let seed = || {
        HashMap::from([
            ((pool, V2_TOKEN0_SLOT), token_slot_word(token0)),
            ((pool, V2_TOKEN1_SLOT), token_slot_word(token1)),
            (
                (pool, V2_RESERVES_SLOT),
                reserves_slot(U256::from(1_u64), U256::from(2_u64), U256::ZERO),
            ),
        ])
    };
    let registry = v2_registry();

    // Eager warms 6/7/8.
    let mut cache_eager = setup_cache().await?;
    cache_eager.set_storage_batch_fetcher(fetcher_with_failures(seed(), Vec::new()));
    let mut reg_eager = PoolRegistration::new(PoolKey::UniswapV2(pool)).with_state_address(pool);
    registry.cold_start(&mut reg_eager, &mut cache_eager, ColdStartPolicy::Eager)?;
    assert!(
        cache_eager
            .cached_storage_value(pool, V2_TOKEN0_SLOT)
            .is_some()
    );
    assert!(
        cache_eager
            .cached_storage_value(pool, V2_TOKEN1_SLOT)
            .is_some()
    );

    // Lazy warms 8 now and defers the token slots Eager warmed eagerly.
    let mut cache_lazy = setup_cache().await?;
    cache_lazy.set_storage_batch_fetcher(fetcher_with_failures(seed(), Vec::new()));
    let mut reg_lazy = PoolRegistration::new(PoolKey::UniswapV2(pool)).with_state_address(pool);
    let lazy = registry.cold_start(&mut reg_lazy, &mut cache_lazy, ColdStartPolicy::Lazy)?;

    let deferred = match lazy {
        ColdStartOutcome::ReadyWithDeferred(_, deferred) => deferred,
        other => panic!("Lazy should be ReadyWithDeferred, got {other:?}"),
    };
    let deferred_slots: HashSet<(Address, U256)> = deferred
        .iter()
        .flat_map(|w| match w {
            DeferredWork::VerifySlots(slots) => slots.clone(),
            _ => Vec::new(),
        })
        .collect();
    assert!(deferred_slots.contains(&(pool, V2_TOKEN0_SLOT)));
    assert!(deferred_slots.contains(&(pool, V2_TOKEN1_SLOT)));
    assert_eq!(
        cache_lazy.cached_storage_value(pool, V2_TOKEN0_SLOT),
        None,
        "Lazy must not warm token slots up-front"
    );
    assert!(
        cache_lazy
            .cached_storage_value(pool, V2_RESERVES_SLOT)
            .is_some()
    );
    Ok(())
}

// --- Uniswap V3 ---

#[tokio::test]
async fn v3_cold_start_ready_warms_slot0_and_liquidity() -> Result<()> {
    let pool = Address::repeat_byte(0x21);
    let layout = V3StorageLayout::uniswap(60);
    let mut cache = setup_cache().await?;
    cache.set_storage_batch_fetcher(fetcher_with_failures(
        HashMap::from([
            (
                (pool, layout.slot0_slot),
                v3_slot0_word(U256::from(99_u64), 0, U256::ZERO),
            ),
            ((pool, layout.liquidity_slot), U256::from(5_u64)),
        ]),
        Vec::new(),
    ));

    let registry = v3_registry();
    let mut registration = PoolRegistration::new(PoolKey::UniswapV3(pool))
        .with_state_address(pool)
        .with_metadata(ProtocolMetadata::UniswapV3(V3Metadata {
            storage_layout: Some(layout),
            tick_spacing: Some(60),
            ..Default::default()
        }));

    let outcome = registry.cold_start(&mut registration, &mut cache, ColdStartPolicy::Eager)?;

    assert!(
        matches!(outcome, ColdStartOutcome::Ready(_)),
        "got {outcome:?}"
    );
    assert_eq!(registration.status, PoolStatus::Ready);
    assert!(
        cache
            .cached_storage_value(pool, layout.slot0_slot)
            .is_some()
    );
    assert!(
        cache
            .cached_storage_value(pool, layout.liquidity_slot)
            .is_some()
    );
    Ok(())
}

#[tokio::test]
async fn v3_cold_start_missing_layout_is_unsupported() -> Result<()> {
    let pool = Address::repeat_byte(0x22);
    let mut cache = setup_cache().await?;
    cache.set_storage_batch_fetcher(fetcher_with_failures(HashMap::new(), Vec::new()));

    let registry = v3_registry();
    // No storage_layout and no tick_spacing -> layout cannot resolve.
    let mut registration = PoolRegistration::new(PoolKey::UniswapV3(pool))
        .with_state_address(pool)
        .with_metadata(ProtocolMetadata::UniswapV3(V3Metadata::default()));

    let outcome = registry.cold_start(&mut registration, &mut cache, ColdStartPolicy::Eager)?;
    assert!(
        matches!(outcome, ColdStartOutcome::Unsupported(_)),
        "missing layout should be Unsupported, got {outcome:?}"
    );
    Ok(())
}

#[tokio::test]
async fn v3_cold_start_failed_slot0_needs_repair() -> Result<()> {
    let pool = Address::repeat_byte(0x23);
    let layout = V3StorageLayout::uniswap(60);
    let mut cache = setup_cache().await?;
    // slot0 fetch fails (archive miss); slot0 is the mandatory slot.
    cache.set_storage_batch_fetcher(fetcher_with_failures(
        HashMap::new(),
        vec![(pool, layout.slot0_slot)],
    ));

    let registry = v3_registry();
    let mut registration = PoolRegistration::new(PoolKey::UniswapV3(pool))
        .with_state_address(pool)
        .with_metadata(ProtocolMetadata::UniswapV3(V3Metadata {
            storage_layout: Some(layout),
            tick_spacing: Some(60),
            ..Default::default()
        }));

    let outcome = registry.cold_start(&mut registration, &mut cache, ColdStartPolicy::Eager)?;
    assert!(
        matches!(outcome, ColdStartOutcome::NeedsRepair(_, _)),
        "an unfetchable slot0 must need repair, got {outcome:?}"
    );
    Ok(())
}
