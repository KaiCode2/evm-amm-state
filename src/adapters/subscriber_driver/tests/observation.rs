//! Drive the actual reconciliation/publication path with diagnostic comparison.
use super::*;
use crate::adapters::subscriber_driver::observation::Ledger;

#[tokio::test(flavor = "multi_thread")]
async fn comparison_reuses_exact_fetch_without_changing_publication_or_request_count() -> Result<()>
{
    tokio::task::LocalSet::new()
        .run_until(async {
            for enabled in [false, true] {
                let baseline = header(500, B256::repeat_byte(0x49));
                let delivered = header_admitting(501, baseline.hash, Address::repeat_byte(0x77));
                let asserter = Asserter::new();
                let (mut driver, runtime, stats) =
                    subscription_driver(asserter.clone(), baseline).await?;
                if enabled {
                    driver.observation = Some(Ledger::default());
                    stats.observation.set_enabled(true);
                }
                let logs: Vec<_> = (0..2)
                    .map(|index| {
                        reconciled_log(
                            Address::repeat_byte(0x77),
                            keccak256(b"Swap()"),
                            &delivered,
                            index,
                        )
                    })
                    .collect();
                asserter.push_success(&logs);
                let block = BlockRef {
                    number: 501,
                    hash: delivered.hash,
                    parent_hash: Some(delivered.inner.parent_hash),
                    timestamp: Some(delivered.inner.timestamp),
                };
                // Same helper called by handle_batch, with explicit subscription
                // provenance; only first of two logs was consumed at the cutoff.
                if let Some(ledger) = driver.observation.as_mut() {
                    let context = ReactiveContext {
                        chain_id: Some(1),
                        source: InputSource::Subscription,
                        chain_status: ChainStatus::Included {
                            block,
                            confirmations: 0,
                        },
                        block: Some(block),
                        transaction_index: None,
                        log_index: None,
                    };
                    ledger.observe(&ReactiveInputRecord::new(
                        ReactiveInput::BlockHeader(delivered.clone()),
                        context.clone(),
                    ));
                    ledger.observe(&ReactiveInputRecord::new(
                        ReactiveInput::Log(logs[0].clone()),
                        context,
                    ));
                }
                driver.buffer_subscription_log(logs[0].clone())?;
                let _commits = runtime.subscribe_changes().await?;
                driver.reconcile_and_deliver(delivered.clone()).await?;
                assert_eq!(
                    runtime.latest_snapshot().point().block_hash(),
                    delivered.hash
                );
                assert_eq!(stats.snapshot().reconciliation_requests(), 1);
                assert_eq!(stats.snapshot().reconciliation_logs_fetched(), 2);
                assert_eq!(stats.snapshot().canonical_blocks_delivered(), 1);
                assert!(asserter.read_q().is_empty());
                let observed = stats.snapshot().reconciliation_observation();
                if enabled {
                    let observed = observed.unwrap();
                    assert_eq!(
                        (
                            observed.missing_blocks(),
                            observed.reference_logs(),
                            observed.consumed_logs(),
                            observed.missing_logs()
                        ),
                        (1, 2, 1, 1)
                    );
                } else {
                    assert!(observed.is_none());
                }
                runtime.shutdown().await?;
            }
            Ok(())
        })
        .await
}

#[tokio::test(flavor = "multi_thread")]
async fn reorg_resets_observations_and_false_header_hash_still_fails_closed() -> Result<()> {
    tokio::task::LocalSet::new()
        .run_until(async {
            let baseline = header(500, B256::repeat_byte(0x49));
            let delivered = header_admitting(501, baseline.hash, Address::repeat_byte(0x77));
            let asserter = Asserter::new();
            let (mut driver, runtime, stats) =
                subscription_driver(asserter.clone(), baseline).await?;
            driver.observation = Some(Ledger::default());
            stats.observation.set_enabled(true);
            driver.reset_subscription_assembly();
            asserter.push_success(&Vec::<RpcLog>::new());
            let mut false_header = delivered;
            false_header.inner.logs_bloom = Default::default();
            let old = runtime.latest_snapshot().point();
            assert!(matches!(
                driver.reconcile_and_deliver(false_header).await,
                Err(super::super::AmmSubscriberDriverError::Canonical(_))
            ));
            assert_eq!(runtime.latest_snapshot().point(), old);
            assert_eq!(stats.snapshot().reconciliation_requests(), 1);
            assert_eq!(stats.snapshot().canonical_blocks_delivered(), 0);
            assert_eq!(
                stats
                    .snapshot()
                    .reconciliation_observation()
                    .unwrap()
                    .unavailable_blocks(),
                1
            );
            runtime.shutdown().await?;
            Ok(())
        })
        .await
}

#[tokio::test(flavor = "multi_thread")]
async fn provider_failure_is_unavailable_without_publication_or_second_request() -> Result<()> {
    tokio::task::LocalSet::new()
        .run_until(async {
            let baseline = header(500, B256::repeat_byte(0x49));
            let delivered = header_admitting(501, baseline.hash, Address::repeat_byte(0x77));
            let asserter = Asserter::new();
            let (mut driver, runtime, stats) =
                subscription_driver(asserter.clone(), baseline).await?;
            driver.observation = Some(Ledger::default());
            stats.observation.set_enabled(true);
            asserter.push_failure_msg("fixture reconciliation failure");
            let old = runtime.latest_snapshot().point();
            assert!(matches!(
                driver.reconcile_and_deliver(delivered).await,
                Err(super::super::AmmSubscriberDriverError::Provider(_))
            ));
            assert_eq!(runtime.latest_snapshot().point(), old);
            assert_eq!(stats.snapshot().reconciliation_requests(), 1);
            let observed = stats.snapshot().reconciliation_observation().unwrap();
            assert_eq!(
                (
                    observed.unavailable_blocks(),
                    observed.missing_blocks(),
                    observed.missing_logs()
                ),
                (1, 0, 0)
            );
            assert!(asserter.read_q().is_empty());
            runtime.shutdown().await?;
            Ok(())
        })
        .await
}
