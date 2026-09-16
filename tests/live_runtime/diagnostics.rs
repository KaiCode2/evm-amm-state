use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn committed_decode_failure_preserves_typed_cause_and_event_identity() -> Result<()> {
    tokio::task::LocalSet::new().run_until(async {
        let mut cache = setup_cache().await;
        align_cache(&mut cache, 500);
        let emitter = Address::repeat_byte(0x31);
        let topic = B256::repeat_byte(0x32);
        let mut registry = AdapterRegistry::new();
        registry.register_adapter(Arc::new(TestFailingAdapter { protocol: "diagnostic-test", emitter, topic }))?;
        let key = PoolKey::Custom(CustomPoolKey::Address { protocol: "diagnostic-test", address: emitter });
        registry.register_pool(PoolRegistration::new(key.clone()).with_status(PoolStatus::Ready))?;
        let runtime = AmmRuntime::spawn(cache, registry, runtime_baseline(500), AmmRuntimeConfig::default())?;
        let changes = runtime.ingest_batch(canonical_log_batch(501, [(emitter, topic, 7)])).await?;
        let diagnostics = changes.decode_diagnostics();
        assert_eq!(diagnostics.entries().len(), 1);
        let entry = &diagnostics.entries()[0];
        assert_eq!(entry.pool().key(), &key);
        assert_eq!(entry.error(), &AdapterEventError::MalformedLog("forced runtime failure"));
        assert_eq!(entry.error_class(), "malformed_log");
        assert!(matches!(entry.input(), evm_fork_cache::reactive::InputRef::Log { transaction_hash, log_index: 7, .. } if *transaction_hash == B256::repeat_byte(8)));
        assert_eq!(diagnostics.omitted(), 0);
        runtime.shutdown().await?;
        Ok(())
    }).await
}

#[tokio::test(flavor = "multi_thread")]
async fn committed_decode_diagnostics_are_bounded_and_do_not_leak_into_the_next_commit()
-> Result<()> {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut cache = setup_cache().await;
            align_cache(&mut cache, 500);
            let emitter = Address::repeat_byte(0x31);
            let topic = B256::repeat_byte(0x32);
            let mut registry = AdapterRegistry::new();
            registry.register_adapter(Arc::new(TestFailingAdapter {
                protocol: "bounded-diagnostics",
                emitter,
                topic,
            }))?;
            registry.register_pool(custom_registration("bounded-diagnostics", emitter))?;
            let runtime = AmmRuntime::spawn(
                cache,
                registry,
                runtime_baseline(500),
                AmmRuntimeConfig::default(),
            )?;
            let changes = runtime
                .ingest_batch(canonical_log_batch(
                    501,
                    (0..40).map(|index| (emitter, topic, index)),
                ))
                .await?;
            assert_eq!(changes.decode_diagnostics().entries().len(), 32);
            assert_eq!(changes.decode_diagnostics().omitted(), 8);
            let next = runtime.ingest_batch(canonical_log_batch(502, [])).await?;
            assert!(next.decode_diagnostics().entries().is_empty());
            assert_eq!(next.decode_diagnostics().omitted(), 0);
            runtime.shutdown().await?;
            Ok(())
        })
        .await
}
