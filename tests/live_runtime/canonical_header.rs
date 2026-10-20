//! Full-header evidence must follow the same reliable publication as its state.
use super::*;
use evm_amm_state::adapters::{AmmStateCommit, AmmStateIncident};

fn assert_header(commit: &AmmStateCommit, expected: &RpcHeader) {
    let evidence = commit.canonical_header().expect("canonical evidence");
    assert_eq!(evidence.header(), expected);
    assert_eq!(evidence.point(), commit.snapshot().point());
    assert_eq!(evidence.point(), commit.changes().point());
    assert_eq!(evidence.point().block_hash(), expected.hash);
    assert_eq!(commit.snapshot().version(), commit.changes().version());
}

#[tokio::test(flavor = "multi_thread")]
async fn exact_header_survives_empty_blocks_and_reliable_backpressure() -> Result<()> {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut cache = setup_cache().await;
            align_cache(&mut cache, 500);
            let runtime = AmmRuntime::spawn(
                cache,
                AdapterRegistry::new(),
                runtime_baseline(500),
                AmmRuntimeConfig::default().with_critical_change_capacity(1),
            )?;
            let mut commits = runtime.subscribe_changes().await?;
            let mut inner = canonical_header(501).inner;
            // A block may have unrelated logs while the registered interest set is empty.
            inner.logs_bloom = alloy_primitives::Bloom::repeat_byte(0x21);
            let first = RpcHeader::new(inner);
            let second = canonical_child_header(&first);
            runtime
                .ingest_batch(empty_canonical_batch_from_header(first.clone()))
                .await?;
            let waiting_runtime = runtime.clone();
            let waiting_header = second.clone();
            let mut blocked = tokio::task::spawn_local(async move {
                waiting_runtime
                    .ingest_batch(empty_canonical_batch_from_header(waiting_header))
                    .await
            });
            assert!(
                tokio::time::timeout(Duration::from_millis(20), &mut blocked)
                    .await
                    .is_err()
            );
            assert_eq!(runtime.latest_snapshot().point().block_number(), 501);
            let first_commit = commits.next_commit().await.expect("first queued commit");
            assert_header(&first_commit, &first);
            blocked.await??;
            let second_commit = commits.next_commit().await.expect("second queued commit");
            assert_header(&second_commit, &second);
            assert_eq!(first_commit.changes().version(), AmmStateVersion::new(1));
            assert_eq!(second_commit.changes().version(), AmmStateVersion::new(2));
            assert_eq!(
                first_commit.snapshot().runtime_id(),
                second_commit.snapshot().runtime_id()
            );
            assert_eq!(second.parent_hash, first.hash);
            assert!(commits.try_next_commit().is_none());
            runtime.shutdown().await?;
            assert!(commits.next_commit().await.is_none());
            Ok(())
        })
        .await
}

#[tokio::test(flavor = "multi_thread")]
async fn lifecycle_commits_and_replacement_subscriptions_do_not_reuse_header_evidence() -> Result<()>
{
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut cache = setup_cache().await;
            align_cache(&mut cache, 500);
            let runtime = AmmRuntime::spawn(
                cache,
                AdapterRegistry::new(),
                runtime_baseline(500),
                AmmRuntimeConfig::default(),
            )?;
            let mut commits = runtime.subscribe_changes().await?;
            runtime.ingest_batch(empty_canonical_batch(501)).await?;
            assert_header(
                &commits.next_commit().await.expect("canonical commit"),
                &canonical_header(501),
            );
            let adapter = runtime
                .add_adapter(Arc::new(TestWriteAdapter {
                    protocol: "header-lifecycle",
                    emitter: Address::repeat_byte(0xa1),
                    topic: B256::repeat_byte(0xa2),
                    target: StateSlot::new(Address::repeat_byte(0xa3), U256::from(4)),
                }))
                .await?;
            assert!(
                commits
                    .next_commit()
                    .await
                    .expect("add commit")
                    .canonical_header()
                    .is_none()
            );
            runtime.remove_adapter(adapter).await?;
            assert!(
                commits
                    .next_commit()
                    .await
                    .expect("remove commit")
                    .canonical_header()
                    .is_none()
            );
            runtime
                .install_prepared_pools(Vec::new(), runtime.latest_snapshot().point())
                .await?;
            assert!(
                commits
                    .next_commit()
                    .await
                    .expect("prepared commit")
                    .canonical_header()
                    .is_none()
            );
            drop(commits);
            let mut replacement = runtime.subscribe_changes().await?;
            assert_eq!(replacement.snapshot().version(), AmmStateVersion::new(4));
            assert!(
                replacement.try_next_commit().is_none(),
                "a new baseline does not replay header history"
            );
            runtime.ingest_batch(empty_canonical_batch(502)).await?;
            let next = replacement.next_commit().await.expect("new commit");
            assert_header(&next, &canonical_header(502));
            assert_eq!(next.changes().version(), AmmStateVersion::new(5));
            runtime.shutdown().await?;
            Ok(())
        })
        .await
}

#[tokio::test(flavor = "multi_thread")]
async fn reorg_header_is_the_replacement_and_keeps_the_reset_incident() -> Result<()> {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut cache = setup_cache().await;
            align_cache(&mut cache, 500);
            let runtime = AmmRuntime::spawn(
                cache,
                AdapterRegistry::new(),
                runtime_baseline(500),
                AmmRuntimeConfig::default(),
            )?;
            let mut commits = runtime.subscribe_changes().await?;
            runtime.ingest_batch(empty_canonical_batch(501)).await?;
            let original = commits.next_commit().await.expect("original");
            let mut inner = canonical_header(501).inner;
            inner.logs_bloom = alloy_primitives::Bloom::repeat_byte(0x42);
            let replacement = RpcHeader::new(inner);
            runtime
                .ingest_batch(empty_canonical_batch_from_header(replacement.clone()))
                .await?;
            let changed = commits.next_commit().await.expect("replacement");
            assert_header(&changed, &replacement);
            assert_ne!(
                original.canonical_header().unwrap().header().hash,
                replacement.hash
            );
            assert!(matches!(
                changed.changes().incidents(),
                [AmmStateIncident::Reorg { .. }]
            ));
            runtime.shutdown().await?;
            Ok(())
        })
        .await
}

#[test]
fn modified_bloom_without_resealing_cannot_create_header_evidence() {
    let mut header = canonical_header(501);
    header.inner.logs_bloom = alloy_primitives::Bloom::repeat_byte(0x42);
    assert!(matches!(
        AmmCanonicalBatch::from_verified_block(1, header, 0, ReactiveInputBatch::new(Vec::new())),
        Err(AmmCanonicalBatchError::HeaderHashMismatch { .. })
    ));
}
