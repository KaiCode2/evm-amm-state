use super::*;
use evm_fork_cache::reactive::SubscriberGapPolicy;

mod transport;

#[tokio::test(flavor = "multi_thread")]
async fn event_only_actual_pubsub_loop_holds_then_closes_without_outbound_requests() -> Result<()> {
    actual_pubsub_loop(false).await
}

#[tokio::test(flavor = "multi_thread")]
async fn event_only_actual_pubsub_disconnect_does_not_reconnect_or_replay() -> Result<()> {
    actual_pubsub_loop(true).await
}

async fn actual_pubsub_loop(disconnect: bool) -> Result<()> {
    tokio::task::LocalSet::new()
        .run_until(async {
            let baseline = header(500, B256::repeat_byte(0x49));
            let mut cache = setup_cache().await;
            cache.advance_block(&baseline)?;
            let runtime = AmmRuntime::spawn(
                cache,
                AdapterRegistry::new(),
                AmmRuntimeBaseline::from_verified_header(1, baseline.clone())?,
                AmmRuntimeConfig::default(),
            )?;
            let server = transport::Server::new();
            let provider = ProviderBuilder::new()
                .connect_pubsub_with(server.clone())
                .await?;
            let mut config = SubscriberConfig::default();
            config.reconnect.enabled = false;
            let mut subscriber = AlloySubscriber::new(provider, SubscriberMode::PubSub, config)
                .with_gap_policy(SubscriberGapPolicy::FailClosed);
            subscriber
                .register_interests(&[
                    ReactiveInterest::Logs(LogInterest {
                        provider_filter: Filter::new()
                            .address(Address::repeat_byte(0x77))
                            .event_signature(B256::repeat_byte(0x88)),
                        local_matcher: None,
                        route_key: None,
                    }),
                    ReactiveInterest::Logs(LogInterest {
                        provider_filter: Filter::new()
                            .address(Address::repeat_byte(0x78))
                            .event_signature(B256::repeat_byte(0x99)),
                        local_matcher: None,
                        route_key: None,
                    }),
                ])
                .await?;
            let handle = runtime
                .attach_alloy_subscriber(
                    subscriber,
                    AmmSubscriberDriverConfig::default()
                        .with_canonical_log_source(CanonicalLogSource::SubscriptionOnly),
                )
                .await?;
            server.wait_subscribed().await?;
            let cold_requests = server.requests();
            assert_eq!(
                cold_requests
                    .iter()
                    .filter(|method| *method == "eth_subscribe")
                    .count(),
                2
            );
            let empty = header(501, baseline.hash);
            server.header(&empty)?;
            wait_point(&runtime, 501).await?;
            let busy = header_admitting(502, empty.hash, Address::repeat_byte(0x77));
            server.header(&busy)?;
            server.log(&reconciled_log(
                Address::repeat_byte(0x77),
                B256::repeat_byte(0x88),
                &busy,
                0,
            ))?;
            let quiet = header(503, busy.hash);
            server.header(&quiet)?;
            tokio::time::sleep(Duration::from_millis(100)).await;
            assert_eq!(runtime.latest_snapshot().point().block_number(), 501);
            assert!(matches!(
                handle.latest_state(),
                AmmSubscriberDriverState::WaitingForClosure {
                    pending_block: 502,
                    ..
                }
            ));
            assert_eq!(
                server.requests(),
                cold_requests,
                "elapsed grace/new head must not fetch"
            );
            let successor = header_admitting(504, quiet.hash, Address::repeat_byte(0x77));
            server.header(&successor)?;
            server.log(&reconciled_log(
                Address::repeat_byte(0x77),
                B256::repeat_byte(0x88),
                &successor,
                0,
            ))?;
            wait_point(&runtime, 503).await?;
            tokio::time::sleep(Duration::from_millis(100)).await;
            assert_eq!(server.requests(), cold_requests);
            assert_eq!(handle.stats().canonical_blocks_delivered(), 3);
            assert_eq!(handle.stats().reconciliation_requests(), 0);
            assert_eq!(handle.stats().lineage_parent_requests(), 0);
            // A real malformed unsolicited log reaches the loss-detecting cache
            // adapter, which must terminate before any automatic getLogs repair.
            if disconnect {
                server.disconnect()?;
            } else {
                server.malformed_log()?;
            }
            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    if matches!(handle.latest_state(), AmmSubscriberDriverState::Failed(_)) {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await?;
            tokio::time::sleep(Duration::from_millis(100)).await;
            assert_eq!(server.requests(), cold_requests);
            assert_eq!(server.blocked_reconnects(), usize::from(disconnect));
            assert_eq!(runtime.latest_snapshot().point().block_number(), 503);
            runtime.shutdown().await?;
            Ok(())
        })
        .await
}

async fn wait_point(runtime: &AmmRuntimeHandle, number: u64) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(2), async {
        while runtime.latest_snapshot().point().block_number() != number {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn event_only_rejects_reorg_late_log_stale_headers_and_implicit_catchup() -> Result<()> {
    tokio::task::LocalSet::new()
        .run_until(async {
            let baseline = header(500, B256::repeat_byte(0x49));
            let asserter = Asserter::new();
            let (mut driver, runtime, stats) =
                subscription_driver(asserter.clone(), baseline.clone()).await?;
            driver.canonical_log_source = CanonicalLogSource::SubscriptionOnly;
            asserter.push_failure_msg("must not consume sentinel through any read");
            assert!(
                driver
                    .accept_event_only_header(&header(502, baseline.hash))
                    .is_err()
            );
            assert!(
                driver
                    .accept_event_only_header(&alternate_header(
                        500,
                        baseline.inner.parent_hash,
                        b"reorg"
                    ))
                    .is_err()
            );
            assert!(
                driver
                    .buffer_subscription_log(reconciled_log(
                        Address::repeat_byte(0x77),
                        B256::ZERO,
                        &baseline,
                        0
                    ))
                    .is_err()
            );
            assert!(
                driver
                    .deliver_through(header(501, baseline.hash))
                    .await
                    .is_err()
            );
            assert!(driver.require_automatic_source_mutation_allowed().is_err());
            let pending = header_admitting(501, baseline.hash, Address::repeat_byte(0x77));
            driver
                .pending_headers
                .insert(501, (pending, Instant::now() - Duration::from_secs(60)));
            assert!(driver.next_seal_deadline().is_none());
            driver
                .drain_sealable(Instant::now(), Duration::ZERO)
                .await?;
            assert_eq!(runtime.latest_snapshot().point().block_number(), 500);
            assert_eq!(asserter.read_q().len(), 1);
            assert_eq!(stats.snapshot().reconciliation_requests(), 0);
            runtime.shutdown().await?;
            Ok(())
        })
        .await
}

#[test]
fn event_only_attachment_rejects_hidden_read_configuration() {
    let cfg = AmmSubscriberDriverConfig::default()
        .with_canonical_log_source(CanonicalLogSource::SubscriptionOnly);
    for fault in 0..7 {
        let mut source = SubscriberConfig::default();
        source.reconnect.enabled = fault == 0;
        source.verify_log_block_context = fault == 1;
        source.hydrate_pending_transactions = fault == 2;
        if fault == 3 {
            source.preconfirmations = evm_fork_cache::reactive::PreconfirmationMode::Preferred;
        }
        let mode = match fault {
            4 => SubscriberMode::Auto,
            6 => SubscriberMode::Polling,
            _ => SubscriberMode::PubSub,
        };
        let subscriber = AlloySubscriber::new(
            ProviderBuilder::new().connect_mocked_client(Asserter::new()),
            mode,
            source,
        )
        .with_gap_policy(if fault == 5 {
            SubscriberGapPolicy::Reconcile
        } else {
            SubscriberGapPolicy::FailClosed
        });
        assert!(super::super::event_only::validate_attachment(&subscriber, cfg).is_err());
    }
}
