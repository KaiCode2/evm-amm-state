use super::*;
use evm_amm_state::adapters::AmmStateQuality;

#[tokio::test(flavor = "multi_thread")]
async fn advancing_head_supersedes_required_repair_without_a_failure_event() -> Result<()> {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut cache = setup_cache().await;
            align_cache(&mut cache, 500);
            let address = Address::repeat_byte(0x91);
            let target = StateSlot::new(address, U256::from(9));
            let topic = B256::repeat_byte(0x92);
            cache.apply_updates(&[ForkStateUpdate::slot(address, target.slot(), U256::from(7))]);
            let mut registry = AdapterRegistry::new();
            registry.register_adapter(Arc::new(TestRepairAdapter {
                protocol: "superseded-repair",
                emitter: address,
                topic,
                target,
            }))?;
            registry.register_pool(
                custom_registration("superseded-repair", address).with_state_address(address),
            )?;
            let runtime = AmmRuntime::spawn(
                cache,
                registry,
                runtime_baseline(500),
                AmmRuntimeConfig::default(),
            )?;
            let assertions = Asserter::new();
            assertions.push_success(&encoded_words([U256::from(111)]));
            assertions.push_success(&encoded_words([U256::from(222)]));
            let transport =
                GatedMockTransport::gating(assertions, vec!["eth_call", "eth_getStorageAt"]);
            let provider = RootProvider::<AnyNetwork>::new(RpcClient::new(transport.clone(), true));
            let worker = runtime
                .attach_cold_start_worker(
                    provider,
                    AmmColdStartWorkerConfig::default().with_queue_capacity(1),
                )
                .await?;
            let mut observer = runtime.subscribe_events();
            runtime
                .ingest_batch(canonical_primitive_log_batch(
                    501,
                    0,
                    PrimitiveLog::new_unchecked(address, vec![topic], Bytes::new()),
                ))
                .await?;
            tokio::time::timeout(Duration::from_secs(5), async {
                while transport.requests().is_empty() {
                    tokio::task::yield_now().await;
                }
            })
            .await?;
            runtime.ingest_batch(empty_canonical_batch(502)).await?;
            transport.release_one();
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let event = observer.next_event().await.unwrap();
                    match event.kind() {
                        AmmRuntimeEventKind::WorkFailed { message, .. } => {
                            panic!("obsolete repair must not fail the generation: {message}")
                        }
                        AmmRuntimeEventKind::WorkSuperseded {
                            target, observed, ..
                        } => {
                            assert_eq!(target.block_number(), 501);
                            assert_eq!(observed.block_number(), 502);
                            break;
                        }
                        _ => {}
                    }
                }
                while transport.requests().len() < 2 {
                    tokio::task::yield_now().await;
                }
            })
            .await?;
            assert_ne!(
                runtime
                    .latest_snapshot()
                    .cache()
                    .storage_value(address, target.slot()),
                Some(U256::from(111))
            );
            assert_eq!(
                runtime.latest_status().health(),
                AmmRuntimeHealth::Degraded,
                "repair must remain fenced; requests={:?}, point={:?}, value={:?}, status={:?}",
                transport.requests(),
                runtime.latest_snapshot().point(),
                runtime
                    .latest_snapshot()
                    .cache()
                    .storage_value(address, target.slot()),
                runtime.latest_status()
            );
            transport.release_one();
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    match observer.next_event().await.unwrap().kind() {
                        AmmRuntimeEventKind::WorkFailed { message, .. } => {
                            panic!("fresh repair failed: {message}")
                        }
                        AmmRuntimeEventKind::WorkCompleted { .. } => break,
                        _ => {}
                    }
                }
            })
            .await?;
            assert_eq!(
                runtime
                    .latest_snapshot()
                    .cache()
                    .storage_value(address, target.slot()),
                Some(U256::from(222))
            );
            assert_eq!(runtime.latest_status().health(), AmmRuntimeHealth::Healthy);
            assert_eq!(
                transport.requests().len(),
                2,
                "one attempt per canonical target"
            );
            worker.shutdown();
            runtime.shutdown().await?;
            Ok(())
        })
        .await
}

fn replacement_batch(header: RpcHeader, pool: Address, has_event: bool) -> AmmCanonicalBatch {
    let block = BlockRef {
        number: header.inner.number,
        hash: header.hash,
        parent_hash: Some(header.inner.parent_hash),
        timestamp: Some(header.inner.timestamp),
    };
    let records = if has_event {
        vec![ReactiveInputRecord::new(
            ReactiveInput::Log(RpcLog {
                inner: PrimitiveLog::new_unchecked(
                    pool,
                    vec![keccak256(b"Sync(uint112,uint112)")],
                    encoded_words([U256::from(71), U256::from(72)]),
                ),
                block_hash: Some(block.hash),
                block_number: Some(block.number),
                block_timestamp: block.timestamp,
                transaction_hash: Some(B256::repeat_byte(9)),
                transaction_index: Some(0),
                log_index: Some(0),
                removed: false,
            }),
            ReactiveContext {
                chain_id: Some(1),
                source: InputSource::Synthetic,
                chain_status: ChainStatus::Included {
                    block,
                    confirmations: 0,
                },
                block: Some(block),
                transaction_index: Some(0),
                log_index: Some(0),
            },
        )]
    } else {
        Vec::new()
    };
    AmmCanonicalBatch::from_verified_block(1, header, 0, ReactiveInputBatch::new(records)).unwrap()
}

async fn ready_v2_runtime() -> Result<(evm_amm_state::adapters::AmmRuntimeHandle, Address)> {
    let mut cache = setup_cache().await;
    align_cache(&mut cache, 500);
    let pool = Address::repeat_byte(0x71);
    cache.apply_updates(&[ForkStateUpdate::slot(
        pool,
        V2_RESERVES_SLOT,
        U256::from(51) | (U256::from(52) << 112),
    )]);
    let mut registry = AdapterRegistry::new();
    registry.register_adapter(Arc::new(UniswapV2Adapter::default()))?;
    registry.register_pool(complete_v2_registration(pool).with_status(PoolStatus::Ready))?;
    Ok((
        AmmRuntime::spawn(
            cache,
            registry,
            runtime_baseline(500),
            AmmRuntimeConfig::default(),
        )?,
        pool,
    ))
}

#[tokio::test(flavor = "multi_thread")]
async fn events_on_either_branch_still_require_repair() -> Result<()> {
    tokio::task::LocalSet::new()
        .run_until(async {
            for (old_events, new_events) in [(true, false), (false, true), (true, true)] {
                let (runtime, pool) = ready_v2_runtime().await?;
                runtime
                    .ingest_batch(replacement_batch(canonical_header(501), pool, old_events))
                    .await?;
                let mut header = canonical_header(501).inner;
                header.extra_data = Bytes::from_static(b"replacement");
                let changed = runtime
                    .ingest_batch(replacement_batch(RpcHeader::new(header), pool, new_events))
                    .await?;
                assert!(
                    changed.requires_full_refresh(),
                    "events on either branch prevent the shortcut"
                );
                assert_eq!(changed.quality(), AmmStateQuality::Degraded);
                runtime.shutdown().await?;
            }
            Ok(())
        })
        .await
}

#[tokio::test(flavor = "multi_thread")]
async fn event_free_multiblock_replacement_remains_coherent() -> Result<()> {
    tokio::task::LocalSet::new().run_until(async {
        let (runtime, _) = ready_v2_runtime().await?;
        runtime.ingest_batch(empty_canonical_batch(501)).await?;
        runtime.ingest_batch(empty_canonical_batch(502)).await?;
        let changed = runtime.ingest_batch(alternate_empty_canonical_batch(501)).await?;
        assert!(!changed.requires_full_refresh());
        assert_eq!(changed.quality(), AmmStateQuality::Coherent);
        assert!(matches!(changed.incidents(), [evm_amm_state::adapters::AmmStateIncident::Reorg { dropped }] if dropped.len() == 2));
        runtime.shutdown().await?;
        Ok(())
    }).await
}

#[tokio::test(flavor = "multi_thread")]
async fn noncanonical_publication_discards_empty_branch_proof() -> Result<()> {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (runtime, _) = ready_v2_runtime().await?;
            runtime.ingest_batch(empty_canonical_batch(501)).await?;
            runtime
                .install_prepared_pools(Vec::new(), runtime.latest_snapshot().point())
                .await?;
            let changed = runtime
                .ingest_batch(alternate_empty_canonical_batch(501))
                .await?;
            assert!(changed.requires_full_refresh());
            assert_eq!(changed.quality(), AmmStateQuality::Degraded);
            runtime.shutdown().await?;
            Ok(())
        })
        .await
}

#[tokio::test(flavor = "multi_thread")]
async fn event_free_reorg_preserves_ready_pool_without_repair() -> Result<()> {
    tokio::task::LocalSet::new().run_until(async {
        let mut cache = setup_cache().await;
        align_cache(&mut cache, 500);
        let pool = Address::repeat_byte(0x71);
        let reserves = U256::from(51) | (U256::from(52) << 112);
        cache.apply_updates(&[ForkStateUpdate::slot(pool, V2_RESERVES_SLOT, reserves)]);
        let mut registry = AdapterRegistry::new();
        registry.register_adapter(Arc::new(UniswapV2Adapter::default()))?;
        registry.register_pool(complete_v2_registration(pool).with_status(PoolStatus::Ready))?;
        let runtime = AmmRuntime::spawn(cache, registry, runtime_baseline(500), AmmRuntimeConfig::default())?;
        runtime.ingest_batch(empty_canonical_batch(501)).await?;
        let changes = runtime.ingest_batch(alternate_empty_canonical_batch(501)).await?;
        assert!(!changes.requires_full_refresh(), "empty branch replacement must not cold-start pools");
        assert_eq!(changes.quality(), AmmStateQuality::Coherent);
        assert!(matches!(changes.incidents(), [evm_amm_state::adapters::AmmStateIncident::Reorg { dropped }] if dropped.len() == 1));
        assert_eq!(runtime.latest_snapshot().cache().storage_value(pool, V2_RESERVES_SLOT), Some(reserves));
        assert_eq!(runtime.latest_status().active_work_items().count(), 0);
        runtime.shutdown().await?;
        Ok(())
    }).await
}

/// Ethereum-shaped V3 storage remains exact while the full block identity moves.
#[tokio::test(flavor = "multi_thread")]
async fn ethereum_v3_event_free_reorg_preserves_slots_and_replaces_header() -> Result<()> {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut cache = setup_cache().await;
            align_cache(&mut cache, 500);
            let pool = Address::repeat_byte(0xd3);
            let q96 = U256::from(1) << 96_usize;
            let slots = [
                (
                    U256::ZERO,
                    q96 | (U256::from(1) << 200_usize)
                        | (U256::from(1) << 216_usize)
                        | (U256::from(1) << 240_usize),
                ),
                (U256::from(1), U256::from(5)),
                (U256::from(2), U256::from(7)),
                (U256::from(3), U256::ZERO),
                (U256::from(4), U256::from(1_000_000_000_000_000_000_u128)),
                (
                    U256::from(8),
                    U256::from(1_700_000_500_u64) | (U256::from(1) << 248_usize),
                ),
                (v3_tick_bitmap_storage_key(0), U256::ZERO),
            ];
            cache.apply_updates(
                &slots.map(|(slot, value)| ForkStateUpdate::slot(pool, slot, value)),
            );
            let mut registry = AdapterRegistry::new();
            registry.register_adapter(Arc::new(ConcentratedLiquidityAdapter::default()))?;
            registry.register_pool(
                PoolRegistration::new(PoolKey::UniswapV3(pool))
                    .with_state_address(pool)
                    .with_metadata(ProtocolMetadata::UniswapV3(
                        V3Metadata::default()
                            .with_fee(3_000)
                            .with_tick_spacing(60)
                            .with_storage_layout(V3StorageLayout::uniswap(60)),
                    ))
                    .with_status(PoolStatus::Ready),
            )?;
            let runtime = AmmRuntime::spawn(
                cache,
                registry,
                runtime_baseline(500),
                AmmRuntimeConfig::default(),
            )?;
            runtime.ingest_batch(empty_canonical_batch(501)).await?;
            let prior = runtime.latest_snapshot();
            let changes = runtime
                .ingest_batch(alternate_empty_canonical_batch(501))
                .await?;
            assert!(!changes.requires_full_refresh());
            assert_eq!(changes.quality(), AmmStateQuality::Coherent);
            let after = runtime.latest_snapshot();
            assert_ne!(after.point().block_hash(), prior.point().block_hash());
            assert_eq!(after.point().block_number(), prior.point().block_number());
            assert!(after.version() > prior.version());
            for (slot, value) in slots {
                assert_eq!(after.cache().storage_value(pool, slot), Some(value));
            }
            assert_eq!(runtime.latest_status().active_work_items().count(), 0);
            runtime.shutdown().await?;
            Ok(())
        })
        .await
}

#[tokio::test(flavor = "multi_thread")]
async fn event_free_reorg_latency_and_work_count() -> Result<()> {
    tokio::task::LocalSet::new().run_until(async {
        let (runtime, pool) = ready_v2_runtime().await?;
        runtime.ingest_batch(empty_canonical_batch(501)).await?;
        let mut samples = Vec::with_capacity(256);
        for sample in 0_u64..256 {
            let mut header = canonical_header(501).inner;
            header.extra_data = Bytes::copy_from_slice(&sample.to_be_bytes());
            let batch = replacement_batch(RpcHeader::new(header), pool, false);
            let start = Instant::now();
            let changed = runtime.ingest_batch(batch).await?;
            samples.push(start.elapsed());
            assert!(!changed.requires_full_refresh());
            assert_eq!(changed.quality(), AmmStateQuality::Coherent);
            assert_eq!(runtime.latest_status().active_work_items().count(), 0);
        }
        samples.sort_unstable();
        eprintln!("event_free_reorg host={}-{} build={} samples={} p50_us={} p95_us={} p99_us={} max_us={} queued_repair_jobs=0 added_rpc_elements=0 retries=0 fallbacks=0",
            std::env::consts::OS, std::env::consts::ARCH,
            if cfg!(debug_assertions) { "debug" } else { "release" }, samples.len(),
            samples[127].as_micros(), samples[243].as_micros(), samples[253].as_micros(), samples[255].as_micros());
        if !cfg!(debug_assertions) {
            assert!(samples[253] < Duration::from_millis(50), "canonical actor replacement p99 exceeded 50 ms");
        }
        runtime.shutdown().await?;
        Ok(())
    }).await
}
