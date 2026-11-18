use super::*;
use alloy_primitives::{B256, Log as PrimitiveLog};
use alloy_rpc_client::RpcClient;
use alloy_transport::mock::Asserter;
use evm_fork_cache::{StateUpdate, reactive::ReactiveReport};

fn hash(number: u64) -> B256 {
    B256::from(U256::from(number).to_be_bytes::<32>())
}

fn block(number: u64) -> BlockRef {
    BlockRef {
        number,
        hash: hash(number),
        parent_hash: Some(hash(number - 1)),
        timestamp: Some(1_700_000_000 + number),
    }
}

fn log(address: Address, number: u64, r0: u64, r1: u64) -> RpcLog {
    let mut data = U256::from(r0).to_be_bytes::<32>().to_vec();
    data.extend_from_slice(&U256::from(r1).to_be_bytes::<32>());
    RpcLog {
        inner: PrimitiveLog::new_unchecked(
            address,
            vec![keccak256("Sync(uint112,uint112)")],
            data.into(),
        ),
        block_hash: Some(hash(number)),
        block_number: Some(number),
        block_timestamp: Some(1_700_000_000 + number),
        transaction_hash: Some(hash(number + 1000)),
        transaction_index: Some(0),
        log_index: Some(0),
        removed: false,
    }
}

async fn setup() -> Result<(EvmCache, ReactiveRuntime<Ethereum>)> {
    let provider = RootProvider::<AnyNetwork>::new(RpcClient::mocked(Asserter::new()));
    let mut cache = EvmCache::new(Arc::new(provider)).await;
    cache.apply_updates(&[StateUpdate::slot(
        V2_USDC_WETH_PAIR,
        V2_RESERVES_SLOT,
        U256::from(100) | (U256::from(200) << 112),
    )]);
    let registration = PoolRegistration::new(PoolKey::UniswapV2(V2_USDC_WETH_PAIR))
        .with_state_address(V2_USDC_WETH_PAIR)
        .with_metadata(ProtocolMetadata::UniswapV2(
            UniswapV2Metadata::default()
                .with_token0(USDC)
                .with_token1(WETH)
                .with_fee_bps(30),
        ));
    let adapter = UniswapV2Adapter::default();
    let sources = adapter.event_sources(&registration);
    let mut registry = AdapterRegistry::new();
    registry.register_adapter(Arc::new(adapter))?;
    registry.register_pool(registration.with_event_sources(sources))?;
    let mut runtime = ReactiveRuntime::new(ReactiveConfig::default());
    runtime.register_handler(Arc::new(AmmReactiveHandler::new(registry)))?;
    Ok((cache, runtime))
}

fn ingest(
    cache: &mut EvmCache,
    runtime: &mut ReactiveRuntime<Ethereum>,
    log: RpcLog,
    ctx: ReactiveContext,
) -> Result<evm_fork_cache::reactive::ReactiveBatchReport<Ethereum>> {
    Ok(runtime.ingest_batch(
        cache,
        ReactiveInputBatch::new(vec![ReactiveInputRecord::new(ReactiveInput::Log(log), ctx)]),
    )?)
}

#[tokio::test]
async fn fixture_context_preserves_reserves_after_unrelated_adjacent_input() -> Result<()> {
    let (mut cache, mut runtime) = setup().await?;
    let first = log(V2_USDC_WETH_PAIR, 11, 123, 456);
    let ctx = ctx_from_log(&first, block(11))?;
    ingest(&mut cache, &mut runtime, first, ctx)?;
    assert_eq!(cached_reserves(&cache), (U256::from(123), U256::from(456)));
    let unrelated = log(Address::repeat_byte(0xaa), 12, 7, 8);
    let ctx = ctx_from_log(&unrelated, block(12))?;
    let report = ingest(&mut cache, &mut runtime, unrelated, ctx)?;
    assert!(report.applied.is_empty());
    assert_eq!(
        cached_reserves(&cache),
        (U256::from(123), U256::from(456)),
        "the live fixture must not turn missing parent metadata into a rollback"
    );
    assert!(
        !report
            .reports
            .iter()
            .any(|r| matches!(r.as_ref(), ReactiveReport::Reorg(_)))
    );
    Ok(())
}

#[tokio::test]
async fn historical_parentless_input_reproduces_rollback_and_is_rejected() -> Result<()> {
    let (mut cache, mut runtime) = setup().await?;
    let first = log(V2_USDC_WETH_PAIR, 11, 123, 456);
    let ctx = ctx_from_log(&first, block(11))?;
    ingest(&mut cache, &mut runtime, first, ctx)?;
    let unrelated = log(Address::repeat_byte(0xaa), 12, 7, 8);
    let mut ctx = ctx_from_log(&unrelated, block(12))?;
    let unproven = BlockRef {
        parent_hash: None,
        ..block(12)
    };
    ctx.block = Some(unproven);
    ctx.chain_status = ChainStatus::Included {
        block: unproven,
        confirmations: 0,
    };
    let report = ingest(&mut cache, &mut runtime, unrelated, ctx)?;
    assert_eq!(cached_reserves(&cache), (U256::from(100), U256::from(200)));
    assert!(
        report
            .reports
            .iter()
            .any(|r| matches!(r.as_ref(), ReactiveReport::Reorg(_)))
    );
    assert!(lineage::require_no_recovery(&report).is_err());
    Ok(())
}

#[test]
fn removed_missing_and_conflicting_header_identity_are_rejected() {
    let mut input = log(V2_USDC_WETH_PAIR, 11, 123, 456);
    for bad in [
        block(12),
        BlockRef {
            parent_hash: None,
            ..block(11)
        },
        BlockRef {
            timestamp: Some(1),
            ..block(11)
        },
    ] {
        assert!(ctx_from_log(&input, bad).is_err());
    }
    input.removed = true;
    assert!(ctx_from_log(&input, block(11)).is_err());
}

#[tokio::test]
async fn strict_snapshot_has_no_lazy_storage_or_resync_path() -> Result<()> {
    use evm_amm_state::adapters::AdapterCache;
    let (mut cache, _) = setup().await?;
    let mut strict = strict::StrictCache::new(cache.snapshot());
    assert!(
        strict
            .read_storage_slot(V2_USDC_WETH_PAIR, V2_RESERVES_SLOT)
            .is_ok()
    );
    assert!(
        strict
            .read_storage_slot(V2_USDC_WETH_PAIR, U256::from(99))
            .is_err()
    );
    assert!(
        strict
            .verify_slots(&[(V2_USDC_WETH_PAIR, V2_RESERVES_SLOT)])
            .is_err()
    );
    assert_eq!(cached_reserves(&cache), (U256::from(100), U256::from(200)));
    Ok(())
}

#[tokio::test]
async fn changed_hash_is_not_accepted_as_the_same_canonical_block() -> Result<()> {
    let (mut cache, mut runtime) = setup().await?;
    let first = log(V2_USDC_WETH_PAIR, 11, 123, 456);
    let ctx = ctx_from_log(&first, block(11))?;
    ingest(&mut cache, &mut runtime, first, ctx)?;
    let mut replacement = log(Address::repeat_byte(0xaa), 11, 7, 8);
    replacement.block_hash = Some(hash(911));
    let replacement_block = BlockRef {
        hash: hash(911),
        ..block(11)
    };
    let ctx = ctx_from_log(&replacement, replacement_block)?;
    let report = ingest(&mut cache, &mut runtime, replacement, ctx)?;
    assert!(lineage::require_no_recovery(&report).is_err());
    Ok(())
}

#[tokio::test]
async fn metadata_cap_refuses_before_touching_even_a_mock_provider() -> Result<()> {
    let provider = RootProvider::<AnyNetwork>::new(RpcClient::mocked(Asserter::new()));
    let mut headers = lineage::Headers::default();
    headers.requests = lineage::MAX_HEADER_READS;
    let error = headers
        .records(&provider, None, &log(V2_USDC_WETH_PAIR, 11, 1, 2))
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("header metadata read cap exceeded")
    );
    assert_eq!(headers.requests, lineage::MAX_HEADER_READS);
    Ok(())
}

#[tokio::test]
async fn disconnected_evm_records_missing_account_without_provider_fallback() -> Result<()> {
    use evm_amm_state::adapters::AdapterCache;
    let (mut cache, _) = setup().await?;
    let mut strict = strict::StrictCache::new(cache.snapshot());
    let _ = strict.call_raw(
        Address::ZERO,
        Address::repeat_byte(0xbb),
        Bytes::new(),
        false,
    );
    assert!(
        !strict.overlay.missing_state().is_empty(),
        "a seemingly successful empty call must not count as a prepared quote"
    );
    Ok(())
}

fn header(number: u64, parent: B256) -> alloy_rpc_types_eth::Header {
    alloy_rpc_types_eth::Header::new(alloy_consensus::Header {
        number,
        parent_hash: parent,
        timestamp: 1_700_000_000 + number,
        gas_limit: 30_000_000,
        base_fee_per_gas: Some(1),
        ..Default::default()
    })
}

fn header_reply(header: &alloy_rpc_types_eth::Header) -> serde_json::Value {
    let mut reply = serde_json::to_value(header).unwrap();
    reply["transactions"] = serde_json::json!([]);
    reply["uncles"] = serde_json::json!([]);
    reply
}

#[tokio::test]
async fn verified_intervening_headers_preserve_sparse_event_state_without_resync() -> Result<()> {
    let (mut cache, mut runtime) = setup().await?;
    let first_header = header(11, hash(10));
    let middle = header(12, first_header.hash);
    let last = header(13, middle.hash);
    let first_block = BlockRef {
        hash: first_header.hash,
        ..block(11)
    };
    let mut first = log(V2_USDC_WETH_PAIR, 11, 123, 456);
    first.block_hash = Some(first_header.hash);
    let ctx = ctx_from_log(&first, first_block)?;
    ingest(&mut cache, &mut runtime, first, ctx)?;
    let asserter = Asserter::new();
    asserter.push_success(&header_reply(&last));
    asserter.push_success(&header_reply(&middle));
    let provider = RootProvider::<AnyNetwork>::new(RpcClient::mocked(asserter));
    let mut unrelated = log(Address::repeat_byte(0xaa), 13, 7, 8);
    unrelated.block_hash = Some(last.hash);
    let mut headers = lineage::Headers::default();
    let (mut records, context) = headers
        .records(&provider, Some(first_block), &unrelated)
        .await?;
    assert_eq!(headers.requests, 2);
    assert_eq!(records.len(), 1);
    assert!(matches!(&records[0].input, ReactiveInput::BlockHeader(h) if h.hash == middle.hash));
    records.push(ReactiveInputRecord::new(
        ReactiveInput::Log(unrelated.clone()),
        context,
    ));
    let report = runtime.ingest_batch(&mut cache, ReactiveInputBatch::new(records))?;
    lineage::require_no_recovery(&report)?;
    assert_eq!(cached_reserves(&cache), (U256::from(123), U256::from(456)));
    assert!(report.applied.is_empty());
    headers
        .records(&provider, runtime.last_canonical_block(), &unrelated)
        .await?;
    assert_eq!(
        headers.requests, 2,
        "cached header reuse must not spend another request"
    );
    Ok(())
}

#[tokio::test]
async fn forged_header_payload_and_unrelated_parent_are_rejected() -> Result<()> {
    for corrupt_hash in [true, false] {
        let first_block = block(11);
        let mut incoming = header(12, hash(99));
        let mut input = log(V2_USDC_WETH_PAIR, 12, 1, 2);
        if corrupt_hash {
            incoming.hash = hash(12);
        }
        input.block_hash = Some(incoming.hash);
        let asserter = Asserter::new();
        asserter.push_success(&header_reply(&incoming));
        let provider = RootProvider::<AnyNetwork>::new(RpcClient::mocked(asserter));
        let mut headers = lineage::Headers::default();
        let result = headers.records(&provider, Some(first_block), &input).await;
        assert!(result.is_err());
        assert_eq!(headers.requests, 1);
    }
    Ok(())
}
