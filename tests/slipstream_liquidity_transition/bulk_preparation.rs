//! Synthetic preparation-to-event regressions over the reviewed layouts.

use super::*;
use alloy_provider::{RootProvider, network::AnyNetwork};
use alloy_rpc_client::RpcClient;
use alloy_transport::mock::Asserter;
use evm_amm_state::adapters::AdapterEventError;
use evm_amm_state::adapters::v3_sync::{V3SyncSpec, build_full_sync_program, decode_full_sync};
use evm_fork_cache::cache::EvmCache;
use revm::context::result::ExecutionResult;
use revm::state::{AccountInfo, Bytecode};
use std::sync::Arc;
use std::time::Instant;

const OPTIMISM_POOL: Address = address!("173cdc71e29d5cffa6d090ad99f555a24b8831f9");

async fn full_sync_state(fixture: &TransitionFixture) -> FixtureState {
    let asserter = Asserter::new();
    let provider = RootProvider::<AnyNetwork>::new(RpcClient::mocked(asserter.clone()));
    let mut cache = EvmCache::new(Arc::new(provider)).await;
    asserter.push_failure_msg("unexpected provider access");
    cache
        .db_mut()
        .insert_account_info(Address::ZERO, AccountInfo::default());
    let spec = V3SyncSpec::slipstream(fixture.layout);
    let code = Bytecode::new_raw(build_full_sync_program(&spec));
    cache.db_mut().insert_account_info(
        fixture.pool,
        AccountInfo {
            code_hash: code.hash_slow(),
            code: Some(code),
            ..Default::default()
        },
    );
    // This synthetic contract defines every unseeded storage cell as zero.
    cache
        .db_mut()
        .replace_account_storage(fixture.pool, Default::default())
        .unwrap();
    for ((address, slot), value) in &fixture.state.0 {
        cache
            .db_mut()
            .insert_account_storage(*address, *slot, *value)
            .unwrap();
    }
    let result = cache
        .call_raw(Address::ZERO, fixture.pool, Bytes::new(), false)
        .unwrap();
    let ExecutionResult::Success { output, .. } = result else {
        panic!("full sync failed: {result:?}");
    };
    let snapshot = decode_full_sync(&spec, &output.into_data()).unwrap();
    assert_eq!(asserter.read_q().len(), 1, "full sync touched its provider");
    FixtureState(
        snapshot
            .storage_entries(&spec)
            .into_iter()
            .map(|(slot, value)| ((fixture.pool, slot), value))
            .collect(),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn bulk_prepared_state_supports_successive_burns_mints_and_tick_reinitialization() {
    let samples: usize = std::env::var("SLIPSTREAM_LIQUIDITY_SAMPLES")
        .map(|value| value.parse().expect("valid sample count"))
        .unwrap_or(1);
    assert!(samples > 0);
    for (chain_id, pool) in [(10, OPTIMISM_POOL), (8_453, POOL)] {
        for reward_growth in [U256::ZERO, U256::from(17)] {
            let mut fixture = TransitionFixture::new(pool, chain_id, -200, 200);
            fixture.seed_initialized_ticks(10);
            fixture.state.0.insert((pool, U256::from(9)), reward_growth);
            let parent = full_sync_state(&fixture).await;
            let mut durations = Vec::with_capacity(samples);
            for _ in 0..samples {
                fixture.state = parent.clone();
                let started = Instant::now();
                let mut liquidity = 10_u128;
                for (index, (is_mint, amount)) in [
                    (false, 3),
                    (true, 5),
                    (false, 12),
                    (true, 7),
                    (false, 7),
                    (true, 10),
                ]
                .into_iter()
                .enumerate()
                {
                    let was_empty = liquidity == 0;
                    fixture.context = AdapterEventContext::for_block(
                        100 + index as u64,
                        B256::repeat_byte(100 + index as u8),
                        1_001 + index as u64,
                    )
                    .with_chain_id(chain_id)
                    .with_parent_hash(B256::repeat_byte(99 + index as u8))
                    .with_transaction_hash(TX_HASH)
                    .with_event_order(0, 0);
                    let result = fixture.decode(&liquidity_log(pool, is_mint, -200, 200, amount));
                    assert_eq!(result.error, None, "chain {chain_id}, event {index}");
                    let event = result.event.expect("recognized liquidity event");
                    assert_eq!(event.quality, UpdateQuality::Exact);
                    assert_eq!(event.repair, RepairAction::None);
                    fixture.state = apply(&fixture.state, &event.updates);
                    liquidity = if is_mint {
                        liquidity + amount
                    } else {
                        liquidity - amount
                    };
                    assert_eq!(
                        fixture
                            .state
                            .storage(pool, fixture.layout.liquidity_slot)
                            .unwrap()
                            & WORD_128_MASK,
                        U256::from(990 + liquidity),
                    );
                    for keys in [fixture.lower_keys, fixture.upper_keys] {
                        assert_eq!(
                            fixture.state.storage(pool, keys[0]).unwrap() & WORD_128_MASK,
                            U256::from(liquidity),
                        );
                        if liquidity == 0 {
                            for slot in keys {
                                assert_eq!(fixture.state.storage(pool, slot), Some(U256::ZERO));
                            }
                        }
                    }
                    assert_eq!(
                        fixture.state.storage(pool, fixture.lower_bitmap),
                        Some(if liquidity == 0 {
                            U256::ZERO
                        } else {
                            U256::from(1) << 255
                        }),
                    );
                    assert_eq!(
                        fixture.state.storage(pool, fixture.upper_bitmap),
                        Some(if liquidity == 0 {
                            U256::ZERO
                        } else {
                            U256::from(1) << 1
                        }),
                    );
                    if was_empty {
                        assert_eq!(
                            fixture.state.storage(pool, fixture.lower_keys[4]),
                            Some(if chain_id == 10 {
                                reward_growth
                            } else {
                                U256::ZERO
                            }),
                        );
                        assert_eq!(
                            fixture.state.storage(pool, fixture.upper_keys[4]),
                            Some(U256::ZERO),
                        );
                    }
                    assert_eq!(
                        fixture.state.storage(pool, U256::from(9)),
                        Some(reward_growth)
                    );
                    let collect = fixture.decode(&collect_log(pool, -200, 200));
                    assert_eq!(collect.error, None);
                    let event = collect.event.unwrap();
                    assert_eq!(event.quality, UpdateQuality::Exact);
                    assert_eq!(event.repair, RepairAction::None);
                    assert!(event.updates.is_empty());
                }
                durations.push(started.elapsed().as_nanos());
            }
            durations.sort_unstable();
            eprintln!(
                "liquidity replay chain={chain_id} reward={reward_growth} samples={samples} \
                 events_per_sample=12 provider_reads=0 p50_ns={} p95_ns={} p99_ns={} max_ns={}",
                durations[(samples - 1) / 2],
                durations[(samples - 1) * 95 / 100],
                durations[(samples - 1) * 99 / 100],
                durations[samples - 1],
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_reward_global_still_rejects_optimism_mint_and_burn() {
    let mut fixture = TransitionFixture::new(OPTIMISM_POOL, 10, -200, 200);
    fixture.seed_initialized_ticks(10);
    fixture.state = full_sync_state(&fixture).await;
    fixture.state.0.remove(&(fixture.pool, U256::from(9)));
    for is_mint in [false, true] {
        let result = fixture.decode(&liquidity_log(fixture.pool, is_mint, -200, 200, 3));
        assert_eq!(
            result.error,
            Some(AdapterEventError::MissingState {
                address: fixture.pool,
                slot: U256::from(9)
            }),
        );
        let event = result.event.unwrap();
        assert_eq!(event.quality, UpdateQuality::RequiresRepair);
        assert_eq!(event.repair, RepairAction::PurgeStorage(fixture.pool));
        assert!(matches!(
            event.updates.as_slice(),
            [StateUpdate::Purge { .. }]
        ));
    }
}
