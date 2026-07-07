//! MANAGER-AUTHORED acceptance tests for Cycle 4: `AdapterRegistry::cold_start_many`
//! (bundled one-shot hydration with graceful per-pool fallback) and the
//! `supports_one_shot_hydration` classification. The implementation agent must
//! make these pass WITHOUT modifying them.
//!
//! The actual bundled `run_storage_programs` fast path executes against a live
//! node, so it is exercised by the env-gated example, not here. Offline these
//! tests pin: (1) which pools are fast-hydration-eligible, and (2) that
//! `cold_start_many` seeds+verifies, then — when the one-shot hydration cannot
//! run (empty mock provider → the `eth_call` errors) — falls back per pool to
//! the normal cold-start and still finalizes every pool `Ready`, in input order.

use std::collections::HashMap;
use std::sync::Arc;

use alloy_eips::BlockId;
use alloy_primitives::{Address, B256, U256};
use alloy_provider::{RootProvider, network::AnyNetwork};
use alloy_rpc_client::RpcClient;
use alloy_transport::mock::Asserter;
use anyhow::Result;

use evm_amm_state::adapters::storage::{
    V2_RESERVES_SLOT, V2_TOKEN0_SLOT, V2_TOKEN1_SLOT, V3StorageLayout,
};
use evm_amm_state::adapters::{
    AdapterRegistry, ColdStartOutcome, ColdStartPolicy, CurveMetadata, CurveVariant, PoolKey,
    PoolRegistration, PoolStatus, ProtocolMetadata, UniswapV2Adapter, UniswapV2Metadata,
    V3Metadata, supports_one_shot_hydration, uniswap_v2_pair_runtime_code_hash,
};
use evm_fork_cache::AccountFieldsSample;
use evm_fork_cache::cache::{AccountFieldsFetchFn, EvmCache, StorageBatchFetchFn};

/// A provider over an empty mock: any `eth_call` (e.g. a one-shot hydration
/// program) errors, so `cold_start_many` must fall back gracefully.
fn empty_mock_provider() -> Arc<RootProvider<AnyNetwork>> {
    let client = RpcClient::mocked(Asserter::new());
    Arc::new(RootProvider::<AnyNetwork>::new(client))
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

fn account_fields_fetcher(values: HashMap<Address, (U256, B256)>) -> AccountFieldsFetchFn {
    Arc::new(move |addresses: Vec<Address>, _block: BlockId| {
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

fn token_word(addr: Address) -> U256 {
    U256::from_be_slice(addr.as_slice())
}

fn reserves(reserve0: u64, reserve1: u64) -> U256 {
    U256::from(reserve0) | (U256::from(reserve1) << 112)
}

/// A metadata-complete Uniswap V2 registration (token0/token1 + fee): eligible
/// for the fast one-shot path (see `fast_metadata_complete`), so it exercises the
/// fast→fallback transition rather than being diverted to fallback by the gate.
fn v2_registration(pool: Address) -> PoolRegistration {
    PoolRegistration::new(PoolKey::UniswapV2(pool))
        .with_state_address(pool)
        .with_metadata(ProtocolMetadata::UniswapV2(
            UniswapV2Metadata::default()
                .with_token0(Address::repeat_byte(0xa0))
                .with_token1(Address::repeat_byte(0xa1))
                .with_fee_bps(30),
        ))
}

/// `cold_start_many` over fast-eligible pools whose one-shot hydration cannot
/// execute (empty mock provider) must seed+verify them and then fall back per
/// pool to the normal cold-start, finalizing every pool `Ready` in input order.
#[tokio::test(flavor = "multi_thread")]
async fn cold_start_many_falls_back_to_ready_when_hydration_cannot_run() -> Result<()> {
    let pool_a = Address::repeat_byte(0x51);
    let pool_b = Address::repeat_byte(0x52);
    let provider = empty_mock_provider();
    let mut cache = EvmCache::new(provider.clone()).await;

    let mut storage = HashMap::new();
    for (i, pool) in [pool_a, pool_b].into_iter().enumerate() {
        let t0 = Address::repeat_byte(0x60 + i as u8);
        let t1 = Address::repeat_byte(0x70 + i as u8);
        storage.insert((pool, V2_TOKEN0_SLOT), token_word(t0));
        storage.insert((pool, V2_TOKEN1_SLOT), token_word(t1));
        storage.insert(
            (pool, V2_RESERVES_SLOT),
            reserves(10 + i as u64, 20 + i as u64),
        );
    }
    cache.set_storage_batch_fetcher(stub_fetcher(storage));
    let expected_hash = uniswap_v2_pair_runtime_code_hash();
    cache.set_account_fields_fetcher(account_fields_fetcher(HashMap::from([
        (pool_a, (U256::ZERO, expected_hash)),
        (pool_b, (U256::ZERO, expected_hash)),
    ])));

    let mut registry = AdapterRegistry::new();
    registry.register_adapter(Arc::new(UniswapV2Adapter::default()))?;

    let mut pools = vec![v2_registration(pool_a), v2_registration(pool_b)];
    let outcomes = registry
        .cold_start_many(
            &mut pools,
            &mut cache,
            provider.as_ref(),
            ColdStartPolicy::Eager,
        )
        .await?;

    assert_eq!(outcomes.len(), 2, "one outcome per input pool");
    assert!(
        outcomes
            .iter()
            .all(|o| matches!(o, ColdStartOutcome::Ready(_))),
        "every pool must finalize Ready (fast path or fallback); got {outcomes:?}"
    );
    assert_eq!(
        pools[0].key,
        PoolKey::UniswapV2(pool_a),
        "input order preserved"
    );
    assert_eq!(pools[1].key, PoolKey::UniswapV2(pool_b));
    assert!(pools.iter().all(|p| p.status == PoolStatus::Ready));
    Ok(())
}

/// The metadata-completeness gate: a registration still missing its identity
/// metadata (a Uniswap V2 pool with no `token0`/`token1`) must NOT be finalized
/// by the fast path — it falls back to the normal `cold_start`, whose planner
/// decodes and merges the tokens from storage. Without the gate the fast path
/// would mark it `Ready` with `token0`/`token1` still `None` (finish() skipped).
#[tokio::test(flavor = "multi_thread")]
async fn cold_start_many_incomplete_metadata_falls_back_and_merges_tokens() -> Result<()> {
    let pool = Address::repeat_byte(0x55);
    let t0 = Address::repeat_byte(0x66);
    let t1 = Address::repeat_byte(0x77);
    let provider = empty_mock_provider();
    let mut cache = EvmCache::new(provider.clone()).await;

    cache.set_storage_batch_fetcher(stub_fetcher(HashMap::from([
        ((pool, V2_TOKEN0_SLOT), token_word(t0)),
        ((pool, V2_TOKEN1_SLOT), token_word(t1)),
        ((pool, V2_RESERVES_SLOT), reserves(10, 20)),
    ])));

    let mut registry = AdapterRegistry::new();
    registry.register_adapter(Arc::new(UniswapV2Adapter::default()))?;

    // No token0/token1 in metadata → not fast-eligible → must fall back.
    let mut pools = vec![
        PoolRegistration::new(PoolKey::UniswapV2(pool))
            .with_state_address(pool)
            .with_metadata(ProtocolMetadata::UniswapV2(
                UniswapV2Metadata::default().with_fee_bps(30),
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

    assert!(matches!(outcomes[0], ColdStartOutcome::Ready(_)));
    assert_eq!(pools[0].status, PoolStatus::Ready);
    // The fallback cold_start decoded + merged the tokens from storage; the fast
    // path (which skips finish()) would have left them None.
    match &pools[0].metadata {
        ProtocolMetadata::UniswapV2(m) => {
            assert_eq!(m.token0, Some(t0), "fallback must merge decoded token0");
            assert_eq!(m.token1, Some(t1), "fallback must merge decoded token1");
            assert_eq!(m.fee_bps, Some(30), "config fee_bps must survive the merge");
        }
        other => panic!("expected UniswapV2 metadata, got {other:?}"),
    }
    Ok(())
}

/// An empty pool slice is a no-op that returns no outcomes (and touches nothing).
#[tokio::test(flavor = "multi_thread")]
async fn cold_start_many_empty_is_noop() -> Result<()> {
    let provider = empty_mock_provider();
    let mut cache = EvmCache::new(provider.clone()).await;
    let registry = AdapterRegistry::new();
    let mut pools: Vec<PoolRegistration> = Vec::new();
    let outcomes = registry
        .cold_start_many(
            &mut pools,
            &mut cache,
            provider.as_ref(),
            ColdStartPolicy::Eager,
        )
        .await?;
    assert!(outcomes.is_empty());
    Ok(())
}

/// The fast-path classification: Uniswap V2 and V3-with-layout pools support
/// one-shot hydration; a V3 pool missing its storage layout and a Curve pool
/// (no persisted flat read-set) do not.
#[test]
fn supports_one_shot_hydration_classifies_by_protocol_and_metadata() {
    let v2 = v2_registration(Address::repeat_byte(0x01));
    assert!(
        supports_one_shot_hydration(&v2),
        "V2 has a fixed flat read-set"
    );

    let v3_ready = PoolRegistration::new(PoolKey::UniswapV3(Address::repeat_byte(0x02)))
        .with_state_address(Address::repeat_byte(0x02))
        .with_metadata(ProtocolMetadata::UniswapV3(
            V3Metadata::default()
                .with_token0(Address::repeat_byte(0x0a))
                .with_token1(Address::repeat_byte(0x0b))
                .with_fee(500)
                .with_tick_spacing(10)
                .with_storage_layout(V3StorageLayout::uniswap(10)),
        ));
    assert!(
        supports_one_shot_hydration(&v3_ready),
        "V3 with a storage layout can run the one-shot full sync"
    );

    let v3_no_layout = PoolRegistration::new(PoolKey::UniswapV3(Address::repeat_byte(0x03)))
        .with_state_address(Address::repeat_byte(0x03))
        .with_metadata(ProtocolMetadata::UniswapV3(
            V3Metadata::default()
                .with_token0(Address::repeat_byte(0x0a))
                .with_token1(Address::repeat_byte(0x0b))
                .with_fee(500),
        ));
    assert!(
        !supports_one_shot_hydration(&v3_no_layout),
        "V3 without a storage layout cannot build the one-shot program"
    );

    let curve = PoolRegistration::new(PoolKey::Curve(Address::repeat_byte(0x04)))
        .with_state_address(Address::repeat_byte(0x04))
        .with_metadata(ProtocolMetadata::Curve(
            CurveMetadata::default().with_variant(CurveVariant::StableSwap),
        ));
    assert!(
        !supports_one_shot_hydration(&curve),
        "Curve has no persisted flat read-set until discovery runs"
    );
}
