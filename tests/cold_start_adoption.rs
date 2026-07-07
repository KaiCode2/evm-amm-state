//! Phase A4 slice 1 — MANAGER-AUTHORED acceptance tests for the cold-start
//! adoption (V2/V3 planners over `EvmCache::run_cold_start`).
//!
//! These pin the new `AdapterRegistry::cold_start` contract and the archive-miss
//! improvement (per-slot `SlotFetch` replaces the `cached_storage(..).is_none()`
//! proxy). The implementation agent must make these pass WITHOUT modifying them.
//! All tests run fully offline over a mocked provider + stub fetcher.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use alloy_eips::BlockId;
use alloy_primitives::{Address, B256, Bytes, U256, hex};
use alloy_provider::{RootProvider, network::AnyNetwork};
use alloy_rpc_client::RpcClient;
use alloy_rpc_types_eth::{AccessList, AccessListItem, AccessListResult};
use alloy_transport::mock::Asserter;
use anyhow::Result;

use evm_amm_state::adapters::storage::SolidlyStorageLayout;
use evm_amm_state::adapters::storage::{
    V2_RESERVES_SLOT, V2_TOKEN0_SLOT, V2_TOKEN1_SLOT, V3StorageLayout,
    v3_tick_bitmap_storage_key_with_base, v3_tick_info_storage_keys_with_base, v3_word_position,
};
use evm_amm_state::adapters::{
    AdapterRegistry, BalancerV2Adapter, BalancerV2Metadata, ColdStartOutcome, ColdStartPolicy,
    ConcentratedLiquidityAdapter, CurveAdapter, CurveMetadata, CurveVariant, DeferredWork, PoolKey,
    PoolRegistration, PoolStatus, ProtocolMetadata, RepairAction, SolidlyV2Adapter,
    SolidlyV2Metadata, UniswapV2Adapter, UniswapV2Metadata, UnsupportedReason,
    V3ImmutablePatchValues, V3Metadata, uniswap_v2_pair_runtime_code_hash, uniswap_v3_code_seed,
    uniswap_v3_max_liquidity_per_tick,
};
use evm_fork_cache::AccountFieldsSample;
use evm_fork_cache::cache::{AccountFieldsFetchFn, CodeSeedState, EvmCache, StorageBatchFetchFn};
use revm::state::{AccountInfo, Bytecode};

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
    Arc::new(move |requests: Vec<(Address, U256)>, _block: BlockId| {
        requests
            .into_iter()
            .map(|(address, slot)| {
                if fail.contains(&(address, slot)) {
                    (
                        address,
                        slot,
                        Err(evm_fork_cache::StorageFetchError::custom("archive miss")),
                    )
                } else {
                    (
                        address,
                        slot,
                        Ok(values.get(&(address, slot)).copied().unwrap_or_default()),
                    )
                }
            })
            .collect()
    })
}

fn account_fields_fetcher(
    values: HashMap<Address, (U256, B256)>,
    calls: Arc<AtomicUsize>,
) -> AccountFieldsFetchFn {
    Arc::new(move |addresses: Vec<Address>, _block: BlockId| {
        calls.fetch_add(1, Ordering::SeqCst);
        Ok(addresses
            .into_iter()
            .filter_map(|address| {
                values.get(&address).map(|(balance, code_hash)| {
                    (
                        address,
                        AccountFieldsSample {
                            balance: *balance,
                            code_hash: *code_hash,
                        },
                    )
                })
            })
            .collect())
    })
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
        .register_adapter(Arc::new(ConcentratedLiquidityAdapter::default()))
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
    let expected_hash = uniswap_v2_pair_runtime_code_hash();
    cache.set_account_fields_fetcher(account_fields_fetcher(
        HashMap::from([(pool, (U256::ZERO, expected_hash))]),
        Arc::new(AtomicUsize::new(0)),
    ));

    let registry = v2_registry();
    // Config-supplied fee must survive the cold-start (V2 has no on-chain fee).
    let mut registration = PoolRegistration::new(PoolKey::UniswapV2(pool))
        .with_state_address(pool)
        .with_metadata(ProtocolMetadata::UniswapV2(
            UniswapV2Metadata::default().with_fee_bps(30),
        ));

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

// A Lazy cold-start records its token slots as deferred work but does not warm
// them; `run_deferred` must execute that deferred work and warm them.
#[tokio::test]
async fn v2_run_deferred_warms_lazy_deferred_slots() -> Result<()> {
    let pool = Address::repeat_byte(0x14);
    let token0 = Address::repeat_byte(0xb0);
    let token1 = Address::repeat_byte(0xb1);
    let mut cache = setup_cache().await?;
    cache.set_storage_batch_fetcher(fetcher_with_failures(
        HashMap::from([
            ((pool, V2_TOKEN0_SLOT), token_slot_word(token0)),
            ((pool, V2_TOKEN1_SLOT), token_slot_word(token1)),
            (
                (pool, V2_RESERVES_SLOT),
                reserves_slot(U256::from(1_u64), U256::from(2_u64), U256::ZERO),
            ),
        ]),
        Vec::new(),
    ));

    let registry = v2_registry();
    let mut registration = PoolRegistration::new(PoolKey::UniswapV2(pool)).with_state_address(pool);
    let outcome = registry.cold_start(&mut registration, &mut cache, ColdStartPolicy::Lazy)?;
    let deferred = match outcome {
        ColdStartOutcome::ReadyWithDeferred(_, d) => d,
        other => panic!("Lazy should be ReadyWithDeferred, got {other:?}"),
    };
    // Lazy did not warm the token slots up-front.
    assert_eq!(cache.cached_storage_value(pool, V2_TOKEN0_SLOT), None);

    // Drive the deferred work; the deferred token slots are now warmed.
    registry.run_deferred(&deferred, &mut cache)?;

    assert!(
        cache.cached_storage_value(pool, V2_TOKEN0_SLOT).is_some(),
        "run_deferred must warm deferred token0"
    );
    assert!(
        cache.cached_storage_value(pool, V2_TOKEN1_SLOT).is_some(),
        "run_deferred must warm deferred token1"
    );
    assert!(
        cache.cached_storage_value(pool, V2_RESERVES_SLOT).is_some(),
        "reserves stay warm"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn v2_cold_start_seeds_and_verifies_runtime_bytecode_once() -> Result<()> {
    let pool = Address::repeat_byte(0x15);
    let token0 = Address::repeat_byte(0xb2);
    let token1 = Address::repeat_byte(0xb3);
    let expected_hash = uniswap_v2_pair_runtime_code_hash();

    let mut cache = setup_cache().await?;
    cache.set_storage_batch_fetcher(fetcher_with_failures(
        HashMap::from([
            ((pool, V2_TOKEN0_SLOT), token_slot_word(token0)),
            ((pool, V2_TOKEN1_SLOT), token_slot_word(token1)),
            (
                (pool, V2_RESERVES_SLOT),
                reserves_slot(U256::from(3_u64), U256::from(4_u64), U256::ZERO),
            ),
        ]),
        Vec::new(),
    ));
    let account_field_calls = Arc::new(AtomicUsize::new(0));
    cache.set_account_fields_fetcher(account_fields_fetcher(
        HashMap::from([(pool, (U256::from(99_u64), expected_hash))]),
        account_field_calls.clone(),
    ));

    let registry = v2_registry();
    let mut registration = PoolRegistration::new(PoolKey::UniswapV2(pool))
        .with_state_address(pool)
        .with_metadata(ProtocolMetadata::UniswapV2(
            UniswapV2Metadata::default().with_fee_bps(30),
        ));

    let first = registry.cold_start(&mut registration, &mut cache, ColdStartPolicy::Eager)?;
    assert!(matches!(first, ColdStartOutcome::Ready(_)), "got {first:?}");
    assert_eq!(account_field_calls.load(Ordering::SeqCst), 1);
    assert!(matches!(
        cache.code_seed_state(&pool),
        Some(CodeSeedState::Verified { code_hash, .. }) if *code_hash == expected_hash
    ));

    let second = registry.cold_start(&mut registration, &mut cache, ColdStartPolicy::Eager)?;
    assert!(
        matches!(second, ColdStartOutcome::Ready(_)),
        "got {second:?}"
    );
    assert_eq!(
        account_field_calls.load(Ordering::SeqCst),
        1,
        "verified code seeds must not be reverified on later cold-starts"
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
        .with_metadata(ProtocolMetadata::UniswapV3(
            V3Metadata::default()
                .with_storage_layout(layout)
                .with_tick_spacing(60),
        ));

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

#[tokio::test(flavor = "multi_thread")]
async fn v3_bytecode_template_patches_immutables_with_explicit_factory() -> Result<()> {
    let pool = Address::repeat_byte(0x2c);
    let factory = Address::repeat_byte(0xf0);
    let token0 = Address::repeat_byte(0xc2);
    let token1 = Address::repeat_byte(0xc3);
    let fee = 500u32;
    let tick_spacing = 60;
    let seed = uniswap_v3_code_seed(
        pool,
        &V3ImmutablePatchValues {
            pool_address: Some(pool),
            factory: Some(factory),
            token0: Some(token0),
            token1: Some(token1),
            fee: Some(fee),
            tick_spacing: Some(tick_spacing),
            max_liquidity_per_tick: uniswap_v3_max_liquidity_per_tick(tick_spacing),
        },
    )?;
    let expected_hash = seed.code_hash;

    let mut cache = setup_cache().await?;
    cache.set_account_fields_fetcher(account_fields_fetcher(
        HashMap::from([(pool, (U256::from(1_u64), expected_hash))]),
        Arc::new(AtomicUsize::new(0)),
    ));
    cache.seed_account_code(seed.address, seed.runtime_bytecode)?;
    let report = cache.verify_code_seeds()?;
    assert_eq!(report.verified, vec![pool]);
    assert!(matches!(
        cache.code_seed_state(&pool),
        Some(CodeSeedState::Verified { code_hash, .. }) if *code_hash == expected_hash
    ));
    Ok(())
}

/// The bitmap bit for `tick` within its word: `floor(tick/spacing) mod 256`.
fn v3_bit(tick: i32, spacing: i32) -> U256 {
    let bit = tick.div_euclid(spacing).rem_euclid(256) as u32;
    U256::from(1) << bit
}

// Eager cold-start must warm a bounded WINDOW of neighbouring tick-bitmap words
// (and their initialized ticks), not just the current word — so a moderate
// tick-crossing swap is offline-pre-warmed. Currently only the current word is
// warmed, so the W0±1 bitmap + their tick-info slots are unfetched (None) -> red.
#[tokio::test]
async fn v3_cold_start_warms_neighbouring_tick_words() -> Result<()> {
    let pool = Address::repeat_byte(0x24);
    let layout = V3StorageLayout::uniswap(60);
    let spacing = 60i32;

    // Current tick 0 -> word 0; neighbours -1 and +1.
    let w0 = v3_word_position(0, spacing);
    let key_w0 = v3_tick_bitmap_storage_key_with_base(w0, layout.tick_bitmap_base_slot);
    let key_wp1 = v3_tick_bitmap_storage_key_with_base(w0 + 1, layout.tick_bitmap_base_slot);
    let key_wm1 = v3_tick_bitmap_storage_key_with_base(w0 - 1, layout.tick_bitmap_base_slot);

    // One initialized tick per word (self-checked placement).
    let tick_w0 = 60; // word 0, bit 1
    let tick_wp1 = 256 * 60; // word +1, bit 0
    let tick_wm1 = -60; // word -1, bit 255
    assert_eq!(v3_word_position(tick_w0, spacing), w0);
    assert_eq!(v3_word_position(tick_wp1, spacing), w0 + 1);
    assert_eq!(v3_word_position(tick_wm1, spacing), w0 - 1);

    let info_w0 = v3_tick_info_storage_keys_with_base(tick_w0, layout.ticks_base_slot);
    let info_wp1 = v3_tick_info_storage_keys_with_base(tick_wp1, layout.ticks_base_slot);
    let info_wm1 = v3_tick_info_storage_keys_with_base(tick_wm1, layout.ticks_base_slot);

    let mut cache = setup_cache().await?;
    cache.set_storage_batch_fetcher(fetcher_with_failures(
        HashMap::from([
            (
                (pool, layout.slot0_slot),
                v3_slot0_word(U256::from(99_u64), 0, U256::ZERO),
            ),
            ((pool, layout.liquidity_slot), U256::from(5_u64)),
            ((pool, key_w0), v3_bit(tick_w0, spacing)),
            ((pool, key_wp1), v3_bit(tick_wp1, spacing)),
            ((pool, key_wm1), v3_bit(tick_wm1, spacing)),
            ((pool, info_w0[0]), U256::from(1_u64)),
            ((pool, info_w0[1]), U256::from(1_u64)),
            ((pool, info_w0[2]), U256::from(1_u64)),
            ((pool, info_w0[3]), U256::from(1_u64)),
            ((pool, info_wp1[0]), U256::from(1_u64)),
            ((pool, info_wp1[3]), U256::from(1_u64)),
            ((pool, info_wm1[0]), U256::from(1_u64)),
            ((pool, info_wm1[3]), U256::from(1_u64)),
        ]),
        Vec::new(),
    ));

    let registry = v3_registry();
    let mut registration = PoolRegistration::new(PoolKey::UniswapV3(pool))
        .with_state_address(pool)
        .with_metadata(ProtocolMetadata::UniswapV3(
            V3Metadata::default()
                .with_storage_layout(layout)
                .with_tick_spacing(60),
        ));

    let outcome = registry.cold_start(&mut registration, &mut cache, ColdStartPolicy::Eager)?;
    assert!(
        matches!(outcome, ColdStartOutcome::Ready(_)),
        "got {outcome:?}"
    );

    // Current word still warmed (regression).
    assert!(cache.cached_storage_value(pool, key_w0).is_some());
    assert!(cache.cached_storage_value(pool, info_w0[0]).is_some());
    // All FOUR Tick.Info words of an initialized tick are warmed: a tick-crossing
    // quote also reads feeGrowthOutside{0,1}X128 (words 1/2), so warming only
    // {0, 3} would force a lazy fetch mid-quote (not fully offline).
    assert!(cache.cached_storage_value(pool, info_w0[1]).is_some());
    assert!(cache.cached_storage_value(pool, info_w0[2]).is_some());
    assert!(cache.cached_storage_value(pool, info_w0[3]).is_some());
    // Neighbouring words + their initialized ticks warmed (the new behaviour).
    assert!(
        cache.cached_storage_value(pool, key_wp1).is_some(),
        "word +1 bitmap must be warmed"
    );
    assert!(
        cache.cached_storage_value(pool, key_wm1).is_some(),
        "word -1 bitmap must be warmed"
    );
    assert!(
        cache.cached_storage_value(pool, info_wp1[0]).is_some(),
        "word +1 tick info must be warmed"
    );
    assert!(
        cache.cached_storage_value(pool, info_wm1[0]).is_some(),
        "word -1 tick info must be warmed"
    );
    Ok(())
}

// --- Per-pool tick-warm radius (V3Metadata.warm_word_radius) ---
//
// The cold-start tick-warm window is ±`V3_TICK_WORD_RADIUS` (default 2) bitmap
// words around the current word. `V3Metadata.warm_word_radius: Option<i16>` lets
// a consumer widen (or narrow) that window per pool; `None` keeps the default.
// These two tests pin both directions.

// A configured `warm_word_radius: Some(4)` must warm a bitmap word 4 words out
// (and its initialized ticks) — a word the default ±2 window would NOT reach.
#[tokio::test]
async fn v3_cold_start_respects_wider_configured_radius() -> Result<()> {
    let pool = Address::repeat_byte(0x2A);
    let layout = V3StorageLayout::uniswap(60);
    let spacing = 60i32;

    // Current tick 0 -> word 0. Target word +4 is outside the default ±2 window.
    let w0 = v3_word_position(0, spacing);
    let tick_w0 = 60; // word 0, bit 1 (current-word initialized tick)
    let tick_wp4 = 4 * 256 * 60; // word +4, bit 0
    assert_eq!(v3_word_position(tick_w0, spacing), w0);
    assert_eq!(v3_word_position(tick_wp4, spacing), w0 + 4);

    let key_wp4 = v3_tick_bitmap_storage_key_with_base(w0 + 4, layout.tick_bitmap_base_slot);
    let info_w0 = v3_tick_info_storage_keys_with_base(tick_w0, layout.ticks_base_slot);
    let info_wp4 = v3_tick_info_storage_keys_with_base(tick_wp4, layout.ticks_base_slot);

    let mut cache = setup_cache().await?;
    cache.set_storage_batch_fetcher(fetcher_with_failures(
        HashMap::from([
            (
                (pool, layout.slot0_slot),
                v3_slot0_word(U256::from(99_u64), 0, U256::ZERO),
            ),
            ((pool, layout.liquidity_slot), U256::from(5_u64)),
            (
                (
                    pool,
                    v3_tick_bitmap_storage_key_with_base(w0, layout.tick_bitmap_base_slot),
                ),
                v3_bit(tick_w0, spacing),
            ),
            ((pool, key_wp4), v3_bit(tick_wp4, spacing)),
            ((pool, info_w0[0]), U256::from(1_u64)),
            ((pool, info_w0[3]), U256::from(1_u64)),
            ((pool, info_wp4[0]), U256::from(1_u64)),
            ((pool, info_wp4[3]), U256::from(1_u64)),
        ]),
        Vec::new(),
    ));

    let registry = v3_registry();
    let mut registration = PoolRegistration::new(PoolKey::UniswapV3(pool))
        .with_state_address(pool)
        .with_metadata(ProtocolMetadata::UniswapV3(
            V3Metadata::default()
                .with_storage_layout(layout)
                .with_tick_spacing(60)
                .with_warm_word_radius(4),
        ));

    let outcome = registry.cold_start(&mut registration, &mut cache, ColdStartPolicy::Eager)?;
    assert!(
        matches!(outcome, ColdStartOutcome::Ready(_)),
        "got {outcome:?}"
    );
    assert!(
        cache.cached_storage_value(pool, key_wp4).is_some(),
        "word +4 bitmap must be warmed under warm_word_radius = Some(4)"
    );
    assert!(
        cache.cached_storage_value(pool, info_wp4[0]).is_some(),
        "word +4 tick info must be warmed under warm_word_radius = Some(4)"
    );
    Ok(())
}

// `warm_word_radius: None` must preserve the default ±2 window exactly: a word at
// +2 is warmed, a word at +3 is not. Guards the default against drift.
#[tokio::test]
async fn v3_cold_start_default_radius_preserves_two_word_window() -> Result<()> {
    let pool = Address::repeat_byte(0x2B);
    let layout = V3StorageLayout::uniswap(60);
    let spacing = 60i32;

    let w0 = v3_word_position(0, spacing);
    let tick_wp2 = 2 * 256 * 60; // word +2, bit 0 (inside the default window)
    let tick_wp3 = 3 * 256 * 60; // word +3, bit 0 (outside the default window)
    assert_eq!(v3_word_position(tick_wp2, spacing), w0 + 2);
    assert_eq!(v3_word_position(tick_wp3, spacing), w0 + 3);

    let key_wp2 = v3_tick_bitmap_storage_key_with_base(w0 + 2, layout.tick_bitmap_base_slot);
    let key_wp3 = v3_tick_bitmap_storage_key_with_base(w0 + 3, layout.tick_bitmap_base_slot);
    let info_wp2 = v3_tick_info_storage_keys_with_base(tick_wp2, layout.ticks_base_slot);
    let info_wp3 = v3_tick_info_storage_keys_with_base(tick_wp3, layout.ticks_base_slot);

    let mut cache = setup_cache().await?;
    cache.set_storage_batch_fetcher(fetcher_with_failures(
        HashMap::from([
            (
                (pool, layout.slot0_slot),
                v3_slot0_word(U256::from(99_u64), 0, U256::ZERO),
            ),
            ((pool, layout.liquidity_slot), U256::from(5_u64)),
            ((pool, key_wp2), v3_bit(tick_wp2, spacing)),
            ((pool, key_wp3), v3_bit(tick_wp3, spacing)),
            ((pool, info_wp2[0]), U256::from(1_u64)),
            ((pool, info_wp2[3]), U256::from(1_u64)),
            ((pool, info_wp3[0]), U256::from(1_u64)),
            ((pool, info_wp3[3]), U256::from(1_u64)),
        ]),
        Vec::new(),
    ));

    let registry = v3_registry();
    let mut registration = PoolRegistration::new(PoolKey::UniswapV3(pool))
        .with_state_address(pool)
        .with_metadata(ProtocolMetadata::UniswapV3(
            V3Metadata::default()
                .with_storage_layout(layout)
                .with_tick_spacing(60),
        ));

    let outcome = registry.cold_start(&mut registration, &mut cache, ColdStartPolicy::Eager)?;
    assert!(
        matches!(outcome, ColdStartOutcome::Ready(_)),
        "got {outcome:?}"
    );
    assert!(
        cache.cached_storage_value(pool, key_wp2).is_some(),
        "word +2 bitmap must be warmed by the default ±2 window"
    );
    assert!(
        cache.cached_storage_value(pool, key_wp3).is_none(),
        "word +3 bitmap must NOT be warmed: the default radius is 2, not 3"
    );
    assert!(
        cache.cached_storage_value(pool, info_wp2[0]).is_some(),
        "word +2 tick info must be warmed by the default window"
    );
    assert!(
        cache.cached_storage_value(pool, info_wp3[0]).is_none(),
        "word +3 tick info must NOT be warmed under the default window"
    );
    Ok(())
}

// Policy boundary: HotSlotsOnly warms only slot0 + liquidity — NO tick bitmap
// words (current or neighbouring). Guards the multi-word scan from leaking into
// the hot-only policy.
#[tokio::test]
async fn v3_cold_start_hot_slots_only_skips_tick_words() -> Result<()> {
    let pool = Address::repeat_byte(0x25);
    let layout = V3StorageLayout::uniswap(60);
    let key_w0 =
        v3_tick_bitmap_storage_key_with_base(v3_word_position(0, 60), layout.tick_bitmap_base_slot);

    let mut cache = setup_cache().await?;
    cache.set_storage_batch_fetcher(fetcher_with_failures(
        HashMap::from([
            (
                (pool, layout.slot0_slot),
                v3_slot0_word(U256::from(99_u64), 0, U256::ZERO),
            ),
            ((pool, layout.liquidity_slot), U256::from(5_u64)),
            ((pool, key_w0), v3_bit(60, 60)),
        ]),
        Vec::new(),
    ));

    let registry = v3_registry();
    let mut registration = PoolRegistration::new(PoolKey::UniswapV3(pool))
        .with_state_address(pool)
        .with_metadata(ProtocolMetadata::UniswapV3(
            V3Metadata::default()
                .with_storage_layout(layout)
                .with_tick_spacing(60),
        ));

    let outcome =
        registry.cold_start(&mut registration, &mut cache, ColdStartPolicy::HotSlotsOnly)?;
    assert!(
        matches!(outcome, ColdStartOutcome::Ready(_)),
        "got {outcome:?}"
    );
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
    assert!(
        cache.cached_storage_value(pool, key_w0).is_none(),
        "HotSlotsOnly must not warm any tick bitmap word"
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
async fn v3_cold_start_zero_tick_spacing_is_unsupported() -> Result<()> {
    let pool = Address::repeat_byte(0x2a);
    let mut cache = setup_cache().await?;
    cache.set_storage_batch_fetcher(fetcher_with_failures(HashMap::new(), Vec::new()));

    let registry = v3_registry();
    // tick_spacing 0 must be rejected as an unresolvable layout, not divide by
    // zero inside the planner's bitmap-word math.
    let mut registration = PoolRegistration::new(PoolKey::UniswapV3(pool))
        .with_state_address(pool)
        .with_metadata(ProtocolMetadata::UniswapV3(
            V3Metadata::default().with_tick_spacing(0),
        ));

    let outcome = registry.cold_start(&mut registration, &mut cache, ColdStartPolicy::Eager)?;
    assert!(
        matches!(outcome, ColdStartOutcome::Unsupported(_)),
        "zero tick spacing should be Unsupported, got {outcome:?}"
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
        .with_metadata(ProtocolMetadata::UniswapV3(
            V3Metadata::default()
                .with_storage_layout(layout)
                .with_tick_spacing(60),
        ));

    let outcome = registry.cold_start(&mut registration, &mut cache, ColdStartPolicy::Eager)?;
    assert!(
        matches!(outcome, ColdStartOutcome::NeedsRepair(_, _)),
        "an unfetchable slot0 must need repair, got {outcome:?}"
    );
    Ok(())
}

// --- Balancer V2 (slice 2: discover -> verify access-list cold start) ---

/// A mocked-provider cache plus the asserter, so a test can prove no RPC was
/// issued (`asserter.read_q().is_empty()`).
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

/// Install raw runtime bytecode (a compiled mock-vault fixture) at `vault`.
fn install_vault_runtime(cache: &mut EvmCache, vault: Address, runtime: &str) {
    let code = Bytecode::new_raw(Bytes::from(
        hex::decode(runtime.trim()).expect("valid mock-vault runtime hex"),
    ));
    let code_hash = code.hash_slow();
    cache.db_mut().insert_account_info(
        vault,
        AccountInfo {
            balance: U256::ZERO,
            nonce: 1,
            code: Some(code),
            code_hash,
            account_id: None,
        },
    );
}

/// Install the compiled `MockBalancerVault` stub at `vault`. Its
/// `getPoolTokens(bytes32)` SLOADs fixed slots 0..=4 and returns the dynamic
/// `(address[] tokens, uint256[] balances, uint256 lastChangeBlock)` tuple built
/// from them (length 2).
fn install_mock_vault(cache: &mut EvmCache, vault: Address) {
    install_vault_runtime(
        cache,
        vault,
        include_str!("fixtures/mock_balancer_vault_runtime.hex"),
    );
}

fn balancer_registry() -> AdapterRegistry {
    let mut registry = AdapterRegistry::new();
    registry
        .register_adapter(Arc::new(BalancerV2Adapter::default()))
        .unwrap();
    registry
}

#[tokio::test(flavor = "multi_thread")]
async fn balancer_cold_start_discover_verify_ready() -> Result<()> {
    let vault = Address::repeat_byte(0x31);
    // Distinct leading-20 / trailing-12 so the pool_address derivation
    // (leading 20 bytes of the poolId) can't accidentally pass via a wrong
    // slice range or byte order.
    let mut pid = [0u8; 32];
    pid[..20].fill(0x11);
    pid[20..].fill(0x22);
    let pool_id = B256::from(pid);
    let token0 = Address::repeat_byte(0xc0);
    let token1 = Address::repeat_byte(0xc1);

    let (mut cache, asserter) = setup_cache_with_asserter().await?;
    // The block beneficiary (default Address::ZERO) is credited gas during the
    // discover call's transact; install it so the offline run does not fetch it.
    install_default_account(&mut cache, Address::ZERO);
    install_mock_vault(&mut cache, vault);
    // Seed the vault's fixed slots 0..=4 that getPoolTokens SLOADs. Token slots
    // (0,1) hold the immutable token addresses; balance slots (2,3) hold STALE
    // values, so the verify round must refresh them to the fetcher's fresh ones.
    cache
        .db_mut()
        .insert_account_storage(vault, U256::from(0), token_slot_word(token0))?;
    cache
        .db_mut()
        .insert_account_storage(vault, U256::from(1), token_slot_word(token1))?;
    cache
        .db_mut()
        .insert_account_storage(vault, U256::from(2), U256::from(1_u64))?;
    cache
        .db_mut()
        .insert_account_storage(vault, U256::from(3), U256::from(2_u64))?;
    cache
        .db_mut()
        .insert_account_storage(vault, U256::from(4), U256::from(7_u64))?;
    // Round 2's verify fetcher returns FRESH balances for the discovered slots.
    cache.set_storage_batch_fetcher(fetcher_with_failures(
        HashMap::from([
            ((vault, U256::from(0)), token_slot_word(token0)),
            ((vault, U256::from(1)), token_slot_word(token1)),
            ((vault, U256::from(2)), U256::from(1000_u64)),
            ((vault, U256::from(3)), U256::from(2000_u64)),
            ((vault, U256::from(4)), U256::from(7_u64)),
        ]),
        Vec::new(),
    ));

    let registry = balancer_registry();
    let mut registration = PoolRegistration::new(PoolKey::BalancerV2(pool_id))
        .with_state_address(vault)
        .with_metadata(ProtocolMetadata::BalancerV2(
            BalancerV2Metadata::default().with_vault(vault),
        ));

    let outcome = registry.cold_start(&mut registration, &mut cache, ColdStartPolicy::Eager)?;

    assert!(
        matches!(outcome, ColdStartOutcome::Ready(_)),
        "discover->verify should reach Ready, got {outcome:?}"
    );
    assert_eq!(registration.status, PoolStatus::Ready);
    match registration.metadata {
        ProtocolMetadata::BalancerV2(ref m) => {
            assert_eq!(
                m.tokens,
                vec![token0, token1],
                "tokens decoded from the getPoolTokens return data"
            );
            assert_eq!(m.vault, Some(vault));
            // pool_address is the leading 20 bytes of the poolId (Balancer
            // poolId = address(20) | specialization | nonce).
            assert_eq!(
                m.pool_address,
                Some(Address::repeat_byte(0x11)),
                "pool_address must be the leading 20 bytes of the poolId"
            );
        }
        ref other => panic!("expected BalancerV2 metadata, got {other:?}"),
    }
    // The verify round refreshed the discovered balance slots to the fetcher's
    // fresh values (proving discover -> verify warmed them, not the stale seed).
    assert_eq!(
        cache.cached_storage_value(vault, U256::from(2)),
        Some(U256::from(1000_u64))
    );
    assert_eq!(
        cache.cached_storage_value(vault, U256::from(3)),
        Some(U256::from(2000_u64))
    );
    assert!(
        asserter.read_q().is_empty(),
        "the cold start must be fully offline (no RPC)"
    );
    Ok(())
}

// Verify-only fast path (Balancer): when the balance read-set is already known
// (balance_slots pre-populated), cold-start skips the getPoolTokens discovery.
// No vault runtime is installed, so a discover call would fail — reaching Ready
// proves no discovery ran. The single verify round warms the known slot, and the
// config-supplied tokens are preserved (no getPoolTokens decode to repopulate).
#[tokio::test(flavor = "multi_thread")]
async fn balancer_cold_start_verify_only_skips_discovery() -> Result<()> {
    let vault = Address::repeat_byte(0x32);
    let mut pid = [0u8; 32];
    pid[..20].fill(0x11);
    pid[20..].fill(0x22);
    let pool_id = B256::from(pid);
    let token0 = Address::repeat_byte(0xc0);
    let token1 = Address::repeat_byte(0xc1);
    let stale = U256::from(1_u64);
    let fresh = U256::from(1000_u64);

    let (mut cache, asserter) = setup_cache_with_asserter().await?;
    // Bare, code-less vault: the known slot injects offline (account exists), and
    // a discover getPoolTokens would fail on code-less code — so reaching Ready
    // proves the verify-only path skipped discovery.
    install_default_account(&mut cache, vault);
    cache
        .db_mut()
        .insert_account_storage(vault, U256::from(2), stale)?;
    cache.set_storage_batch_fetcher(fetcher_with_failures(
        HashMap::from([((vault, U256::from(2)), fresh)]),
        Vec::new(),
    ));

    let registry = balancer_registry();
    let mut registration = PoolRegistration::new(PoolKey::BalancerV2(pool_id))
        .with_state_address(vault)
        .with_metadata(ProtocolMetadata::BalancerV2(
            BalancerV2Metadata::default()
                .with_vault(vault)
                .with_tokens([token0, token1])
                // A pre-populated read-set selects the verify-only fast path.
                .with_balance_slots([U256::from(2)]),
        ));

    let outcome = registry.cold_start(&mut registration, &mut cache, ColdStartPolicy::Eager)?;

    assert!(
        matches!(outcome, ColdStartOutcome::Ready(_)),
        "verify-only cold-start should reach Ready without discovery, got {outcome:?}"
    );
    assert_eq!(registration.status, PoolStatus::Ready);
    match registration.metadata {
        ProtocolMetadata::BalancerV2(ref m) => {
            assert_eq!(m.vault, Some(vault));
            assert_eq!(
                m.tokens,
                vec![token0, token1],
                "config tokens must be preserved (no getPoolTokens decode ran)"
            );
            assert!(
                m.balance_slots.contains(&U256::from(2)),
                "the known read-set must be persisted, got {:?}",
                m.balance_slots
            );
            assert_eq!(
                m.pool_address,
                Some(Address::repeat_byte(0x11)),
                "pool_address is still derived from the poolId"
            );
        }
        ref other => panic!("expected BalancerV2 metadata, got {other:?}"),
    }
    assert_eq!(
        cache.cached_storage_value(vault, U256::from(2)),
        Some(fresh),
        "the single verify round must refresh the known slot to the fresh value"
    );
    assert!(
        asserter.read_q().is_empty(),
        "the verify-only cold start must be fully offline (no RPC)"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn balancer_cold_start_missing_vault_is_unsupported() -> Result<()> {
    let pool_id = B256::repeat_byte(0x33);
    let mut cache = setup_cache().await?;
    cache.set_storage_batch_fetcher(fetcher_with_failures(HashMap::new(), Vec::new()));

    let registry = balancer_registry();
    // No vault metadata and no state_addresses -> the vault is unresolvable.
    let mut registration = PoolRegistration::new(PoolKey::BalancerV2(pool_id))
        .with_metadata(ProtocolMetadata::BalancerV2(BalancerV2Metadata::default()));

    let outcome = registry.cold_start(&mut registration, &mut cache, ColdStartPolicy::Eager)?;
    assert!(
        matches!(
            outcome,
            ColdStartOutcome::Unsupported(UnsupportedReason::MissingMetadata("Balancer vault"))
        ),
        "a vault-less Balancer pool must be Unsupported for the vault reason, got {outcome:?}"
    );
    Ok(())
}

// Drives the empty-capture branch: getPoolTokens decodes fine but touches no
// vault storage, so the discovery yields an empty `(vault, slot)` set. The
// repair must re-discover (`ColdStart`), NOT purge the shared singleton vault's
// storage (which would wipe every co-tenant Balancer pool).
#[tokio::test(flavor = "multi_thread")]
async fn balancer_cold_start_empty_capture_repairs_via_coldstart() -> Result<()> {
    let vault = Address::repeat_byte(0x41);
    let pool_id = B256::repeat_byte(0x42);

    let (mut cache, asserter) = setup_cache_with_asserter().await?;
    install_default_account(&mut cache, Address::ZERO);
    install_vault_runtime(
        &mut cache,
        vault,
        include_str!("fixtures/mock_balancer_vault_noslot_runtime.hex"),
    );

    let registry = balancer_registry();
    let mut registration = PoolRegistration::new(PoolKey::BalancerV2(pool_id))
        .with_state_address(vault)
        .with_metadata(ProtocolMetadata::BalancerV2(
            BalancerV2Metadata::default().with_vault(vault),
        ));

    let outcome = registry.cold_start(&mut registration, &mut cache, ColdStartPolicy::Eager)?;
    assert!(
        matches!(
            outcome,
            ColdStartOutcome::NeedsRepair(_, RepairAction::ColdStart { .. })
        ),
        "empty capture must re-discover, not purge the shared vault, got {outcome:?}"
    );
    assert_ne!(registration.status, PoolStatus::Ready);
    assert!(asserter.read_q().is_empty(), "must be fully offline");
    Ok(())
}

// A getPoolTokens that reverts must be classified as a failed discovery
// (NeedsRepair via re-discovery), never silently driven to Ready.
#[tokio::test(flavor = "multi_thread")]
async fn balancer_cold_start_reverting_call_needs_repair() -> Result<()> {
    let vault = Address::repeat_byte(0x51);
    let pool_id = B256::repeat_byte(0x52);

    let (mut cache, asserter) = setup_cache_with_asserter().await?;
    install_default_account(&mut cache, Address::ZERO);
    install_vault_runtime(
        &mut cache,
        vault,
        include_str!("fixtures/mock_balancer_vault_revert_runtime.hex"),
    );

    let registry = balancer_registry();
    let mut registration = PoolRegistration::new(PoolKey::BalancerV2(pool_id))
        .with_state_address(vault)
        .with_metadata(ProtocolMetadata::BalancerV2(
            BalancerV2Metadata::default().with_vault(vault),
        ));

    let outcome = registry.cold_start(&mut registration, &mut cache, ColdStartPolicy::Eager)?;
    assert!(
        matches!(
            outcome,
            ColdStartOutcome::NeedsRepair(_, RepairAction::ColdStart { .. })
        ),
        "a reverting getPoolTokens must need repair, not reach Ready, got {outcome:?}"
    );
    assert_ne!(registration.status, PoolStatus::Ready);
    assert!(asserter.read_q().is_empty(), "must be fully offline");
    Ok(())
}

// The verify round warms the discovered balance slots. If one is unfetchable (an
// archive miss), the pool must NOT be marked Ready with unwarmed balances — it
// must need repair, mirroring the V2/V3 mandatory-slot behavior.
#[tokio::test(flavor = "multi_thread")]
async fn balancer_cold_start_failed_balance_slot_needs_repair() -> Result<()> {
    let vault = Address::repeat_byte(0x61);
    let pool_id = B256::repeat_byte(0x62);
    let token0 = Address::repeat_byte(0xc0);
    let token1 = Address::repeat_byte(0xc1);

    let (mut cache, _asserter) = setup_cache_with_asserter().await?;
    install_default_account(&mut cache, Address::ZERO);
    install_mock_vault(&mut cache, vault);
    cache
        .db_mut()
        .insert_account_storage(vault, U256::from(0), token_slot_word(token0))?;
    cache
        .db_mut()
        .insert_account_storage(vault, U256::from(1), token_slot_word(token1))?;
    cache
        .db_mut()
        .insert_account_storage(vault, U256::from(2), U256::from(1_u64))?;
    cache
        .db_mut()
        .insert_account_storage(vault, U256::from(3), U256::from(2_u64))?;
    cache
        .db_mut()
        .insert_account_storage(vault, U256::from(4), U256::from(7_u64))?;
    // Verify round: slot 2 (a discovered balance slot) fails to fetch.
    cache.set_storage_batch_fetcher(fetcher_with_failures(
        HashMap::from([
            ((vault, U256::from(0)), token_slot_word(token0)),
            ((vault, U256::from(1)), token_slot_word(token1)),
            ((vault, U256::from(3)), U256::from(2000_u64)),
            ((vault, U256::from(4)), U256::from(7_u64)),
        ]),
        vec![(vault, U256::from(2))],
    ));

    let registry = balancer_registry();
    let mut registration = PoolRegistration::new(PoolKey::BalancerV2(pool_id))
        .with_state_address(vault)
        .with_metadata(ProtocolMetadata::BalancerV2(
            BalancerV2Metadata::default().with_vault(vault),
        ));

    let outcome = registry.cold_start(&mut registration, &mut cache, ColdStartPolicy::Eager)?;
    assert!(
        matches!(outcome, ColdStartOutcome::NeedsRepair(_, _)),
        "an unfetchable discovered balance slot must need repair, got {outcome:?}"
    );
    assert_ne!(registration.status, PoolStatus::Ready);
    Ok(())
}

// The decode + slot capture must generalize beyond two tokens (real weighted /
// stable pools hold 3..8).
#[tokio::test(flavor = "multi_thread")]
async fn balancer_cold_start_three_tokens_ready() -> Result<()> {
    let vault = Address::repeat_byte(0x71);
    let pool_id = B256::repeat_byte(0x72);
    let token0 = Address::repeat_byte(0xc0);
    let token1 = Address::repeat_byte(0xc1);
    let token2 = Address::repeat_byte(0xc2);

    let (mut cache, asserter) = setup_cache_with_asserter().await?;
    install_default_account(&mut cache, Address::ZERO);
    install_vault_runtime(
        &mut cache,
        vault,
        include_str!("fixtures/mock_balancer_vault_3_runtime.hex"),
    );
    // Slots 0..=6: token0,token1,token2, balance0,balance1,balance2, lastChangeBlock.
    cache
        .db_mut()
        .insert_account_storage(vault, U256::from(0), token_slot_word(token0))?;
    cache
        .db_mut()
        .insert_account_storage(vault, U256::from(1), token_slot_word(token1))?;
    cache
        .db_mut()
        .insert_account_storage(vault, U256::from(2), token_slot_word(token2))?;
    cache
        .db_mut()
        .insert_account_storage(vault, U256::from(3), U256::from(1_u64))?;
    cache
        .db_mut()
        .insert_account_storage(vault, U256::from(4), U256::from(2_u64))?;
    cache
        .db_mut()
        .insert_account_storage(vault, U256::from(5), U256::from(3_u64))?;
    cache
        .db_mut()
        .insert_account_storage(vault, U256::from(6), U256::from(9_u64))?;
    cache.set_storage_batch_fetcher(fetcher_with_failures(
        HashMap::from([
            ((vault, U256::from(0)), token_slot_word(token0)),
            ((vault, U256::from(1)), token_slot_word(token1)),
            ((vault, U256::from(2)), token_slot_word(token2)),
            ((vault, U256::from(3)), U256::from(1000_u64)),
            ((vault, U256::from(4)), U256::from(2000_u64)),
            ((vault, U256::from(5)), U256::from(3000_u64)),
            ((vault, U256::from(6)), U256::from(9_u64)),
        ]),
        Vec::new(),
    ));

    let registry = balancer_registry();
    let mut registration = PoolRegistration::new(PoolKey::BalancerV2(pool_id))
        .with_state_address(vault)
        .with_metadata(ProtocolMetadata::BalancerV2(
            BalancerV2Metadata::default().with_vault(vault),
        ));

    let outcome = registry.cold_start(&mut registration, &mut cache, ColdStartPolicy::Eager)?;
    assert!(
        matches!(outcome, ColdStartOutcome::Ready(_)),
        "got {outcome:?}"
    );
    match registration.metadata {
        ProtocolMetadata::BalancerV2(ref m) => {
            assert_eq!(m.tokens, vec![token0, token1, token2], "3-token decode");
        }
        ref other => panic!("expected BalancerV2 metadata, got {other:?}"),
    }
    // All three discovered balance slots were refreshed by the verify round.
    assert_eq!(
        cache.cached_storage_value(vault, U256::from(3)),
        Some(U256::from(1000_u64))
    );
    assert_eq!(
        cache.cached_storage_value(vault, U256::from(5)),
        Some(U256::from(3000_u64))
    );
    assert!(asserter.read_q().is_empty(), "must be fully offline");
    Ok(())
}

// --- Solidly V2 ---

fn solidly_registry() -> AdapterRegistry {
    let mut registry = AdapterRegistry::new();
    registry
        .register_adapter(Arc::new(SolidlyV2Adapter::default()))
        .unwrap();
    registry
}

// Eager cold-start warms both reserve slots (two separate uint256 slots, unlike
// V2's packed slot) plus the token slots, and reaches Ready.
#[tokio::test]
async fn solidly_cold_start_ready_warms_reserves_and_tokens() -> Result<()> {
    let pool = Address::repeat_byte(0x51);
    let token0 = Address::repeat_byte(0xc0);
    let token1 = Address::repeat_byte(0xc1);
    // Arbitrary test layout — the adapter verifies whatever the layout names.
    let (r0_slot, r1_slot, t0_slot, t1_slot) = (
        U256::from(10_u64),
        U256::from(11_u64),
        U256::from(12_u64),
        U256::from(13_u64),
    );
    let layout = SolidlyStorageLayout::new(r0_slot, r1_slot, t0_slot, t1_slot);

    let mut cache = setup_cache().await?;
    cache.set_storage_batch_fetcher(fetcher_with_failures(
        HashMap::from([
            ((pool, r0_slot), U256::from(1_000_u64)),
            ((pool, r1_slot), U256::from(2_000_u64)),
            ((pool, t0_slot), token_slot_word(token0)),
            ((pool, t1_slot), token_slot_word(token1)),
        ]),
        Vec::new(),
    ));

    let registry = solidly_registry();
    let mut registration = PoolRegistration::new(PoolKey::SolidlyV2(pool))
        .with_state_address(pool)
        .with_metadata(ProtocolMetadata::SolidlyV2(
            SolidlyV2Metadata::default()
                .with_stable(false)
                .with_storage_layout(layout),
        ));

    let outcome = registry.cold_start(&mut registration, &mut cache, ColdStartPolicy::Eager)?;
    assert!(
        matches!(outcome, ColdStartOutcome::Ready(_)),
        "got {outcome:?}"
    );
    assert_eq!(registration.status, PoolStatus::Ready);
    assert!(cache.cached_storage_value(pool, r0_slot).is_some());
    assert!(cache.cached_storage_value(pool, r1_slot).is_some());
    assert!(cache.cached_storage_value(pool, t0_slot).is_some());
    assert!(cache.cached_storage_value(pool, t1_slot).is_some());
    Ok(())
}

// Reserves are mandatory: a genuine on-chain zero (degenerate pool) and an
// archive miss must produce DISTINCT repairs (the per-slot SlotFetch point),
// mirroring the V2 adapter.
#[tokio::test]
async fn solidly_cold_start_zero_vs_failed_reserves_are_distinct_repairs() -> Result<()> {
    let pool = Address::repeat_byte(0x52);
    let layout = SolidlyStorageLayout::new(
        U256::from(10_u64),
        U256::from(11_u64),
        U256::from(12_u64),
        U256::from(13_u64),
    );
    let metadata = || {
        ProtocolMetadata::SolidlyV2(
            SolidlyV2Metadata::default()
                .with_stable(false)
                .with_storage_layout(layout),
        )
    };

    // Case A: reserves read a genuine on-chain ZERO (degenerate pool).
    let mut cache_zero = setup_cache().await?;
    cache_zero.set_storage_batch_fetcher(fetcher_with_failures(HashMap::new(), Vec::new()));
    let mut reg_zero = PoolRegistration::new(PoolKey::SolidlyV2(pool))
        .with_state_address(pool)
        .with_metadata(metadata());
    let zero =
        solidly_registry().cold_start(&mut reg_zero, &mut cache_zero, ColdStartPolicy::Eager)?;

    // Case B: reserve0 FAILS to fetch (archive / historical miss).
    let mut cache_fail = setup_cache().await?;
    cache_fail.set_storage_batch_fetcher(fetcher_with_failures(
        HashMap::new(),
        vec![(pool, layout.reserve0_slot)],
    ));
    let mut reg_fail = PoolRegistration::new(PoolKey::SolidlyV2(pool))
        .with_state_address(pool)
        .with_metadata(metadata());
    let fail =
        solidly_registry().cold_start(&mut reg_fail, &mut cache_fail, ColdStartPolicy::Eager)?;

    assert!(
        matches!(zero, ColdStartOutcome::NeedsRepair(_, _)),
        "genuine-zero reserves should need repair, got {zero:?}"
    );
    assert!(
        matches!(fail, ColdStartOutcome::NeedsRepair(_, _)),
        "archive-miss reserves should need repair, got {fail:?}"
    );
    assert_ne!(
        repair_of(&zero),
        repair_of(&fail),
        "a genuine zero and an archive miss must produce different repairs"
    );
    Ok(())
}

// A layout whose slots collide (here reserve0 == token0) is rejected at the
// planner boundary rather than silently corrupting the verdict / token decode.
#[tokio::test]
async fn solidly_cold_start_colliding_layout_is_unsupported() -> Result<()> {
    let pool = Address::repeat_byte(0x53);
    let mut cache = setup_cache().await?;
    cache.set_storage_batch_fetcher(fetcher_with_failures(HashMap::new(), Vec::new()));
    let registry = solidly_registry();
    // reserve0_slot == token0_slot (both 10) -> colliding.
    let mut registration = PoolRegistration::new(PoolKey::SolidlyV2(pool))
        .with_state_address(pool)
        .with_metadata(ProtocolMetadata::SolidlyV2(
            SolidlyV2Metadata::default()
                .with_stable(false)
                .with_storage_layout(SolidlyStorageLayout::new(
                    U256::from(10_u64),
                    U256::from(11_u64),
                    U256::from(10_u64),
                    U256::from(13_u64),
                )),
        ));
    let outcome = registry.cold_start(&mut registration, &mut cache, ColdStartPolicy::Eager)?;
    assert!(
        matches!(outcome, ColdStartOutcome::Unsupported(_)),
        "a colliding layout must be Unsupported, got {outcome:?}"
    );
    Ok(())
}

// Lazy warms the reserves now and defers exactly the token slots; HotSlotsOnly
// warms reserves only and does NOT defer.
#[tokio::test]
async fn solidly_cold_start_lazy_defers_token_slots() -> Result<()> {
    let pool = Address::repeat_byte(0x54);
    let token0 = Address::repeat_byte(0xc0);
    let token1 = Address::repeat_byte(0xc1);
    let (r0, r1, t0, t1) = (
        U256::from(10_u64),
        U256::from(11_u64),
        U256::from(12_u64),
        U256::from(13_u64),
    );
    let layout = SolidlyStorageLayout::new(r0, r1, t0, t1);
    let seed = || {
        HashMap::from([
            ((pool, r0), U256::from(1_000_u64)),
            ((pool, r1), U256::from(2_000_u64)),
            ((pool, t0), token_slot_word(token0)),
            ((pool, t1), token_slot_word(token1)),
        ])
    };

    // Lazy: ReadyWithDeferred, reserves warm, tokens deferred (not warm).
    let mut cache = setup_cache().await?;
    cache.set_storage_batch_fetcher(fetcher_with_failures(seed(), Vec::new()));
    let registry = solidly_registry();
    let mut reg = PoolRegistration::new(PoolKey::SolidlyV2(pool))
        .with_state_address(pool)
        .with_metadata(ProtocolMetadata::SolidlyV2(
            SolidlyV2Metadata::default()
                .with_stable(false)
                .with_storage_layout(layout),
        ));
    let outcome = registry.cold_start(&mut reg, &mut cache, ColdStartPolicy::Lazy)?;
    let deferred = match outcome {
        ColdStartOutcome::ReadyWithDeferred(_, d) => d,
        other => panic!("Lazy should be ReadyWithDeferred, got {other:?}"),
    };
    let deferred_slots: HashSet<(Address, U256)> = deferred
        .iter()
        .flat_map(|w| match w {
            DeferredWork::VerifySlots(slots) => slots.clone(),
            _ => Vec::new(),
        })
        .collect();
    assert!(deferred_slots.contains(&(pool, t0)) && deferred_slots.contains(&(pool, t1)));
    assert!(cache.cached_storage_value(pool, r0).is_some());
    assert_eq!(
        cache.cached_storage_value(pool, t0),
        None,
        "tokens deferred, not warm"
    );

    // run_deferred warms the deferred token slots.
    registry.run_deferred(&deferred, &mut cache)?;
    assert!(cache.cached_storage_value(pool, t0).is_some());
    assert!(cache.cached_storage_value(pool, t1).is_some());

    // HotSlotsOnly: plain Ready (no defer), reserves warm, tokens not warm.
    let mut cache_h = setup_cache().await?;
    cache_h.set_storage_batch_fetcher(fetcher_with_failures(seed(), Vec::new()));
    let mut reg_h = PoolRegistration::new(PoolKey::SolidlyV2(pool))
        .with_state_address(pool)
        .with_metadata(ProtocolMetadata::SolidlyV2(
            SolidlyV2Metadata::default()
                .with_stable(false)
                .with_storage_layout(layout),
        ));
    let outcome_h = registry.cold_start(&mut reg_h, &mut cache_h, ColdStartPolicy::HotSlotsOnly)?;
    assert!(
        matches!(outcome_h, ColdStartOutcome::Ready(_)),
        "HotSlotsOnly should be plain Ready (no defer), got {outcome_h:?}"
    );
    assert!(cache_h.cached_storage_value(pool, r0).is_some());
    assert_eq!(cache_h.cached_storage_value(pool, t0), None);
    Ok(())
}

// --- Curve StableSwap (discover -> verify access-list cold start) ---
//
// Models the Balancer discover->verify tests above. The mock pool runtime
// (`mock_curve_pool_runtime.hex`) SLOADs slot 0 for any call and returns it, so
// the discover `get_dy(0, 1, dx)` captures `(pool, 0)`; the verify round then
// re-reads it from the fetcher. `coins` is config-supplied and must survive the
// run unchanged (it is the pool's static coin ordering, not discovered).

fn curve_registry() -> AdapterRegistry {
    let mut registry = AdapterRegistry::new();
    registry
        .register_adapter(Arc::new(CurveAdapter::default()))
        .unwrap();
    registry
}

// Discover -> verify -> Ready: the discover pass captures the get_dy read-set
// (slot 0 on this stub), the verify round warms it to the fetcher's fresh value,
// and `finish` persists `discovered_slots` (containing slot 0) while preserving
// the config-supplied `coins`.
#[tokio::test(flavor = "multi_thread")]
async fn curve_cold_start_discover_verify_ready() -> Result<()> {
    let pool = Address::repeat_byte(0xc1);
    let dai = Address::repeat_byte(0x01);
    let usdc = Address::repeat_byte(0x02);
    let usdt = Address::repeat_byte(0x03);
    let stale = U256::from(1_u64);
    let fresh = U256::from(999_900_u64);

    let (mut cache, asserter) = setup_cache_with_asserter().await?;
    // The block beneficiary (Address::ZERO) is credited gas during the discover
    // call's transact; install it so the offline run does not fetch it.
    install_default_account(&mut cache, Address::ZERO);
    install_vault_runtime(
        &mut cache,
        pool,
        include_str!("fixtures/mock_curve_pool_runtime.hex"),
    );
    // Seed slot 0 STALE so the discover call has something to SLOAD; the verify
    // round must refresh it to the fetcher's FRESH value.
    cache
        .db_mut()
        .insert_account_storage(pool, U256::ZERO, stale)?;
    cache.set_storage_batch_fetcher(fetcher_with_failures(
        HashMap::from([((pool, U256::ZERO), fresh)]),
        Vec::new(),
    ));

    let registry = curve_registry();
    let mut registration = PoolRegistration::new(PoolKey::Curve(pool))
        .with_state_address(pool)
        .with_metadata(ProtocolMetadata::Curve(
            CurveMetadata::default()
                .with_coins(vec![dai, usdc, usdt])
                .with_discovered_slots(Vec::new())
                .with_variant(CurveVariant::StableSwap),
        ));

    let outcome = registry.cold_start(&mut registration, &mut cache, ColdStartPolicy::Eager)?;

    assert!(
        matches!(outcome, ColdStartOutcome::Ready(_)),
        "discover->verify should reach Ready, got {outcome:?}"
    );
    assert_eq!(registration.status, PoolStatus::Ready);
    match registration.metadata {
        ProtocolMetadata::Curve(ref m) => {
            assert_eq!(
                m.coins,
                vec![dai, usdc, usdt],
                "config coins must be preserved across cold-start"
            );
            assert!(
                !m.discovered_slots.is_empty(),
                "cold-start must persist the discovered read-set"
            );
            assert!(
                m.discovered_slots.contains(&U256::ZERO),
                "the get_dy read-set slot 0 must be discovered, got {:?}",
                m.discovered_slots
            );
        }
        ref other => panic!("expected Curve metadata, got {other:?}"),
    }
    // The verify round refreshed the discovered slot to the fetcher's fresh value
    // (proving discover -> verify warmed it, not the stale seed).
    assert_eq!(
        cache.cached_storage_value(pool, U256::ZERO),
        Some(fresh),
        "verify round must refresh the discovered slot to the fresh value"
    );
    assert!(
        asserter.read_q().is_empty(),
        "the cold start must be fully offline (no RPC)"
    );
    Ok(())
}

// Verify-only fast path: when the read-set is already known (discovered_slots
// pre-populated), cold-start skips discovery entirely. No pool runtime is
// installed here, so a discover `get_dy` sim would fail — reaching Ready proves
// no discovery ran. The single verify round warms the known slot, and coins /
// variant / the discovered set are preserved.
#[tokio::test(flavor = "multi_thread")]
async fn curve_cold_start_verify_only_skips_discovery() -> Result<()> {
    let pool = Address::repeat_byte(0xc4);
    let dai = Address::repeat_byte(0x01);
    let usdc = Address::repeat_byte(0x02);
    let stale = U256::from(1_u64);
    let fresh = U256::from(777_000_u64);

    let (mut cache, asserter) = setup_cache_with_asserter().await?;
    // Install a BARE, code-less pool account (no runtime). Two consequences:
    // (1) the verified slot can be injected fully offline (the account exists,
    //     so no lazy account fetch), and (2) it doubles as the discovery-skip
    //     proof — a discover round would run `get_dy` against a code-less
    //     account, capture no slots, and repair (NoSlotsDiscovered), so reaching
    //     Ready proves the verify-only path skipped discovery entirely.
    install_default_account(&mut cache, pool);
    // Seed the known slot STALE; the single verify round must refresh it.
    cache
        .db_mut()
        .insert_account_storage(pool, U256::ZERO, stale)?;
    cache.set_storage_batch_fetcher(fetcher_with_failures(
        HashMap::from([((pool, U256::ZERO), fresh)]),
        Vec::new(),
    ));

    let registry = curve_registry();
    let mut registration = PoolRegistration::new(PoolKey::Curve(pool))
        .with_state_address(pool)
        .with_metadata(ProtocolMetadata::Curve(
            CurveMetadata::default()
                .with_coins(vec![dai, usdc])
                // A pre-populated read-set selects the verify-only fast path.
                .with_discovered_slots(vec![U256::ZERO])
                .with_variant(CurveVariant::CryptoSwap),
        ));

    let outcome = registry.cold_start(&mut registration, &mut cache, ColdStartPolicy::Eager)?;

    assert!(
        matches!(outcome, ColdStartOutcome::Ready(_)),
        "verify-only cold-start should reach Ready without discovery, got {outcome:?}"
    );
    assert_eq!(registration.status, PoolStatus::Ready);
    match registration.metadata {
        ProtocolMetadata::Curve(ref m) => {
            assert_eq!(m.coins, vec![dai, usdc], "config coins must be preserved");
            assert_eq!(
                m.variant,
                CurveVariant::CryptoSwap,
                "config variant must be preserved"
            );
            assert!(
                m.discovered_slots.contains(&U256::ZERO),
                "the known read-set must be persisted, got {:?}",
                m.discovered_slots
            );
        }
        ref other => panic!("expected Curve metadata, got {other:?}"),
    }
    assert_eq!(
        cache.cached_storage_value(pool, U256::ZERO),
        Some(fresh),
        "the single verify round must refresh the known slot to the fresh value"
    );
    assert!(
        asserter.read_q().is_empty(),
        "the verify-only cold start must be fully offline (no RPC)"
    );
    Ok(())
}

// Verify-only fast path, archive miss: a known slot that fails in the verify
// round must NOT be marked Ready over an unwarmed read-set — it needs a
// `VerifySlots` repair over the known slots, mirroring the discovery path's
// archive-miss behavior.
#[tokio::test(flavor = "multi_thread")]
async fn curve_cold_start_verify_only_unfetchable_slot_needs_repair() -> Result<()> {
    let pool = Address::repeat_byte(0xc5);

    let (mut cache, _asserter) = setup_cache_with_asserter().await?;
    cache.set_storage_batch_fetcher(fetcher_with_failures(
        HashMap::new(),
        vec![(pool, U256::ZERO)],
    ));

    let registry = curve_registry();
    let mut registration = PoolRegistration::new(PoolKey::Curve(pool))
        .with_state_address(pool)
        .with_metadata(ProtocolMetadata::Curve(
            CurveMetadata::default()
                .with_coins(vec![Address::repeat_byte(0x01), Address::repeat_byte(0x02)])
                .with_discovered_slots(vec![U256::ZERO])
                .with_variant(CurveVariant::StableSwap),
        ));

    let outcome = registry.cold_start(&mut registration, &mut cache, ColdStartPolicy::Eager)?;
    assert!(
        matches!(
            outcome,
            ColdStartOutcome::NeedsRepair(_, RepairAction::VerifySlots(_))
        ),
        "an unfetchable known slot must need a VerifySlots repair, got {outcome:?}"
    );
    assert_ne!(registration.status, PoolStatus::Ready);
    Ok(())
}

// Two-shot first-boot warming (`cold_start_primed`): a Curve pool with NO known
// read-set has its `get_dy` read-set derived by one `eth_createAccessList` (shot
// 1) and bulk-loaded (shot 2), so the follow-up discover runs WARM. The only two
// provider requests queued are createAccessList + the one bundled load; the
// discover does not fault slot 0 over RPC (it was prewarmed) — reaching Ready
// with an empty request queue proves the serial faulting was eliminated.
#[tokio::test(flavor = "multi_thread")]
async fn curve_cold_start_primed_access_list_avoids_serial_faults() -> Result<()> {
    let pool = Address::repeat_byte(0xca);
    let fresh = U256::from(999_000_u64);

    let asserter = Asserter::new();
    let provider = Arc::new(RootProvider::<AnyNetwork>::new(RpcClient::mocked(
        asserter.clone(),
    )));
    let mut cache = EvmCache::new(provider.clone()).await;

    // Batch gas price (eth_gasPrice), fetched once before the access-list probe.
    asserter.push_success(&U256::from(1_000_000_000_u64));
    // Shot 1: createAccessList reports the pool touches slot 0.
    asserter.push_success(&AccessListResult {
        access_list: AccessList(vec![AccessListItem {
            address: pool,
            storage_keys: vec![B256::ZERO],
        }]),
        gas_used: U256::ZERO,
        error: None,
    });
    // Shot 2: the bundled storage program returns slot 0's value (32 bytes).
    asserter.push_success(&Bytes::from(fresh.to_be_bytes::<32>().to_vec()));

    // Runtime so the warm discover's `get_dy` executes + the account is present
    // (no account fetch); ZERO is the discover call's gas-credited beneficiary;
    // the stub fetcher serves the verify round offline.
    install_default_account(&mut cache, Address::ZERO);
    install_vault_runtime(
        &mut cache,
        pool,
        include_str!("fixtures/mock_curve_pool_runtime.hex"),
    );
    cache.set_storage_batch_fetcher(fetcher_with_failures(
        HashMap::from([((pool, U256::ZERO), fresh)]),
        Vec::new(),
    ));

    let mut registry = AdapterRegistry::new();
    registry.register_adapter(Arc::new(CurveAdapter::default()))?;
    let mut registration = PoolRegistration::new(PoolKey::Curve(pool))
        .with_state_address(pool)
        .with_metadata(ProtocolMetadata::Curve(
            CurveMetadata::default()
                .with_coins(vec![Address::repeat_byte(0x01), Address::repeat_byte(0x02)])
                .with_variant(CurveVariant::StableSwap),
        ));

    let outcome = registry
        .cold_start_primed(&mut registration, &mut cache, provider.as_ref(), ColdStartPolicy::Eager)
        .await?;

    assert!(
        matches!(outcome, ColdStartOutcome::Ready(_)),
        "primed cold-start should reach Ready, got {outcome:?}"
    );
    match registration.metadata {
        ProtocolMetadata::Curve(ref m) => assert!(
            m.discovered_slots.contains(&U256::ZERO),
            "the warm discover must persist the read-set, got {:?}",
            m.discovered_slots
        ),
        ref other => panic!("expected Curve metadata, got {other:?}"),
    }
    assert_eq!(
        cache.cached_storage_value(pool, U256::ZERO),
        Some(fresh),
        "slot 0 warmed to the fresh value"
    );
    // Exactly createAccessList + one bundled load were issued: a serial slot
    // fault would have needed a third (un-queued) request and failed the run.
    assert!(
        asserter.read_q().is_empty(),
        "no serial per-slot faults: only createAccessList + one bundled load"
    );
    Ok(())
}

// Graceful fallback: when the provider lacks `eth_createAccessList` (here, the
// mock has no queued response, so it errors), priming is skipped and the pool
// finalizes through the normal local discovery — still Ready.
#[tokio::test(flavor = "multi_thread")]
async fn curve_cold_start_primed_falls_back_when_access_list_unsupported() -> Result<()> {
    let pool = Address::repeat_byte(0xcb);
    let stale = U256::from(1_u64);
    let fresh = U256::from(555_000_u64);

    let asserter = Asserter::new();
    let provider = Arc::new(RootProvider::<AnyNetwork>::new(RpcClient::mocked(
        asserter.clone(),
    )));
    let mut cache = EvmCache::new(provider.clone()).await;

    // No queued response → createAccessList errors → priming skipped. The local
    // discovery then runs: pre-seed slot 0 so its `get_dy` SLOAD does not fault,
    // and the stub fetcher refreshes it in the verify round. ZERO is the discover
    // call's gas-credited beneficiary.
    install_default_account(&mut cache, Address::ZERO);
    install_vault_runtime(
        &mut cache,
        pool,
        include_str!("fixtures/mock_curve_pool_runtime.hex"),
    );
    cache
        .db_mut()
        .insert_account_storage(pool, U256::ZERO, stale)?;
    cache.set_storage_batch_fetcher(fetcher_with_failures(
        HashMap::from([((pool, U256::ZERO), fresh)]),
        Vec::new(),
    ));

    let mut registry = AdapterRegistry::new();
    registry.register_adapter(Arc::new(CurveAdapter::default()))?;
    let mut registration = PoolRegistration::new(PoolKey::Curve(pool))
        .with_state_address(pool)
        .with_metadata(ProtocolMetadata::Curve(
            CurveMetadata::default()
                .with_coins(vec![Address::repeat_byte(0x01), Address::repeat_byte(0x02)])
                .with_variant(CurveVariant::StableSwap),
        ));

    let outcome = registry
        .cold_start_primed(&mut registration, &mut cache, provider.as_ref(), ColdStartPolicy::Eager)
        .await?;

    assert!(
        matches!(outcome, ColdStartOutcome::Ready(_)),
        "unsupported createAccessList must fall back to local discovery and still reach Ready, got {outcome:?}"
    );
    assert_eq!(registration.status, PoolStatus::Ready);
    assert_eq!(
        cache.cached_storage_value(pool, U256::ZERO),
        Some(fresh),
        "local discovery + verify refreshed the slot"
    );
    Ok(())
}

// A reverting `get_dy` discover call must be classified as a failed discovery
// (NeedsRepair via re-discovery), never silently driven to Ready.
#[tokio::test(flavor = "multi_thread")]
async fn curve_cold_start_reverting_discover_needs_repair() -> Result<()> {
    let pool = Address::repeat_byte(0xc2);

    let (mut cache, asserter) = setup_cache_with_asserter().await?;
    install_default_account(&mut cache, Address::ZERO);
    install_vault_runtime(
        &mut cache,
        pool,
        include_str!("fixtures/mock_balancer_vault_revert_runtime.hex"),
    );

    let registry = curve_registry();
    let mut registration = PoolRegistration::new(PoolKey::Curve(pool))
        .with_state_address(pool)
        .with_metadata(ProtocolMetadata::Curve(
            CurveMetadata::default()
                .with_coins(vec![Address::repeat_byte(0x01), Address::repeat_byte(0x02)])
                .with_discovered_slots(Vec::new())
                .with_variant(CurveVariant::StableSwap),
        ));

    let outcome = registry.cold_start(&mut registration, &mut cache, ColdStartPolicy::Eager)?;
    assert!(
        matches!(
            outcome,
            ColdStartOutcome::NeedsRepair(_, RepairAction::ColdStart { .. })
        ),
        "a reverting get_dy discover must need re-discovery, not reach Ready, got {outcome:?}"
    );
    assert_ne!(registration.status, PoolStatus::Ready);
    assert!(asserter.read_q().is_empty(), "must be fully offline");
    Ok(())
}

// The verify round warms the discovered read-set. If a discovered slot is
// unfetchable (an archive miss), the pool must NOT be marked Ready with an
// unwarmed read-set — it must need a `VerifySlots` repair over the discovered
// slots, mirroring the Balancer / V2 / V3 archive-miss behavior.
#[tokio::test(flavor = "multi_thread")]
async fn curve_cold_start_failed_slot_needs_verify_repair() -> Result<()> {
    let pool = Address::repeat_byte(0xc3);

    let (mut cache, _asserter) = setup_cache_with_asserter().await?;
    install_default_account(&mut cache, Address::ZERO);
    install_vault_runtime(
        &mut cache,
        pool,
        include_str!("fixtures/mock_curve_pool_runtime.hex"),
    );
    // Seed slot 0 so the discover call SLOADs it, but fail it in the verify round.
    cache
        .db_mut()
        .insert_account_storage(pool, U256::ZERO, U256::from(1_u64))?;
    cache.set_storage_batch_fetcher(fetcher_with_failures(
        HashMap::new(),
        vec![(pool, U256::ZERO)],
    ));

    let registry = curve_registry();
    let mut registration = PoolRegistration::new(PoolKey::Curve(pool))
        .with_state_address(pool)
        .with_metadata(ProtocolMetadata::Curve(
            CurveMetadata::default()
                .with_coins(vec![Address::repeat_byte(0x01), Address::repeat_byte(0x02)])
                .with_discovered_slots(Vec::new())
                .with_variant(CurveVariant::StableSwap),
        ));

    let outcome = registry.cold_start(&mut registration, &mut cache, ColdStartPolicy::Eager)?;
    assert!(
        matches!(
            outcome,
            ColdStartOutcome::NeedsRepair(_, RepairAction::VerifySlots(_))
        ),
        "an unfetchable discovered slot must need a VerifySlots repair, got {outcome:?}"
    );
    assert_ne!(registration.status, PoolStatus::Ready);
    Ok(())
}

// CryptoSwap (Curve v2) cold-start: same discover -> verify -> Ready machinery,
// but the discover `get_dy` uses the uint256-index ABI. The generic mock returns
// slot 0 for ANY selector, so it serves the uint256 ABI too; `finish` must
// persist `variant: CryptoSwap` alongside the discovered slots and the
// config-supplied coins.
#[tokio::test(flavor = "multi_thread")]
async fn curve_cryptoswap_cold_start_discover_verify_ready_persists_variant() -> Result<()> {
    let pool = Address::repeat_byte(0xc7);
    let usdt = Address::repeat_byte(0x01);
    let wbtc = Address::repeat_byte(0x02);
    let weth = Address::repeat_byte(0x03);
    let stale = U256::from(1_u64);
    let fresh = U256::from(147_348_u64);

    let (mut cache, asserter) = setup_cache_with_asserter().await?;
    install_default_account(&mut cache, Address::ZERO);
    install_vault_runtime(
        &mut cache,
        pool,
        include_str!("fixtures/mock_curve_pool_runtime.hex"),
    );
    cache
        .db_mut()
        .insert_account_storage(pool, U256::ZERO, stale)?;
    cache.set_storage_batch_fetcher(fetcher_with_failures(
        HashMap::from([((pool, U256::ZERO), fresh)]),
        Vec::new(),
    ));

    let registry = curve_registry();
    let mut registration = PoolRegistration::new(PoolKey::Curve(pool))
        .with_state_address(pool)
        .with_metadata(ProtocolMetadata::Curve(
            CurveMetadata::default()
                .with_coins(vec![usdt, wbtc, weth])
                .with_discovered_slots(Vec::new())
                .with_variant(CurveVariant::CryptoSwap),
        ));

    let outcome = registry.cold_start(&mut registration, &mut cache, ColdStartPolicy::Eager)?;

    assert!(
        matches!(outcome, ColdStartOutcome::Ready(_)),
        "CryptoSwap discover->verify should reach Ready, got {outcome:?}"
    );
    assert_eq!(registration.status, PoolStatus::Ready);
    match registration.metadata {
        ProtocolMetadata::Curve(ref m) => {
            assert_eq!(
                m.variant,
                CurveVariant::CryptoSwap,
                "cold-start must persist the CryptoSwap variant"
            );
            assert_eq!(
                m.coins,
                vec![usdt, wbtc, weth],
                "config coins must be preserved across cold-start"
            );
            assert!(
                m.discovered_slots.contains(&U256::ZERO),
                "the CryptoSwap get_dy read-set slot 0 must be discovered, got {:?}",
                m.discovered_slots
            );
        }
        ref other => panic!("expected Curve metadata, got {other:?}"),
    }
    assert_eq!(
        cache.cached_storage_value(pool, U256::ZERO),
        Some(fresh),
        "verify round must refresh the discovered slot to the fresh value"
    );
    assert!(
        asserter.read_q().is_empty(),
        "the cold start must be fully offline (no RPC)"
    );
    Ok(())
}
