use alloy_primitives::{Address, B256};
use evm_amm_state::adapters::{
    AdapterGeneration, AdapterInstanceId, AdapterKey, AmmChangeImpact, AmmChangeSet,
    AmmChangeSetError, AmmPoolChange, AmmPoolChangeKind, AmmRuntimeEvent, AmmRuntimeEventKind,
    AmmRuntimeHealth, AmmRuntimeStatusSnapshot, AmmStateIncident, AmmStatePoint, AmmStateQuality,
    AmmStateVersion, AmmWorkClass, AmmWorkKind, AmmWorkProgress, DiscoveryGeneration,
    DiscoveryOwnerId, DiscoveryOwnerKey, InvalidPoolRuntimeTransition, InvalidWorkProgress,
    OwnerRuntimeState, PoolGeneration, PoolInstanceId, PoolKey, PoolLifecycle, PoolRuntimeState,
    PoolStateRef, PoolStateRevision, PoolStatus, ProtocolId, QueryEvidencePolicy, QueueDepths,
    RegistrationEvidenceSet, RegistrationProvenance, RegistrationReorgAction,
    RegistrationSourceKey, RuntimeLifecycleMap, RuntimeOwnerId, RuntimeSequenceOverflow,
    RuntimeWorkId, StatePosition, WorkId,
};

#[test]
fn pool_instance_generation_changes_without_reusing_logical_identity() {
    let key = PoolKey::UniswapV2(Address::repeat_byte(0x11));
    let first_generation = PoolGeneration::new(7);
    let next_generation = first_generation.checked_next().expect("generation 8");

    let first = PoolInstanceId::new(key.clone(), first_generation);
    let replacement = PoolInstanceId::new(key.clone(), next_generation);

    assert_eq!(first.key(), &key);
    assert_eq!(first.generation().get(), 7);
    assert_eq!(replacement.key(), &key);
    assert_eq!(replacement.generation().get(), 8);
    assert_ne!(first, replacement);
    assert_eq!(
        PoolGeneration::new(u64::MAX).checked_next(),
        Err(RuntimeSequenceOverflow::new("PoolGeneration"))
    );
}

#[test]
fn status_snapshot_recovers_unknown_progress_and_events_advance_in_order() {
    let pool = PoolInstanceId::new(
        PoolKey::UniswapV2(Address::repeat_byte(0x44)),
        PoolGeneration::new(6),
    );
    let mut lifecycles = RuntimeLifecycleMap::default();
    lifecycles.set_pool(pool.clone(), PoolRuntimeState::CatchingUp);

    let work = RuntimeWorkId::new(RuntimeOwnerId::Pool(pool.clone()), WorkId::new(20));
    let progress = AmmWorkProgress::new(AmmWorkKind::ColdStart, 4, None).unwrap();
    let mut queues = QueueDepths::default();
    queues.set(AmmWorkClass::Bootstrap, 3);

    let status = AmmRuntimeStatusSnapshot::new(
        40,
        AmmStateVersion::new(9),
        lifecycles,
        [(work.clone(), progress.clone())],
        queues,
        AmmRuntimeHealth::Healthy,
    );
    assert_eq!(status.pool_state(&pool), Some(PoolRuntimeState::CatchingUp));
    assert_eq!(status.active_work(&work).unwrap().total(), None);
    assert_eq!(status.queue_depth(AmmWorkClass::Bootstrap), 3);

    let progress_event = AmmRuntimeEvent::new(
        40,
        AmmRuntimeEventKind::WorkProgress {
            work: work.clone(),
            progress,
        },
    );
    let committed = progress_event
        .checked_next(AmmRuntimeEventKind::StateCommitted {
            version: AmmStateVersion::new(10),
            point: AmmStatePoint::post_block(1, 600, B256::repeat_byte(0x60)),
        })
        .expect("sequence 41");
    assert_eq!(committed.sequence(), 41);
    assert!(matches!(
        committed.kind(),
        AmmRuntimeEventKind::StateCommitted { version, .. } if version.get() == 10
    ));
}

#[test]
fn known_work_progress_rejects_completed_units_beyond_total() {
    assert_eq!(
        AmmWorkProgress::new(AmmWorkKind::ColdStart, 5, Some(4)),
        Err(InvalidWorkProgress::new(5, 4))
    );
    assert_eq!(
        AmmWorkProgress::new(AmmWorkKind::Discovery, 5, None)
            .unwrap()
            .total(),
        None,
        "unknown totals remain explicit and do not invent a percentage"
    );
}

#[test]
fn observer_events_keep_registration_and_chain_incidents_typed() {
    let pool = PoolInstanceId::new(
        PoolKey::UniswapV2(Address::repeat_byte(0x45)),
        PoolGeneration::new(2),
    );
    let registered = AmmRuntimeEvent::new(
        70,
        AmmRuntimeEventKind::RegistrationAccepted { pool: pool.clone() },
    );
    let gap = registered
        .checked_next(AmmRuntimeEventKind::Gap { from: 701, to: 703 })
        .unwrap();
    let removed = gap
        .checked_next(AmmRuntimeEventKind::RegistrationRemoved { pool })
        .unwrap();

    assert_eq!(registered.sequence(), 70);
    assert_eq!(gap.sequence(), 71);
    assert_eq!(removed.sequence(), 72);
    assert!(matches!(
        gap.kind(),
        AmmRuntimeEventKind::Gap { from: 701, to: 703 }
    ));
    assert_eq!(
        AmmRuntimeEvent::new(u64::MAX, AmmRuntimeEventKind::Gap { from: 1, to: 1 },)
            .checked_next(AmmRuntimeEventKind::Gap { from: 2, to: 2 }),
        Err(RuntimeSequenceOverflow::new("AmmRuntimeEvent.sequence"))
    );
}

#[test]
fn committed_change_set_has_one_canonically_ordered_change_per_pool() {
    let point = AmmStatePoint::post_block(1, 500, B256::repeat_byte(0x50));
    let pool_a = PoolInstanceId::new(
        PoolKey::UniswapV2(Address::repeat_byte(0x01)),
        PoolGeneration::new(1),
    );
    let pool_b = PoolInstanceId::new(
        PoolKey::UniswapV2(Address::repeat_byte(0x02)),
        PoolGeneration::new(1),
    );
    let change_a = AmmPoolChange::new(
        pool_a.clone(),
        PoolStateRevision::new(4),
        AmmPoolChangeKind::Updated,
        AmmChangeImpact::state_only(),
    );
    let change_b = AmmPoolChange::new(
        pool_b.clone(),
        PoolStateRevision::new(8),
        AmmPoolChangeKind::Degraded,
        AmmChangeImpact::quoteability(),
    );

    let gap = AmmStateIncident::Gap { from: 490, to: 491 };
    let coverage = AmmStateIncident::CoverageGap {
        address: Address::repeat_byte(0xaa),
        block: 492,
    };
    let dropped_499 = AmmStatePoint::post_block(1, 499, B256::repeat_byte(0x49));
    let dropped_500 = AmmStatePoint::post_block(1, 500, B256::repeat_byte(0x50));
    let reorg = AmmStateIncident::Reorg {
        dropped: vec![dropped_500, dropped_499, dropped_500],
    };
    let changes = AmmChangeSet::new(
        AmmStateVersion::new(12),
        point,
        AmmStateQuality::Degraded,
        [change_b.clone(), change_a.clone()],
        [coverage.clone(), gap.clone(), reorg.clone(), gap.clone()],
        false,
    )
    .expect("unique changes");

    assert_eq!(changes.version().get(), 12);
    assert_eq!(changes.point(), point);
    assert_eq!(
        changes.pool_changes(),
        &[change_a.clone(), change_b.clone()]
    );
    assert_eq!(
        changes.incidents(),
        &[
            AmmStateIncident::Reorg {
                dropped: vec![dropped_499, dropped_500],
            },
            gap,
            coverage,
        ],
        "incidents and dropped state points are canonical regardless of caller order"
    );

    assert_eq!(
        AmmChangeSet::new(
            AmmStateVersion::new(12),
            point,
            AmmStateQuality::Coherent,
            [change_a.clone(), change_a],
            [],
            false,
        ),
        Err(AmmChangeSetError::duplicate_pool(pool_a))
    );
}

#[test]
fn registration_reorg_action_respects_all_remaining_provenance_evidence() {
    let dropped_hash = B256::repeat_byte(0xd0);
    let owner = DiscoveryOwnerId::new(
        DiscoveryOwnerKey::new("ethereum.uniswap-v3.factory"),
        DiscoveryGeneration::new(2),
    );
    let orphaned_factory_log = RegistrationProvenance::factory_log(
        owner.clone(),
        Address::repeat_byte(0xfa),
        1,
        1_000,
        dropped_hash,
        B256::repeat_byte(0xaa),
        7,
    );

    let factory_only =
        RegistrationEvidenceSet::new(orphaned_factory_log.clone(), std::iter::empty());
    assert_eq!(
        factory_only.reorg_action(&[dropped_hash]),
        RegistrationReorgAction::Remove
    );

    let query_needing_revalidation = RegistrationProvenance::state_query(
        owner,
        1,
        1_000,
        dropped_hash,
        QueryEvidencePolicy::RevalidateOnReorg,
    );
    let query_backed =
        RegistrationEvidenceSet::new(orphaned_factory_log.clone(), [query_needing_revalidation]);
    assert_eq!(
        query_backed.reorg_action(&[dropped_hash]),
        RegistrationReorgAction::Revalidate
    );

    let stable = RegistrationProvenance::stable(RegistrationSourceKey::new("config.mainnet"));
    let stable_backed = RegistrationEvidenceSet::new(orphaned_factory_log, [stable.clone()]);
    assert_eq!(
        stable_backed.reorg_action(&[dropped_hash]),
        RegistrationReorgAction::Keep
    );

    let canonical_a = RegistrationEvidenceSet::new(
        RegistrationProvenance::stable(RegistrationSourceKey::new("config.secondary")),
        [stable.clone(), stable.clone()],
    );
    let canonical_b = RegistrationEvidenceSet::new(
        stable,
        [RegistrationProvenance::stable(RegistrationSourceKey::new(
            "config.secondary",
        ))],
    );
    assert_eq!(
        canonical_a, canonical_b,
        "evidence is an ordered set, not an insertion log"
    );
}

#[test]
fn discovery_owner_lifecycle_has_typed_observer_events() {
    let owner = DiscoveryOwnerId::new(
        DiscoveryOwnerKey::new("ethereum.uniswap-v2.factory"),
        DiscoveryGeneration::new(8),
    );
    let accepted = AmmRuntimeEventKind::DiscoveryRegistrationAccepted {
        owner: owner.clone(),
    };
    let removing = AmmRuntimeEventKind::DiscoveryLifecycleTransition {
        owner: owner.clone(),
        from: OwnerRuntimeState::Active,
        to: OwnerRuntimeState::Removing,
    };
    let removed = AmmRuntimeEventKind::DiscoveryRegistrationRemoved { owner };

    assert_ne!(accepted, removing);
    assert_ne!(removing, removed);
}

#[test]
fn pool_runtime_lifecycle_is_a_checked_sidecar_to_pool_status() {
    let pool = PoolInstanceId::new(
        PoolKey::UniswapV2(Address::repeat_byte(0x33)),
        PoolGeneration::new(1),
    );
    let mut lifecycle = PoolLifecycle::new(pool, PoolRuntimeState::Discovered);

    for next in [
        PoolRuntimeState::Queued,
        PoolRuntimeState::Hydrating,
        PoolRuntimeState::CatchingUp,
        PoolRuntimeState::Searchable,
        PoolRuntimeState::Live,
        PoolRuntimeState::Degraded,
        PoolRuntimeState::CatchingUp,
        PoolRuntimeState::Searchable,
        PoolRuntimeState::Live,
    ] {
        lifecycle.transition_to(next).expect("valid lifecycle edge");
    }

    assert_eq!(lifecycle.state(), PoolRuntimeState::Live);
    assert_eq!(
        lifecycle.state().required_pool_status(),
        Some(PoolStatus::Ready)
    );

    let error = lifecycle
        .transition_to(PoolRuntimeState::Hydrating)
        .expect_err("live state cannot restart hydration directly");
    assert_eq!(
        error,
        InvalidPoolRuntimeTransition::new(PoolRuntimeState::Live, PoolRuntimeState::Hydrating)
    );
    assert_eq!(lifecycle.state(), PoolRuntimeState::Live);

    lifecycle.transition_to(PoolRuntimeState::Removing).unwrap();
    lifecycle.transition_to(PoolRuntimeState::Removed).unwrap();
    assert!(
        lifecycle.transition_to(PoolRuntimeState::Queued).is_err(),
        "removed is terminal"
    );
}

#[test]
fn post_block_state_provenance_fences_quotes_and_starts_catch_up_at_the_next_block() {
    let pool = PoolInstanceId::new(
        PoolKey::UniswapV2(Address::repeat_byte(0x22)),
        PoolGeneration::new(3),
    );
    let point_100 = AmmStatePoint::post_block(1, 100, B256::repeat_byte(0x64));
    let point_101 = AmmStatePoint::post_block(1, 101, B256::repeat_byte(0x65));
    let revision = PoolStateRevision::new(9);

    assert_eq!(point_100.position(), StatePosition::PostBlock);
    assert_eq!(point_100.first_unapplied_block().expect("block 101"), 101);
    assert_eq!(AmmStateVersion::initial().get(), 0);
    assert_eq!(AmmStateVersion::initial().checked_next().unwrap().get(), 1);
    assert_eq!(
        AmmStateVersion::new(u64::MAX).checked_next(),
        Err(RuntimeSequenceOverflow::new("AmmStateVersion"))
    );
    assert_eq!(
        PoolStateRevision::new(u64::MAX).checked_next(),
        Err(RuntimeSequenceOverflow::new("PoolStateRevision"))
    );

    let at_100 = PoolStateRef::new(pool.clone(), revision, point_100);
    let at_101 = PoolStateRef::new(pool, revision, point_101);
    assert_ne!(
        at_100, at_101,
        "the block point is part of quote provenance"
    );
    assert_eq!(at_100.revision().get(), 9);

    assert_eq!(
        AmmStatePoint::post_block(1, u64::MAX, B256::ZERO).first_unapplied_block(),
        Err(RuntimeSequenceOverflow::new("AmmStatePoint.block_number"))
    );
}

#[test]
fn owner_and_work_generations_fence_replacements_and_retries_independently() {
    let adapter = AdapterInstanceId::new(
        AdapterKey::new(
            ProtocolId::UniswapV3,
            [
                ProtocolId::Slipstream,
                ProtocolId::PancakeV3,
                ProtocolId::UniswapV3,
            ],
        ),
        AdapterGeneration::new(2),
    );
    let discovery = DiscoveryOwnerId::new(
        DiscoveryOwnerKey::new("ethereum.uniswap-v3.factory"),
        DiscoveryGeneration::new(5),
    );

    let adapter_owner = RuntimeOwnerId::Adapter(adapter.clone());
    let discovery_owner = RuntimeOwnerId::Discovery(discovery.clone());
    assert_ne!(adapter_owner, discovery_owner);
    assert_eq!(
        adapter.key().protocols(),
        &[
            ProtocolId::UniswapV3,
            ProtocolId::PancakeV3,
            ProtocolId::Slipstream,
        ]
    );
    assert_eq!(
        adapter.key(),
        &AdapterKey::new(
            ProtocolId::Slipstream,
            [ProtocolId::PancakeV3, ProtocolId::UniswapV3],
        ),
        "adapter-family identity is canonical, not primary-protocol identity"
    );
    assert_eq!(discovery.key().as_str(), "ethereum.uniswap-v3.factory");

    let first_attempt = WorkId::new(41);
    let retry = first_attempt.checked_next().expect("work id 42");
    assert_eq!(retry.get(), 42);
    assert_ne!(first_attempt, retry);
    assert_eq!(
        WorkId::new(u64::MAX).checked_next(),
        Err(RuntimeSequenceOverflow::new("WorkId"))
    );
    assert_eq!(
        AdapterGeneration::new(u64::MAX).checked_next(),
        Err(RuntimeSequenceOverflow::new("AdapterGeneration"))
    );
    assert_eq!(
        DiscoveryGeneration::new(u64::MAX).checked_next(),
        Err(RuntimeSequenceOverflow::new("DiscoveryGeneration"))
    );
}
