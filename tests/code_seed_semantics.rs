//! MANAGER-AUTHORED acceptance tests for verified-code-seed *semantics* during
//! cold-start. The implementation agent must make these pass WITHOUT modifying
//! them.
//!
//! The governing principle: **seeding is an optimization, never a gate.** A
//! pool's own runtime code is otherwise lazily fetched at first simulate (see
//! `sim.rs`), so a seed that fails to verify must degrade to exactly that
//! lazy-fetch fallback — never to a fatal error or a permanently `Degraded`
//! pool. `evm-fork-cache` purges a contradicted seed; the pool then proceeds
//! with its normal cold-start outcome and the mismatch is *surfaced*, not acted
//! on as a repair.
//!
//! All tests run fully offline over a mocked provider + stub fetchers.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use alloy_eips::BlockId;
use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_provider::{RootProvider, network::AnyNetwork};
use alloy_rpc_client::RpcClient;
use alloy_transport::mock::Asserter;
use anyhow::Result;

use evm_amm_state::adapters::storage::{V2_RESERVES_SLOT, V2_TOKEN0_SLOT, V2_TOKEN1_SLOT};
use evm_amm_state::adapters::{
    AdapterRegistry, CodeSeedMismatch, CodeSeedReport, ColdStartOutcome, ColdStartPolicy, PoolKey,
    PoolRegistration, PoolStatus, ProtocolMetadata, UniswapV2Adapter, UniswapV2Metadata,
    uniswap_v2_pair_runtime_code_hash,
};
use evm_fork_cache::AccountFieldsSample;
use evm_fork_cache::cache::{AccountFieldsFetchFn, CodeSeedState, EvmCache, StorageBatchFetchFn};
use revm::state::{AccountInfo, Bytecode};

// --- helpers ---

async fn setup_cache() -> Result<EvmCache> {
    let asserter = Asserter::new();
    let client = RpcClient::mocked(asserter);
    let provider = RootProvider::<AnyNetwork>::new(client);
    Ok(EvmCache::new(Arc::new(provider)).await)
}

fn stub_fetcher(values: HashMap<(Address, U256), U256>) -> StorageBatchFetchFn {
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

fn v2_registry() -> AdapterRegistry {
    let mut registry = AdapterRegistry::new();
    registry
        .register_adapter(Arc::new(UniswapV2Adapter::default()))
        .unwrap();
    registry
}

/// A V2 pool whose three warmed slots the stub fetcher serves, so cold-start
/// itself always reaches `Ready`; only code-seed behavior varies per test.
fn v2_pool_cache_and_registration(
    pool: Address,
) -> (HashMap<(Address, U256), U256>, PoolRegistration) {
    let token0 = Address::repeat_byte(0xb0);
    let token1 = Address::repeat_byte(0xb1);
    let storage = HashMap::from([
        ((pool, V2_TOKEN0_SLOT), token_slot_word(token0)),
        ((pool, V2_TOKEN1_SLOT), token_slot_word(token1)),
        (
            (pool, V2_RESERVES_SLOT),
            reserves_slot(U256::from(7_u64), U256::from(9_u64), U256::ZERO),
        ),
    ]);
    let registration = PoolRegistration::new(PoolKey::UniswapV2(pool))
        .with_state_address(pool)
        .with_metadata(ProtocolMetadata::UniswapV2(
            UniswapV2Metadata::default().with_fee_bps(30),
        ));
    (storage, registration)
}

// --- semantics tests ---

/// A code-hash mismatch is NON-FATAL: the pool cold-starts to `Ready` over its
/// (lazily fetched) real code, and the contradicted seed is purged. It must not
/// degrade the pool or return a repair action.
#[tokio::test(flavor = "multi_thread")]
async fn code_hash_mismatch_is_non_fatal_and_purges_seed() -> Result<()> {
    let pool = Address::repeat_byte(0x41);
    let (storage, mut registration) = v2_pool_cache_and_registration(pool);
    let wrong_hash = B256::repeat_byte(0x77);

    let mut cache = setup_cache().await?;
    cache.set_storage_batch_fetcher(stub_fetcher(storage));
    cache.set_account_fields_fetcher(account_fields_fetcher(
        HashMap::from([(pool, (U256::ZERO, wrong_hash))]),
        Arc::new(AtomicUsize::new(0)),
    ));

    let registry = v2_registry();
    let outcome = registry.cold_start(&mut registration, &mut cache, ColdStartPolicy::Eager)?;

    assert!(
        matches!(outcome, ColdStartOutcome::Ready(_)),
        "a code-hash mismatch must not block the pool; got {outcome:?}"
    );
    assert_eq!(
        registration.status,
        PoolStatus::Ready,
        "a code-hash mismatch must not degrade the pool"
    );
    assert!(
        cache.code_seed_state(&pool).is_none(),
        "the contradicted seed must be purged, leaving the address unmarked (RPC-origin)"
    );
    Ok(())
}

/// A verified seed is confirmed exactly once and never re-verified on later
/// cold-starts: no second account-fields call.
#[tokio::test(flavor = "multi_thread")]
async fn verified_seed_is_confirmed_once() -> Result<()> {
    let pool = Address::repeat_byte(0x42);
    let (storage, mut registration) = v2_pool_cache_and_registration(pool);
    let expected_hash = uniswap_v2_pair_runtime_code_hash();
    let calls = Arc::new(AtomicUsize::new(0));

    let mut cache = setup_cache().await?;
    cache.set_storage_batch_fetcher(stub_fetcher(storage));
    cache.set_account_fields_fetcher(account_fields_fetcher(
        HashMap::from([(pool, (U256::from(5_u64), expected_hash))]),
        calls.clone(),
    ));

    let registry = v2_registry();
    let first = registry.cold_start(&mut registration, &mut cache, ColdStartPolicy::Eager)?;
    assert!(matches!(first, ColdStartOutcome::Ready(_)), "got {first:?}");
    assert!(matches!(
        cache.code_seed_state(&pool),
        Some(CodeSeedState::Verified { code_hash, .. }) if *code_hash == expected_hash
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 1, "seed verified once");

    let second = registry.cold_start(&mut registration, &mut cache, ColdStartPolicy::Eager)?;
    assert!(
        matches!(second, ColdStartOutcome::Ready(_)),
        "got {second:?}"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "an already-Verified seed must not trigger a second account-fields call"
    );
    Ok(())
}

/// When the cache already holds authoritative RPC-origin code with a *different*
/// hash (e.g. a fork variant of the pool), seeding must be SKIPPED, not raised
/// as a `CodeSeedConflict` error — the pre-existing code is authoritative.
#[tokio::test(flavor = "multi_thread")]
async fn conflict_with_existing_rpc_code_is_skipped_not_error() -> Result<()> {
    let pool = Address::repeat_byte(0x43);
    let (storage, mut registration) = v2_pool_cache_and_registration(pool);

    let mut cache = setup_cache().await?;
    cache.set_storage_batch_fetcher(stub_fetcher(storage));
    cache.set_account_fields_fetcher(account_fields_fetcher(
        HashMap::from([(pool, (U256::ZERO, B256::repeat_byte(0xee)))]),
        Arc::new(AtomicUsize::new(0)),
    ));
    // Pre-install unmarked (RPC-origin) code whose hash differs from the seed.
    let foreign = Bytecode::new_raw(Bytes::from_static(&[0x60, 0x00, 0x60, 0x00, 0xfd]));
    let foreign_hash = foreign.hash_slow();
    cache.db_mut().insert_account_info(
        pool,
        AccountInfo {
            balance: U256::ZERO,
            nonce: 1,
            code: Some(foreign),
            code_hash: foreign_hash,
            account_id: None,
        },
    );

    let registry = v2_registry();
    let outcome = registry.cold_start(&mut registration, &mut cache, ColdStartPolicy::Eager)?;

    assert!(
        matches!(outcome, ColdStartOutcome::Ready(_)),
        "a warm-cache code conflict must be skipped, not fatal; got {outcome:?}"
    );
    assert!(
        cache.code_seed_state(&pool).is_none(),
        "the pre-existing RPC-origin code must be left untouched and unmarked"
    );
    Ok(())
}

/// A deliberate local etch is NEVER overwritten by seeding: it must survive
/// cold-start with its `Etched` mark intact.
#[tokio::test(flavor = "multi_thread")]
async fn etched_code_is_never_overwritten() -> Result<()> {
    let pool = Address::repeat_byte(0x44);
    let (storage, mut registration) = v2_pool_cache_and_registration(pool);

    let mut cache = setup_cache().await?;
    cache.set_storage_batch_fetcher(stub_fetcher(storage));
    cache.set_account_fields_fetcher(account_fields_fetcher(
        HashMap::from([(pool, (U256::ZERO, uniswap_v2_pair_runtime_code_hash()))]),
        Arc::new(AtomicUsize::new(0)),
    ));
    let etched = Bytecode::new_raw(Bytes::from_static(&[0x60, 0x2a, 0x60, 0x00, 0x52]));
    let etched_hash = cache.etch_account_code(pool, etched.original_bytes())?;

    let registry = v2_registry();
    let outcome = registry.cold_start(&mut registration, &mut cache, ColdStartPolicy::Eager)?;

    assert!(
        matches!(outcome, ColdStartOutcome::Ready(_)),
        "got {outcome:?}"
    );
    assert!(
        matches!(
            cache.code_seed_state(&pool),
            Some(CodeSeedState::Etched { code_hash }) if *code_hash == etched_hash
        ),
        "an Etched mark must survive cold-start untouched, got {:?}",
        cache.code_seed_state(&pool)
    );
    Ok(())
}

/// When verification cannot complete (the fetcher returns no sample for the
/// address, or errors), the seed lands `unverifiable` and must be PURGED rather
/// than left `Pending`: the pool must never simulate over unverified code. It
/// falls back to lazy real-code fetch and still reaches `Ready`.
#[tokio::test(flavor = "multi_thread")]
async fn unverifiable_seed_is_purged_and_non_fatal() -> Result<()> {
    let pool = Address::repeat_byte(0x45);
    let (storage, mut registration) = v2_pool_cache_and_registration(pool);

    let mut cache = setup_cache().await?;
    cache.set_storage_batch_fetcher(stub_fetcher(storage));
    // Fetcher returns no sample for the pool → the seed is unverifiable.
    cache.set_account_fields_fetcher(account_fields_fetcher(
        HashMap::new(),
        Arc::new(AtomicUsize::new(0)),
    ));

    let registry = v2_registry();
    let outcome = registry.cold_start(&mut registration, &mut cache, ColdStartPolicy::Eager)?;

    assert!(
        matches!(outcome, ColdStartOutcome::Ready(_)),
        "got {outcome:?}"
    );
    assert_eq!(
        registration.status,
        PoolStatus::Ready,
        "an unverifiable seed must not degrade the pool"
    );
    assert!(
        cache.code_seed_state(&pool).is_none(),
        "an unverifiable seed must be purged, never left Pending (would serve unverified code)"
    );
    Ok(())
}

/// Code seeding can be disabled on the registry; cold-start then performs no
/// seeding and no verification call, and the pool is still `Ready`.
#[tokio::test(flavor = "multi_thread")]
async fn code_seeding_can_be_disabled() -> Result<()> {
    let pool = Address::repeat_byte(0x47);
    let (storage, mut registration) = v2_pool_cache_and_registration(pool);
    let calls = Arc::new(AtomicUsize::new(0));

    let mut cache = setup_cache().await?;
    cache.set_storage_batch_fetcher(stub_fetcher(storage));
    cache.set_account_fields_fetcher(account_fields_fetcher(
        HashMap::from([(pool, (U256::ZERO, uniswap_v2_pair_runtime_code_hash()))]),
        calls.clone(),
    ));

    let mut registry = AdapterRegistry::new();
    registry
        .register_adapter(Arc::new(UniswapV2Adapter::default()))
        .unwrap();
    let registry = registry.with_code_seeding(false);

    let outcome = registry.cold_start(&mut registration, &mut cache, ColdStartPolicy::Eager)?;
    assert!(
        matches!(outcome, ColdStartOutcome::Ready(_)),
        "got {outcome:?}"
    );
    assert!(
        cache.code_seed_state(&pool).is_none(),
        "seeding disabled means the address stays unmarked"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "seeding disabled means no account-fields verification call"
    );
    Ok(())
}

/// The `CodeVerifyReport` is surfaced through the cold-start report so a caller
/// can observe verification results (here, a mismatch) even though the pool is
/// `Ready`.
#[tokio::test(flavor = "multi_thread")]
async fn code_verification_results_are_surfaced_in_report() -> Result<()> {
    let pool = Address::repeat_byte(0x46);
    let (storage, mut registration) = v2_pool_cache_and_registration(pool);
    let wrong_hash = B256::repeat_byte(0x77);

    let mut cache = setup_cache().await?;
    cache.set_storage_batch_fetcher(stub_fetcher(storage));
    cache.set_account_fields_fetcher(account_fields_fetcher(
        HashMap::from([(pool, (U256::ZERO, wrong_hash))]),
        Arc::new(AtomicUsize::new(0)),
    ));

    let registry = v2_registry();
    let outcome = registry.cold_start(&mut registration, &mut cache, ColdStartPolicy::Eager)?;
    let ColdStartOutcome::Ready(report) = outcome else {
        panic!("expected Ready, got {outcome:?}");
    };

    let seeds: CodeSeedReport = report
        .code_seeds
        .expect("code-seed verification results must be surfaced when seeding ran");
    assert!(seeds.verified.is_empty());
    assert_eq!(seeds.mismatched.len(), 1, "the mismatch must be reported");
    assert_eq!(
        seeds.mismatched[0],
        CodeSeedMismatch::new(pool, uniswap_v2_pair_runtime_code_hash(), wrong_hash)
    );
    Ok(())
}
