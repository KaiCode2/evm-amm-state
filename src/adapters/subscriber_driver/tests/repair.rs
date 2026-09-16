use super::*;
use crate::adapters::{
    AdapterColdStartPlanner, AmmRuntimeHealth, ColdStartOutcome, ColdStartPlan, ColdStartPolicy,
    ColdStartReport, ColdStartResults, ColdStartRunReport, ColdStartStep, StateView,
    UnsupportedReason,
};
use std::sync::atomic::{AtomicUsize, Ordering};

struct ChangedSourceAdapter;

impl AmmAdapter for ChangedSourceAdapter {
    fn protocol(&self) -> ProtocolId {
        EmptyAdapter.protocol()
    }
    fn event_sources(&self, pool: &PoolRegistration) -> Vec<EventSource> {
        EmptyAdapter.event_sources(pool)
    }
    fn state_dependencies(&self, _: &PoolRegistration) -> PoolStateDependencies {
        PoolStateDependencies::default()
    }
    fn cold_start_planner(
        &self,
        _: &PoolRegistration,
        _: ColdStartPolicy,
    ) -> Result<Box<dyn AdapterColdStartPlanner>, UnsupportedReason> {
        Ok(Box::new(ChangedSourcePlanner))
    }
}

struct ChangedSourcePlanner;
impl AdapterColdStartPlanner for ChangedSourcePlanner {
    fn initial_plan(&mut self, _: &dyn StateView) -> ColdStartPlan {
        ColdStartPlan::default()
    }
    fn on_results(&mut self, _: &ColdStartResults, _: &dyn StateView) -> ColdStartStep {
        ColdStartStep::Done
    }
    fn finish(&mut self, pool: &mut PoolRegistration, _: &ColdStartRunReport) -> ColdStartOutcome {
        pool.status = PoolStatus::Ready;
        *pool = pool.clone().with_event_sources([EventSource::direct(
            pool.key.address().unwrap(),
            vec![B256::repeat_byte(0x52)],
        )]);
        let mut report = ColdStartReport::new(pool.key.clone(), ColdStartPolicy::Eager);
        report.status = PoolStatus::Ready;
        ColdStartOutcome::Ready(report)
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn same_height_subscriber_mismatch_waits_for_canonical_progress() -> Result<()> {
    tokio::task::LocalSet::new()
        .run_until(async {
            let baseline_header = header(500, B256::repeat_byte(0x49));
            let initial_header = header(501, baseline_header.hash);
            let repair_header = alternate_header(501, baseline_header.hash, b"replacement");
            let next_header = header(502, repair_header.hash);
            let mut cache = setup_cache().await;
            cache.advance_block(&baseline_header)?;
            let pool = registration(Address::repeat_byte(0x56));
            let mut registry = AdapterRegistry::new();
            registry.register_adapter(Arc::new(ChangedSourceAdapter))?;
            registry.register_pool(pool.clone())?;
            let runtime = AmmRuntime::spawn(
                cache,
                registry,
                AmmRuntimeBaseline::from_verified_header(1, baseline_header)?,
                AmmRuntimeConfig::default(),
            )?;
            let baseline = crate::adapters::AmmStatePoint::post_block(1, 501, repair_header.hash);
            let (commands, mut receiver) = mpsc::channel(8);
            let replace_attempts = Arc::new(AtomicUsize::new(0));
            let attempts = Arc::clone(&replace_attempts);
            let fake_driver = tokio::spawn(async move {
                while let Some(command) = receiver.recv().await {
                    match command {
                        SubscriberControlCommand::AdoptExisting { response, .. } => {
                            let _ = response.send(Ok(()));
                        }
                        SubscriberControlCommand::BeginAdd {
                            plans, response, ..
                        } => {
                            assert!(plans.is_empty());
                            let _ = response.send(Ok(SubscriberTransaction(2)));
                        }
                        SubscriberControlCommand::BeginReplace {
                            point, response, ..
                        } => {
                            if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                                assert_eq!(point, baseline);
                                let _ = response.send(Err(
                                    super::super::AmmSubscriberDriverError::Owner(Box::new(
                                        super::super::SubscriberOwnerError::BlockMismatch {
                                            expected_number: point.block_number(),
                                            expected_hash: point.block_hash(),
                                            actual_number: point.block_number(),
                                            actual_hash: B256::repeat_byte(0xff),
                                        },
                                    )),
                                ));
                            } else {
                                assert_eq!(point.block_number(), 502);
                                let _ = response.send(Ok(SubscriberTransaction(1)));
                            }
                        }
                        SubscriberControlCommand::Commit {
                            transaction,
                            interest_revision,
                            point,
                            response,
                        } => {
                            if transaction == SubscriberTransaction(1) {
                                assert_eq!(interest_revision, 1);
                                assert_eq!(point.block_number(), 502);
                            } else {
                                assert_eq!(transaction, SubscriberTransaction(2));
                                assert_eq!(interest_revision, 0);
                            }
                            let _ = response.send(Ok(()));
                        }
                        SubscriberControlCommand::Shutdown { response, .. } => {
                            let _ = response.send(Ok(()));
                            break;
                        }
                        _ => panic!("unexpected fake-driver command"),
                    }
                }
            });
            runtime
                .attach_subscriber_control(AmmSubscriberControl { commands })
                .await?;
            let provider = RootProvider::<AnyNetwork>::new(RpcClient::mocked(Asserter::new()));
            let worker = runtime
                .attach_cold_start_worker(provider, AmmColdStartWorkerConfig::default())
                .await?;
            let mut events = runtime.subscribe_events();
            runtime
                .ingest_subscriber_batch(AmmCanonicalBatch::from_verified_block(
                    1,
                    initial_header,
                    0,
                    ReactiveInputBatch::<Ethereum>::new(Vec::new()),
                )?)
                .await?;
            // An imported-state anchor makes this otherwise empty replacement
            // require repair, so this exercises the genuine fenced worker path.
            runtime
                .install_prepared_pools(Vec::new(), runtime.latest_snapshot().point())
                .await?;
            runtime
                .ingest_subscriber_batch(AmmCanonicalBatch::from_verified_block(
                    1,
                    repair_header,
                    0,
                    ReactiveInputBatch::<Ethereum>::new(Vec::new()),
                )?)
                .await?;
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    match events.next_event().await.unwrap().kind() {
                        AmmRuntimeEventKind::WorkSuperseded {
                            target, observed, ..
                        } => {
                            assert_eq!(*target, baseline);
                            assert_eq!(observed.block_hash(), B256::repeat_byte(0xff));
                            break;
                        }
                        AmmRuntimeEventKind::WorkFailed { message, .. } => {
                            panic!("repair failed: {message}")
                        }
                        _ => {}
                    }
                }
            })
            .await?;
            // Service more actor commands at the same point. None may retry or adopt
            // the provider's conflicting hash. The old registration remains fenced.
            for _ in 0..3 {
                runtime.install_prepared_pools(Vec::new(), baseline).await?;
            }
            assert_eq!(replace_attempts.load(Ordering::SeqCst), 1);
            assert_eq!(runtime.latest_snapshot().point(), baseline);
            assert_eq!(runtime.latest_status().health(), AmmRuntimeHealth::Degraded);
            assert_eq!(runtime.latest_status().active_work_items().count(), 0);
            assert_eq!(runtime.interest_revision(), 0);
            runtime
                .ingest_subscriber_batch(AmmCanonicalBatch::from_verified_block(
                    1,
                    next_header,
                    0,
                    ReactiveInputBatch::<Ethereum>::new(Vec::new()),
                )?)
                .await?;
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    match events.next_event().await.unwrap().kind() {
                        AmmRuntimeEventKind::WorkCompleted { .. } => break,
                        AmmRuntimeEventKind::WorkFailed { message, .. } => {
                            panic!("fresh repair failed: {message}")
                        }
                        _ => {}
                    }
                }
            })
            .await?;
            assert_eq!(replace_attempts.load(Ordering::SeqCst), 2);
            assert_eq!(runtime.latest_status().health(), AmmRuntimeHealth::Healthy);
            assert_eq!(runtime.interest_revision(), 1);
            worker.shutdown();
            runtime.shutdown().await?;
            fake_driver.await?;
            Ok(())
        })
        .await
}
