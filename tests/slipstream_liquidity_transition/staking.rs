//! Continuous quote-state updates through the public synchronization engine.
use super::*;
use alloy_provider::{RootProvider, network::AnyNetwork};
use alloy_rpc_client::RpcClient;
use alloy_rpc_types_eth::Log as RpcLog;
use alloy_transport::mock::Asserter;
use evm_amm_state::adapters::{AdapterRegistry, AmmSyncEngine, PoolStatus};
use evm_fork_cache::cache::EvmCache;
use evm_fork_cache::reactive::{
    BlockRef, ChainStatus, InputSource, ReactiveContext, ReactiveInput, ReactiveInputBatch,
    ReactiveInputRecord,
};
use std::sync::Arc;

pub(super) const OP_POOL: Address = address!("173cdc71e29d5cffa6d090ad99f555a24b8831f9");
pub(super) const GAUGE: Address = address!("41160e66fcaa10cbb148ace60bc2a22d609ec519");
pub(super) const NFT: Address = address!("416b433906b1b72fa758e166e239c43d68dc6f29");
pub(super) const OWNER: Address = Address::repeat_byte(0x42);

fn raw(address: Address, signature: &str, topics: Vec<B256>, words: Vec<U256>) -> Log {
    Log::new(
        address,
        std::iter::once(keccak256(signature))
            .chain(topics)
            .collect(),
        words
            .into_iter()
            .flat_map(|word| word.to_be_bytes::<32>())
            .collect::<Vec<_>>()
            .into(),
    )
    .unwrap()
}

pub(super) fn staking_logs(deposit: bool, amount: u128) -> Vec<(u64, Log)> {
    let token = B256::from(U256::from(7).to_be_bytes::<32>());
    vec![
        (
            1,
            raw(
                OP_POOL,
                "Collect(address,address,int24,int24,uint128,uint128)",
                vec![
                    topic_address(if deposit { NFT } else { GAUGE }),
                    topic_i24(-200),
                    topic_i24(200),
                ],
                vec![
                    U256::from_be_slice(OWNER.as_slice()),
                    U256::ZERO,
                    U256::ZERO,
                ],
            ),
        ),
        // The verified manager emits MetadataUpdate between these two Collect events.
        (
            3,
            raw(
                NFT,
                "Collect(uint256,address,uint256,uint256)",
                vec![token],
                vec![
                    U256::from_be_slice(OWNER.as_slice()),
                    U256::ZERO,
                    U256::ZERO,
                ],
            ),
        ),
        (
            4,
            raw(
                NFT,
                "Transfer(address,address,uint256)",
                vec![
                    topic_address(if deposit { OWNER } else { GAUGE }),
                    topic_address(if deposit { GAUGE } else { OWNER }),
                    token,
                ],
                vec![],
            ),
        ),
        (
            5,
            raw(
                GAUGE,
                if deposit {
                    "Deposit(address,uint256,uint128)"
                } else {
                    "Withdraw(address,uint256,uint128)"
                },
                vec![
                    topic_address(OWNER),
                    token,
                    B256::from(U256::from(amount).to_be_bytes::<32>()),
                ],
                vec![],
            ),
        ),
    ]
}

pub(super) fn batch(logs: Vec<(u64, Log)>, number: u64) -> ReactiveInputBatch {
    let block = BlockRef {
        number,
        hash: B256::from(U256::from(number).to_be_bytes::<32>()),
        parent_hash: Some(B256::from(U256::from(number - 1).to_be_bytes::<32>())),
        timestamp: Some(1_000 + number),
    };
    ReactiveInputBatch::new(
        logs.into_iter()
            .map(|(index, inner)| {
                let log = RpcLog {
                    inner,
                    block_hash: Some(block.hash),
                    block_number: Some(number),
                    block_timestamp: block.timestamp,
                    transaction_hash: Some(TX_HASH),
                    transaction_index: Some(0),
                    log_index: Some(index),
                    removed: false,
                };
                ReactiveInputRecord::new(
                    ReactiveInput::Log(log),
                    ReactiveContext {
                        chain_id: Some(10),
                        source: InputSource::Synthetic,
                        chain_status: ChainStatus::Included {
                            block,
                            confirmations: 0,
                        },
                        block: Some(block),
                        transaction_index: Some(0),
                        log_index: Some(index),
                    },
                )
            })
            .collect(),
    )
}

pub(super) async fn setup() -> (TransitionFixture, EvmCache, AmmSyncEngine, Asserter) {
    let mut fixture = TransitionFixture::new(OP_POOL, 10, -200, 200);
    fixture.seed_initialized_ticks(1_000);
    fixture
        .state
        .0
        .insert((OP_POOL, U256::from(15)), U256::from(200) << 160);
    let asserter = Asserter::new();
    let provider = RootProvider::<AnyNetwork>::new(RpcClient::mocked(asserter.clone()));
    let mut cache = EvmCache::new(Arc::new(provider)).await;
    cache.set_chain_id(10);
    for (address, hash) in [
        (
            OP_POOL,
            b256!("063ca35333cb7f2463f087d40ff9485475550abf4858a2f63c387d4d102b0f4f"),
        ),
        (
            address!("c28ad28853a547556780bebf7847628501a3bcbb"),
            b256!("36c3da904ca0b58544254cd0d978fe4801c32dc1f9e3b3e644487ef541299794"),
        ),
        (
            GAUGE,
            b256!("576a24f21989bca966fb1698aa060e9957ceabe9a970dadb98ba8dc740e845f1"),
        ),
        (
            address!("7155b84a704f0657975827c65ff6fe42e3a962bb"),
            b256!("eb344e9f4bd301360b265a45cf417685e7ba46f7c53e03b592e7fc1fad53abdb"),
        ),
        (
            NFT,
            b256!("e46c8b86983505f15a6598c270339e696f5465478a32f17f1026d76e36fd69de"),
        ),
    ] {
        cache.db_mut().insert_account_info(
            address,
            revm::state::AccountInfo {
                code_hash: hash,
                ..Default::default()
            },
        );
    }
    fixture.state.0.insert(
        (OP_POOL, U256::from(3)),
        U256::from_be_slice(GAUGE.as_slice()),
    );
    fixture.state.0.insert(
        (OP_POOL, U256::from(4)),
        U256::from_be_slice(NFT.as_slice()),
    );
    asserter.push_failure_msg("staking must not access the provider");
    for ((address, slot), value) in &fixture.state.0 {
        cache.apply_updates(&[evm_fork_cache::StateUpdate::slot(*address, *slot, *value)]);
    }
    let mut registry = AdapterRegistry::new();
    registry
        .register_adapter(Arc::new(ConcentratedLiquidityAdapter::default()))
        .unwrap();
    registry
        .register_pool(fixture.registration.clone().with_status(PoolStatus::Ready))
        .unwrap();
    let engine = AmmSyncEngine::new(registry).unwrap();
    (fixture, cache, engine, asserter)
}

#[tokio::test]
async fn gauge_deposit_advances_quote_state_without_provider_reads_or_repairs() {
    let (fixture, mut cache, mut engine, asserter) = setup().await;
    let report = engine
        .ingest_batch(&mut cache, batch(staking_logs(true, 400), 100))
        .unwrap();
    assert_eq!(
        cache.cached_storage_value(OP_POOL, U256::from(15)).unwrap() & WORD_128_MASK,
        U256::from(400),
        "staking changes must reach the persistent quote state"
    );
    assert_eq!(
        cache.cached_storage_value(OP_POOL, fixture.lower_keys[1]),
        Some(U256::from(400))
    );
    assert_eq!(
        cache.cached_storage_value(OP_POOL, fixture.upper_keys[1]),
        Some(U256::from((-400_i128) as u128))
    );
    assert!(report.degraded_pools.is_empty());
    assert_eq!(report.resync_state_updates, 0);
    assert_eq!(
        asserter.read_q().len(),
        1,
        "no provider request is permitted"
    );
}

#[tokio::test]
async fn unverified_staking_runtime_is_rejected_before_mutating_quote_state() {
    for address in [
        NFT,
        GAUGE,
        address!("7155b84a704f0657975827c65ff6fe42e3a962bb"),
        OP_POOL,
        address!("c28ad28853a547556780bebf7847628501a3bcbb"),
    ] {
        let (_, mut cache, mut engine, _) = setup().await;
        cache
            .db_mut()
            .insert_account_info(address, revm::state::AccountInfo::default());
        let report = engine
            .ingest_batch(&mut cache, batch(staking_logs(true, 400), 100))
            .unwrap();
        assert_eq!(
            report.degraded_pools,
            vec![PoolKey::Slipstream(OP_POOL)],
            "unverified runtime {address}"
        );
        assert!(
            cache
                .cached_storage_value(OP_POOL, U256::from(15))
                .is_none()
        );
    }
}

#[tokio::test]
async fn withdrawal_updates_staking_before_receiver_callback_burn_and_swap() {
    let (mut fixture, mut cache, mut engine, asserter) = setup().await;
    fixture.seed_initialized_ticks(1_000_000_000_000);
    fixture.state.0.insert(
        (OP_POOL, U256::from(16)),
        U256::from(1_000_000_000_000_u64) | (U256::from(10_000_000_000_000_u64) << 128),
    );
    for ((address, slot), value) in &fixture.state.0 {
        cache.apply_updates(&[evm_fork_cache::StateUpdate::slot(*address, *slot, *value)]);
    }
    engine
        .ingest_batch(&mut cache, batch(staking_logs(true, 900_000_000_000), 100))
        .unwrap();
    let mut logs = staking_logs(false, 900_000_000_000);
    logs.last_mut().unwrap().0 = 7;
    logs.push((5, burn_log(OP_POOL, -200, 200, 800_000_000_000)));
    // A receiver callback burns liquidity and then swaps before gauge Withdraw.
    // Net input 1e9 with L=200e9 gives ceil(Q96 * 200 / 201), tick=-100.
    let price = (Q96 * U256::from(200) + U256::from(200)) / U256::from(201);
    logs.push((
        6,
        raw(
            OP_POOL,
            "Swap(address,address,int256,int256,uint160,uint128,int24)",
            vec![topic_address(OWNER), topic_address(OWNER)],
            vec![
                U256::from(1_010_101_011_u64),
                U256::MAX - U256::from(995_024_874_u64),
                price,
                U256::from(200_000_000_000_u64),
                U256::MAX - U256::from(99),
            ],
        ),
    ));
    logs.sort_by_key(|(index, _)| *index);
    let report = engine.ingest_batch(&mut cache, batch(logs, 101)).unwrap();
    assert!(report.degraded_pools.is_empty(), "{report:#?}");
    assert_eq!(
        cache.cached_storage_value(OP_POOL, U256::from(15)).unwrap() & WORD_128_MASK,
        U256::ZERO
    );
    assert_eq!(
        cache.cached_storage_value(OP_POOL, U256::from(16)).unwrap() & WORD_128_MASK,
        U256::from(200_000_000_000_u64)
    );
    assert_eq!(
        cache.cached_storage_value(OP_POOL, U256::from(6)).unwrap()
            & ((U256::from(1) << 160) - U256::from(1)),
        price
    );
    assert_eq!(report.resync_state_updates, 0);
    assert_eq!(asserter.read_q().len(), 1);
}

#[tokio::test]
async fn staking_evidence_cannot_be_reused_for_a_different_payload() {
    let (fixture, mut cache, engine, _) = setup().await;
    let source = batch(staking_logs(true, 400), 100);
    let instance = engine
        .ownership()
        .active_pool(&fixture.registration.key)
        .unwrap();
    let proofs = evm_amm_state::adapters::slipstream_staking::SlipstreamStakingBatch::from_batch(
        &source,
        instance,
        &cache.snapshot(),
    );
    let record = &source.records()[3];
    let block = record.context.block.unwrap();
    let mut context = AdapterEventContext::for_block(100, block.hash, block.timestamp.unwrap())
        .with_chain_id(10)
        .with_parent_hash(block.parent_hash.unwrap())
        .with_transaction_hash(TX_HASH)
        .with_event_order(0, 5);
    context.slipstream_staking_evidence = proofs.evidence(OP_POOL, &context);
    let altered = staking_logs(true, 399).pop().unwrap().1;
    let decoded = ConcentratedLiquidityAdapter::default().decode_event_with_context(
        &fixture.registration,
        &altered,
        &cache,
        &context,
    );
    assert!(
        decoded.error.is_some(),
        "evidence must bind the original payload"
    );
    assert_eq!(
        decoded.event.unwrap().quality,
        UpdateQuality::RequiresRepair
    );
}

#[tokio::test]
async fn incomplete_or_conflicting_staking_receipts_fail_closed() {
    for case in 0..7 {
        let (_, mut cache, mut engine, asserter) = setup().await;
        let mut logs = staking_logs(true, 400);
        match case {
            0 => {
                logs.remove(0);
            } // pool range is absent
            1 => {
                logs.remove(1);
            } // token/range association is absent
            2 => {
                logs.remove(2);
            } // custody transfer is absent
            3 => {
                logs[1].0 = 2;
            } // manager Collect is not adjacent to pool Collect
            4 => {
                logs[0].1 = collect_log(OP_POOL, -200, 200);
            } // wrong owner/recipient
            5 => {
                logs[1].1 = raw(
                    NFT,
                    "Collect(uint256,address,uint256,uint256)",
                    vec![B256::repeat_byte(8)],
                    vec![
                        U256::from_be_slice(OWNER.as_slice()),
                        U256::ZERO,
                        U256::ZERO,
                    ],
                );
            }
            6 => {
                logs[0].1 = raw(
                    OP_POOL,
                    "Collect(address,address,int24,int24,uint128,uint128)",
                    vec![topic_address(NFT), topic_i24(200), topic_i24(-200)],
                    vec![
                        U256::from_be_slice(OWNER.as_slice()),
                        U256::ZERO,
                        U256::ZERO,
                    ],
                );
            }
            _ => unreachable!(),
        }
        let report = engine.ingest_batch(&mut cache, batch(logs, 100)).unwrap();
        assert_eq!(
            report.degraded_pools,
            vec![PoolKey::Slipstream(OP_POOL)],
            "case {case}"
        );
        assert!(
            cache
                .cached_storage_value(OP_POOL, U256::from(15))
                .is_none(),
            "case {case}"
        );
        assert!(
            !report.decode_diagnostics().entries().is_empty(),
            "case {case}"
        );
        assert_eq!(asserter.read_q().len(), 1);
    }
}

#[tokio::test]
async fn unrelated_nft_transfers_do_not_create_staking_liquidity() {
    let (_, mut cache, mut engine, asserter) = setup().await;
    let transfer = staking_logs(true, 400).remove(2);
    let report = engine
        .ingest_batch(&mut cache, batch(vec![transfer], 100))
        .unwrap();
    assert!(report.degraded_pools.is_empty());
    assert_eq!(
        cache.cached_storage_value(OP_POOL, U256::from(15)).unwrap() & WORD_128_MASK,
        U256::ZERO
    );
    assert_eq!(asserter.read_q().len(), 1);
}

#[tokio::test]
async fn repeated_deposit_withdraw_cycles_preserve_packed_words_and_never_repair() {
    let (fixture, mut cache, mut engine, asserter) = setup().await;
    let marker = U256::from(73) << 128;
    cache.apply_updates(&[evm_fork_cache::StateUpdate::slot(
        OP_POOL,
        fixture.lower_keys[1],
        marker,
    )]);
    let samples: u64 = std::env::var("SLIPSTREAM_STAKING_SAMPLES")
        .ok()
        .map(|v| v.parse().unwrap())
        .unwrap_or(20);
    assert!((1..=10_000).contains(&samples));
    let background: u64 = std::env::var("SLIPSTREAM_STAKING_BACKGROUND_SLOTS")
        .ok()
        .map(|v| v.parse().unwrap())
        .unwrap_or(0);
    assert!(background <= 100_000);
    for slot in 0..background {
        cache.apply_updates(&[evm_fork_cache::StateUpdate::slot(
            Address::repeat_byte(0x77),
            U256::from(slot),
            U256::from(slot),
        )]);
    }
    let mut durations = Vec::new();
    for block in 100..100 + samples {
        let deposit = block % 2 == 0;
        let started = std::time::Instant::now();
        let report = engine
            .ingest_batch(&mut cache, batch(staking_logs(deposit, 400), block))
            .unwrap();
        durations.push(started.elapsed());
        assert!(report.degraded_pools.is_empty());
        assert_eq!(
            cache.cached_storage_value(OP_POOL, fixture.lower_keys[1]),
            Some(marker | U256::from(if deposit { 400 } else { 0 }))
        );
        assert_eq!(
            cache.cached_storage_value(OP_POOL, U256::from(15)).unwrap() & WORD_128_MASK,
            U256::from(if deposit { 400 } else { 0 })
        );
    }
    assert_eq!(asserter.read_q().len(), 1);
    durations.sort();
    let percentile = |p: usize| {
        durations[(durations.len() * p / 100).min(durations.len() - 1)].as_secs_f64() * 1000.0
    };
    println!(
        "staking batch samples={samples} background_slots={background} p50_ms={} p95_ms={} p99_ms={} max_ms={} RPC_elements=0 retries=0 fallbacks=0",
        percentile(50),
        percentile(95),
        percentile(99),
        percentile(100)
    );
    assert!(percentile(95) < 500.0);
    assert!(percentile(100) < 1000.0);
}

#[tokio::test]
async fn reorg_reverts_staking_and_replacement_rebuilds_its_own_evidence() {
    let (fixture, mut cache, mut engine, _) = setup().await;
    engine
        .ingest_batch(&mut cache, batch(staking_logs(true, 400), 100))
        .unwrap();
    let source = batch(staking_logs(true, 400), 100);
    let records = source
        .records()
        .iter()
        .cloned()
        .map(|mut record| {
            let block = record.context.block.unwrap();
            record.context.chain_status = ChainStatus::Reorged {
                dropped_from: block,
            };
            if let ReactiveInput::Log(log) = &mut record.input {
                log.removed = true;
            }
            record
        })
        .collect();
    engine
        .ingest_batch(&mut cache, ReactiveInputBatch::new(records))
        .unwrap();
    assert_eq!(
        cache.cached_storage_value(OP_POOL, U256::from(15)).unwrap() & WORD_128_MASK,
        U256::ZERO
    );
    assert_eq!(
        cache.cached_storage_value(OP_POOL, fixture.lower_keys[1]),
        Some(U256::ZERO)
    );
    let replacement = batch(staking_logs(true, 250), 100);
    let records = replacement
        .records()
        .iter()
        .cloned()
        .map(|mut record| {
            let mut block = record.context.block.unwrap();
            block.hash = B256::repeat_byte(0xd1);
            record.context.block = Some(block);
            record.context.chain_status = ChainStatus::Included {
                block,
                confirmations: 0,
            };
            if let ReactiveInput::Log(log) = &mut record.input {
                log.block_hash = Some(block.hash);
            }
            record
        })
        .collect();
    engine
        .ingest_batch(&mut cache, ReactiveInputBatch::new(records))
        .unwrap();
    assert_eq!(
        cache.cached_storage_value(OP_POOL, U256::from(15)).unwrap() & WORD_128_MASK,
        U256::from(250)
    );
}

#[tokio::test]
async fn one_custody_transfer_cannot_attest_two_deposits() {
    let (_, mut cache, mut engine, _) = setup().await;
    let mut logs = staking_logs(true, 400);
    logs.push((6, logs.last().unwrap().1.clone()));
    let report = engine.ingest_batch(&mut cache, batch(logs, 100)).unwrap();
    assert_eq!(report.degraded_pools, vec![PoolKey::Slipstream(OP_POOL)]);
    assert!(
        cache
            .cached_storage_value(OP_POOL, U256::from(15))
            .is_none()
    );
}

#[tokio::test]
async fn withdrawal_does_not_hide_an_invalid_parent_staking_balance() {
    let (_, mut cache, mut engine, _) = setup().await;
    engine
        .ingest_batch(&mut cache, batch(staking_logs(true, 400), 100))
        .unwrap();
    cache.apply_updates(&[evm_fork_cache::StateUpdate::slot(
        OP_POOL,
        U256::from(15),
        (U256::from(200) << 160) | U256::from(1_200),
    )]);
    let report = engine
        .ingest_batch(&mut cache, batch(staking_logs(false, 400), 101))
        .unwrap();
    assert_eq!(report.degraded_pools, vec![PoolKey::Slipstream(OP_POOL)]);
}

#[tokio::test]
async fn staking_interests_keep_one_provider_filter_for_the_pool() {
    let (_, _, engine, _) = setup().await;
    let mut routes = evm_fork_cache::reactive::ReactiveRegistry::new();
    routes
        .register_handler(Arc::new(evm_amm_state::adapters::AmmReactiveHandler::new(
            engine.registry().clone(),
        )))
        .unwrap();
    let filters = routes.log_subscription_filters();
    assert_eq!(
        filters.len(),
        1,
        "staking coverage must not triple canonical log queries"
    );
    let addresses: std::collections::BTreeSet<_> = filters[0].address.iter().copied().collect();
    assert_eq!(
        addresses,
        std::collections::BTreeSet::from([OP_POOL, GAUGE, NFT])
    );
}

#[tokio::test]
async fn noncanonical_staking_payloads_are_not_accepted_as_evidence() {
    for index in 0..4 {
        let (_, mut cache, mut engine, _) = setup().await;
        let mut logs = staking_logs(true, 400);
        let original = &logs[index].1;
        let mut data = original.data.data.to_vec();
        data.extend_from_slice(&[0; 32]);
        logs[index].1 =
            Log::new(original.address, original.topics().to_vec(), data.into()).unwrap();
        let report = engine.ingest_batch(&mut cache, batch(logs, 100)).unwrap();
        assert_eq!(
            report.degraded_pools,
            vec![PoolKey::Slipstream(OP_POOL)],
            "event {index}"
        );
    }
}
