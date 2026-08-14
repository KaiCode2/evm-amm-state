use std::{
    collections::BTreeMap,
    str::FromStr,
    sync::Arc,
    time::{Duration, Instant},
};

use alloy_eips::BlockId;
use alloy_network::Ethereum;
use alloy_primitives::{Address, B256, Bytes, Log, U256, keccak256};
use alloy_provider::{RootProvider, network::AnyNetwork};
use alloy_rpc_client::RpcClient;
use alloy_rpc_types_eth::Log as RpcLog;
use alloy_transport::mock::Asserter;
use evm_amm_state::adapters::{
    AdapterEventContext, AdapterRegistry, AmmAdapter, AmmPoolReactiveHandler,
    AmmReactiveRoutingContext, ConcentratedLiquidityAdapter, PoolGeneration, PoolInstanceId,
    PoolKey, PoolRegistration, ProtocolMetadata, SimConfig, SlipstreamFeeEvidenceError,
    SlipstreamFeeEvidenceInsertOutcome, SlipstreamRuntimeFamily, SlipstreamSnapshotIdentity,
    SlipstreamSwapFeeEvidence, StateUpdate, StateView, UpdateQuality, V3Metadata, V3StorageLayout,
    V3SwapTransitionCapability,
};
use evm_fork_cache::cache::EvmCache;
use evm_fork_cache::reactive::{
    BlockRef, ChainStatus, InputSource, ReactiveConfig, ReactiveInput, ReactiveInputBatch,
    ReactiveInputRecord, ReactiveRuntime, StateEffectQuality,
};
use revm::state::{AccountInfo, Bytecode};
use serde_json::Value;

const LOCAL_SLIPSTREAM_QUOTER: Address =
    alloy_primitives::address!("00000000000000000000000000000000000000c6");

#[derive(Clone, Default)]
struct FixtureState(BTreeMap<(Address, U256), U256>);

impl StateView for FixtureState {
    fn storage(&self, address: Address, slot: U256) -> Option<U256> {
        self.0.get(&(address, slot)).copied()
    }
}

fn word(value: &str) -> U256 {
    U256::from_str(value).expect("valid checked-in word")
}

fn address(value: &Value) -> Address {
    Address::from_str(value.as_str().expect("address string")).expect("valid checked-in address")
}

fn hash(value: &Value) -> B256 {
    B256::from_str(value.as_str().expect("hash string")).expect("valid checked-in hash")
}

fn signed_word(value: &str) -> U256 {
    if let Some(magnitude) = value.strip_prefix('-') {
        U256::ZERO.wrapping_sub(U256::from_str(magnitude).expect("signed decimal magnitude"))
    } else {
        U256::from_str(value).expect("signed decimal value")
    }
}

fn topic_address(address: Address) -> B256 {
    let mut topic = [0_u8; 32];
    topic[12..].copy_from_slice(address.as_slice());
    B256::from(topic)
}

async fn cache(chain_id: u64) -> EvmCache {
    let asserter = Asserter::new();
    let client = RpcClient::mocked(asserter);
    let provider = RootProvider::<AnyNetwork>::new(client);
    let mut cache = EvmCache::new(Arc::new(provider)).await;
    cache.set_chain_id(chain_id);
    cache
}

async fn provider_disconnected_cache(chain_id: u64) -> (EvmCache, Asserter) {
    let asserter = Asserter::new();
    let client = RpcClient::mocked(asserter.clone());
    let provider = RootProvider::<AnyNetwork>::new(client);
    let mut cache = EvmCache::new(Arc::new(provider)).await;
    cache.set_chain_id(chain_id);
    asserter.push_failure_msg("Slipstream event-to-simulation must not access the provider");
    (cache, asserter)
}

fn runtime(hex_runtime: &str) -> Bytes {
    Bytes::from(
        alloy_primitives::hex::decode(hex_runtime.trim()).expect("checked-in deployed runtime"),
    )
}

fn address_word(address: Address) -> U256 {
    U256::from_be_slice(address.as_slice())
}

fn mapping_slot(address: Address, slot: U256) -> U256 {
    let mut encoded = [0_u8; 64];
    encoded[12..32].copy_from_slice(address.as_slice());
    encoded[32..].copy_from_slice(&slot.to_be_bytes::<32>());
    U256::from_be_slice(keccak256(encoded).as_slice())
}

fn install(cache: &mut EvmCache, address: Address, code: Bytes, slots: &[(U256, U256)]) {
    let code = Bytecode::new_raw(code);
    let code_hash = code.hash_slow();
    cache.db_mut().insert_account_info(
        address,
        AccountInfo {
            balance: U256::ZERO,
            nonce: 1,
            code: Some(code),
            code_hash,
            account_id: None,
        },
    );
    cache
        .db_mut()
        .replace_account_storage(address, Default::default())
        .expect("mark deployed account storage exact");
    for (slot, value) in slots {
        cache
            .db_mut()
            .insert_account_storage(address, *slot, *value)
            .expect("seed traced parent storage");
    }
}

fn reviewed_runtimes(family: SlipstreamRuntimeFamily) -> [(Address, Bytes); 5] {
    match family {
        SlipstreamRuntimeFamily::AerodromeBaseBifi => [
            (
                alloy_primitives::address!("b378137c90444bbcecd44a1f766851fbf53d2a9e"),
                runtime(include_str!("fixtures/base_slipstream_proxy_runtime.hex")),
            ),
            (
                alloy_primitives::address!("ec8e5342b19977b4ef8892e02d8daecfa1315831"),
                runtime(include_str!(
                    "fixtures/base_slipstream_implementation_runtime.hex"
                )),
            ),
            (
                alloy_primitives::address!("5e7bb104d84c7cb9b682aac2f3d509f5f406809a"),
                runtime(include_str!("fixtures/base_slipstream_factory_runtime.hex")),
            ),
            (
                alloy_primitives::address!("16613524e02ad97edfeF371bc883F2F5d6C480A5"),
                runtime(include_str!("fixtures/base_slipstream_voter_runtime.hex")),
            ),
            (
                alloy_primitives::address!("0ad08370c76ff426f534bb2affd9b5555338ee68"),
                runtime(include_str!(
                    "fixtures/base_slipstream_unstaked_module_runtime.hex"
                )),
            ),
        ],
        SlipstreamRuntimeFamily::VelodromeOptimismBifi => [
            (
                alloy_primitives::address!("173cdc71e29d5cffa6d090ad99f555a24b8831f9"),
                runtime(include_str!(
                    "fixtures/optimism_slipstream_proxy_runtime.hex"
                )),
            ),
            (
                alloy_primitives::address!("c28ad28853a547556780bebf7847628501a3bcbb"),
                runtime(include_str!(
                    "fixtures/optimism_slipstream_implementation_runtime.hex"
                )),
            ),
            (
                alloy_primitives::address!("cc0bddb707055e04e497ab22a59c2af4391cd12f"),
                runtime(include_str!(
                    "fixtures/optimism_slipstream_factory_runtime.hex"
                )),
            ),
            (
                alloy_primitives::address!("41c914ee0c7e1a5edcd0295623e6dc557b5abf3c"),
                runtime(include_str!(
                    "fixtures/optimism_slipstream_voter_runtime.hex"
                )),
            ),
            (
                alloy_primitives::address!("c565f7ba9c56b157da983c4db30e13f5f06c59d9"),
                runtime(include_str!(
                    "fixtures/optimism_slipstream_unstaked_module_runtime.hex"
                )),
            ),
        ],
        _ => panic!("unsupported reviewed fixture family"),
    }
}

async fn assert_fixture(contents: &str) {
    let fixture: Value = serde_json::from_str(contents).expect("valid checked-in fixture");
    let reference = &fixture["reference"];
    let pool_json = &fixture["pool"];
    let swap = &fixture["swap_log"];
    let pool = address(&pool_json["address"]);
    let chain_id = reference["chain_id"].as_u64().expect("chain id");
    let family = match chain_id {
        8_453 => SlipstreamRuntimeFamily::AerodromeBaseBifi,
        10 => SlipstreamRuntimeFamily::VelodromeOptimismBifi,
        other => panic!("unexpected fixture chain {other}"),
    };
    let factory = fixture["parent_storage"]["0x00"]
        .as_str()
        .map(word)
        .map(|word| Address::from_slice(&word.to_be_bytes::<32>()[12..]))
        .expect("factory storage word");
    let token0 = fixture["parent_storage"]["0x01"]
        .as_str()
        .map(word)
        .map(|word| Address::from_slice(&word.to_be_bytes::<32>()[12..]))
        .expect("token0 storage word");
    let token1 = fixture["parent_storage"]["0x02"]
        .as_str()
        .map(word)
        .map(|word| Address::from_slice(&word.to_be_bytes::<32>()[12..]))
        .expect("token1 storage word");
    let transaction_hash = hash(&reference["transaction_hash"]);
    let block_number = reference["block_number"].as_u64().expect("block number");
    let block_hash = hash(&reference["block_hash"]);
    let parent_hash = hash(&reference["parent_hash"]);
    let block_timestamp = reference["block_timestamp"].as_u64().expect("timestamp");
    let transaction_index = reference["transaction_index"].as_u64().expect("tx index");
    let log_index = reference["log_index"].as_u64().expect("log index");
    let base_context = AdapterEventContext::for_block(block_number, block_hash, block_timestamp)
        .with_chain_id(chain_id)
        .with_parent_hash(parent_hash)
        .with_transaction_hash(transaction_hash)
        .with_event_order(transaction_index, log_index);
    let snapshot_identity = SlipstreamSnapshotIdentity::new(
        chain_id,
        block_number,
        block_hash,
        parent_hash,
        block_timestamp,
        transaction_hash,
        transaction_index,
        log_index,
    )
    .expect("complete fixture identity");

    let mut fee_cache = cache(chain_id).await;
    fee_cache.set_block(BlockId::from((block_hash, Some(true))));
    fee_cache.set_timestamp(Some(block_timestamp));
    fee_cache.set_block_context(Some(block_number), Some(0));
    fee_cache
        .db_mut()
        .cache
        .block_hashes
        .insert(U256::from(block_number - 1), parent_hash);
    fee_cache
        .db_mut()
        .insert_account_info(Address::ZERO, AccountInfo::default());
    let mut runtime_accounts = reviewed_runtimes(family);
    for (runtime_address, runtime_code) in &mut runtime_accounts {
        let slots = if *runtime_address == pool {
            fixture["parent_storage"]
                .as_object()
                .expect("parent storage map")
                .iter()
                .map(|(slot, value)| (word(slot), word(value.as_str().expect("parent word"))))
                .collect::<Vec<_>>()
        } else {
            fixture["fee_parent_state"]
                .get(format!("{runtime_address:#x}"))
                .and_then(Value::as_object)
                .map(|slots| {
                    slots
                        .iter()
                        .map(|(slot, value)| {
                            (word(slot), word(value.as_str().expect("fee parent word")))
                        })
                        .collect()
                })
                .unwrap_or_default()
        };
        install(
            &mut fee_cache,
            *runtime_address,
            runtime_code.clone(),
            &slots,
        );
    }
    let fee_snapshot = fee_cache.snapshot();
    let replacement_parent = B256::repeat_byte(0x7f);
    let replacement_context = base_context.with_parent_hash(replacement_parent);
    let replacement_identity = SlipstreamSnapshotIdentity::new(
        chain_id,
        block_number,
        block_hash,
        replacement_parent,
        block_timestamp,
        transaction_hash,
        transaction_index,
        log_index,
    )
    .expect("complete replacement identity");
    assert_eq!(
        ConcentratedLiquidityAdapter::evaluate_slipstream_unstaked_fee(
            family,
            Arc::clone(&fee_snapshot),
            replacement_identity,
            &replacement_context,
        ),
        Err(evm_amm_state::adapters::SlipstreamUnstakedFeeEvaluationError::SnapshotIdentity),
        "same-height replacement lineage must not reuse old fee state",
    );
    let replacement_block_hash = B256::repeat_byte(0x7e);
    let replacement_block_context =
        AdapterEventContext::for_block(block_number, replacement_block_hash, block_timestamp)
            .with_chain_id(chain_id)
            .with_parent_hash(parent_hash)
            .with_transaction_hash(transaction_hash)
            .with_event_order(transaction_index, log_index);
    let replacement_block_identity = SlipstreamSnapshotIdentity::new(
        chain_id,
        block_number,
        replacement_block_hash,
        parent_hash,
        block_timestamp,
        transaction_hash,
        transaction_index,
        log_index,
    )
    .expect("complete same-height replacement identity");
    assert_eq!(
        ConcentratedLiquidityAdapter::evaluate_slipstream_unstaked_fee(
            family,
            Arc::clone(&fee_snapshot),
            replacement_block_identity,
            &replacement_block_context,
        ),
        Err(evm_amm_state::adapters::SlipstreamUnstakedFeeEvaluationError::SnapshotIdentity),
        "a same-parent same-timestamp replacement hash must not reuse the old snapshot",
    );
    let all_staked_candidate =
        ConcentratedLiquidityAdapter::evaluate_slipstream_all_staked_candidate(
            family,
            Arc::clone(&fee_snapshot),
            snapshot_identity,
            &base_context,
        )
        .expect("reviewed pool runtime may produce a replay-checked research candidate");
    assert_eq!(all_staked_candidate.effective_fee(), 0);
    let unstaked_fee = ConcentratedLiquidityAdapter::evaluate_slipstream_unstaked_fee(
        family,
        Arc::clone(&fee_snapshot),
        snapshot_identity,
        &base_context,
    )
    .expect("deployed runtimes must produce provider-free unstaked fee evidence");
    assert_eq!(
        unstaked_fee.effective_fee(),
        pool_json["factory_unstaked_fee"]
            .as_u64()
            .expect("unstaked fee") as u32,
    );
    let drift_code = Bytecode::new_raw(Bytes::from_static(&[0x00]));
    fee_cache.db_mut().insert_account_info(
        pool,
        AccountInfo {
            balance: U256::ZERO,
            nonce: 1,
            code_hash: drift_code.hash_slow(),
            code: Some(drift_code),
            account_id: None,
        },
    );
    let drift_snapshot = fee_cache.snapshot();
    assert_eq!(
        ConcentratedLiquidityAdapter::evaluate_slipstream_unstaked_fee(
            family,
            drift_snapshot,
            snapshot_identity,
            &base_context,
        ),
        Err(
            evm_amm_state::adapters::SlipstreamUnstakedFeeEvaluationError::RuntimeCodeIdentity {
                missing: hash(&pool_json["proxy_runtime_code_hash"]),
            }
        ),
        "an address-preserving proxy runtime upgrade must invalidate the attestation",
    );
    assert_eq!(
        ConcentratedLiquidityAdapter::evaluate_slipstream_all_staked_candidate(
            family,
            fee_cache.snapshot(),
            snapshot_identity,
            &base_context,
        ),
        Err(
            evm_amm_state::adapters::SlipstreamUnstakedFeeEvaluationError::RuntimeCodeIdentity {
                missing: hash(&pool_json["proxy_runtime_code_hash"]),
            }
        ),
        "all-staked research candidates also require resident address-bound runtimes",
    );
    assert_eq!(
        SlipstreamSwapFeeEvidence::new(
            family,
            chain_id,
            pool,
            factory,
            hash(&pool_json["proxy_runtime_code_hash"]),
            address(&pool_json["implementation"]),
            hash(&pool_json["implementation_runtime_code_hash"]),
            pool_json["factory_fee"].as_u64().expect("swap fee") as u32,
            unstaked_fee.effective_fee() + 1,
            unstaked_fee.proof(),
            block_number,
            block_hash,
            parent_hash,
            block_timestamp,
            transaction_hash,
            transaction_index,
            log_index,
        ),
        Err(SlipstreamFeeEvidenceError::UnstakedFeeProofMismatch),
        "an opaque evaluator proof must bind the exact unstaked fee",
    );
    let evidence = SlipstreamSwapFeeEvidence::new(
        family,
        chain_id,
        pool,
        factory,
        hash(&pool_json["proxy_runtime_code_hash"]),
        address(&pool_json["implementation"]),
        hash(&pool_json["implementation_runtime_code_hash"]),
        pool_json["factory_fee"].as_u64().expect("swap fee") as u32,
        unstaked_fee.effective_fee(),
        unstaked_fee.proof(),
        block_number,
        block_hash,
        parent_hash,
        block_timestamp,
        transaction_hash,
        transaction_index,
        log_index,
    )
    .expect("reviewed fixture evidence");
    let mut invalid_evidence = evidence;
    invalid_evidence.effective_unstaked_fee += 1;
    assert_eq!(
        ConcentratedLiquidityAdapter::swap_transition_capability_with_context(
            &PoolRegistration::new(PoolKey::Slipstream(pool)).with_metadata(
                ProtocolMetadata::Slipstream(
                    V3Metadata::default()
                        .with_tick_spacing(
                            pool_json["tick_spacing"].as_i64().expect("spacing") as i32
                        )
                        .with_storage_layout(V3StorageLayout::slipstream(
                            pool_json["tick_spacing"].as_i64().expect("spacing") as i32,
                        )),
                ),
            ),
            &base_context.with_slipstream_fee_evidence(invalid_evidence),
        ),
        V3SwapTransitionCapability::Unsupported,
        "mutating a public evidence field must not produce an Exact capability",
    );
    let context = base_context.with_slipstream_fee_evidence(evidence);
    let spacing = pool_json["tick_spacing"].as_i64().expect("spacing") as i32;
    let registration = PoolRegistration::new(PoolKey::Slipstream(pool))
        .with_state_address(pool)
        .with_metadata(ProtocolMetadata::Slipstream(
            V3Metadata::default()
                .with_token0(token0)
                .with_token1(token1)
                .with_tick_spacing(spacing)
                .with_quoter(LOCAL_SLIPSTREAM_QUOTER)
                .with_storage_layout(V3StorageLayout::slipstream(spacing)),
        ));
    assert_eq!(
        ConcentratedLiquidityAdapter::swap_transition_capability_with_context(
            &registration,
            &context,
        ),
        V3SwapTransitionCapability::Exact,
        "reviewed runtime and exact event evidence must grant provider-free Slipstream replay",
    );
    assert_eq!(
        ConcentratedLiquidityAdapter::swap_transition_capability_with_context(
            &registration,
            &base_context,
        ),
        V3SwapTransitionCapability::Exact,
        "reviewed deployments must keep the quote/search surface exact without fee evidence",
    );
    let zero_spacing = PoolRegistration::new(PoolKey::Slipstream(pool)).with_metadata(
        ProtocolMetadata::Slipstream(
            V3Metadata::default()
                .with_tick_spacing(0)
                .with_storage_layout(V3StorageLayout::slipstream(0)),
        ),
    );
    assert_eq!(
        ConcentratedLiquidityAdapter::swap_transition_capability_with_context(
            &zero_spacing,
            &context,
        ),
        V3SwapTransitionCapability::Unsupported,
        "non-positive Slipstream tick spacing must never produce Exact capability",
    );

    let mut state = FixtureState::default();
    for (slot, value) in fixture["parent_storage"]
        .as_object()
        .expect("parent storage map")
    {
        state.0.insert(
            (pool, word(slot)),
            word(value.as_str().expect("parent word")),
        );
    }

    let mut data = Vec::with_capacity(160);
    data.extend_from_slice(
        &signed_word(swap["amount0"].as_str().expect("amount0")).to_be_bytes::<32>(),
    );
    data.extend_from_slice(
        &signed_word(swap["amount1"].as_str().expect("amount1")).to_be_bytes::<32>(),
    );
    data.extend_from_slice(
        &word(swap["sqrt_price_x96"].as_str().expect("sqrt")).to_be_bytes::<32>(),
    );
    data.extend_from_slice(
        &word(swap["liquidity"].as_str().expect("liquidity")).to_be_bytes::<32>(),
    );
    data.extend_from_slice(
        &signed_word(&swap["tick"].as_i64().expect("tick").to_string()).to_be_bytes::<32>(),
    );
    let log = Log::new(
        pool,
        vec![
            keccak256("Swap(address,address,int256,int256,uint160,uint128,int24)"),
            topic_address(address(&swap["sender"])),
            topic_address(address(&swap["recipient"])),
        ],
        Bytes::from(data),
    )
    .expect("valid fixture log");

    let adapter = Arc::new(ConcentratedLiquidityAdapter::default());
    let registration_with_sources = registration
        .clone()
        .with_event_sources(adapter.event_sources(&registration));
    let mut registry = AdapterRegistry::new();
    registry
        .register_adapter(adapter)
        .expect("register Slipstream adapter");
    registry
        .register_pool(registration_with_sources)
        .expect("register reviewed pool");
    let routing = AmmReactiveRoutingContext::new(Arc::new(registry));
    assert_eq!(
        routing.inject_slipstream_fee_evidence(evidence),
        SlipstreamFeeEvidenceInsertOutcome::Inserted,
    );
    let instance = PoolInstanceId::new(registration.key.clone(), PoolGeneration::new(1));
    let handler = Arc::new(
        AmmPoolReactiveHandler::with_routing_context(routing, instance)
            .expect("build real pool-scoped handler"),
    );
    let mut reactive = ReactiveRuntime::<Ethereum>::new(ReactiveConfig::default());
    reactive
        .register_handler(handler)
        .expect("register real pool-scoped handler");
    let block = BlockRef {
        number: block_number,
        hash: block_hash,
        parent_hash: Some(parent_hash),
        timestamp: Some(block_timestamp),
    };
    let rpc_log = RpcLog {
        inner: log.clone(),
        block_hash: Some(block_hash),
        block_number: Some(block_number),
        block_timestamp: Some(block_timestamp),
        transaction_hash: Some(transaction_hash),
        transaction_index: Some(transaction_index),
        log_index: Some(log_index),
        removed: false,
    };
    let reactive_context = evm_fork_cache::reactive::ReactiveContext {
        chain_id: Some(chain_id),
        source: InputSource::Synthetic,
        chain_status: ChainStatus::Included {
            block,
            confirmations: 0,
        },
        block: Some(block),
        transaction_index: Some(transaction_index),
        log_index: Some(log_index),
    };
    let report = reactive
        .ingest_batch(
            &mut fee_cache,
            ReactiveInputBatch::new(vec![ReactiveInputRecord::new(
                ReactiveInput::Log(rpc_log.clone()),
                reactive_context.clone(),
            )]),
        )
        .expect("real reactive handler fail-closes a Slipstream event");
    assert_eq!(report.applied.len(), 1);
    assert_eq!(
        report.applied[0].quality,
        StateEffectQuality::ExactFromInput,
    );
    assert_eq!(
        fee_cache.cached_storage_value(pool, U256::ZERO),
        Some(U256::from_be_slice(factory.as_slice())),
        "exact Slipstream replay must retain unrelated parent state",
    );

    // The production path deliberately does not require the optional
    // accounting evidence above. Re-run the same event through a fresh
    // reactive runtime backed by a mock provider with no prepared response:
    // any provider access would fail the test. The event must publish an exact
    // quote/search update without invalidation or repair.
    let (mut quote_cache, quote_provider) = provider_disconnected_cache(chain_id).await;
    let parent_slots = fixture["parent_storage"]
        .as_object()
        .expect("parent storage map")
        .iter()
        .map(|(slot, value)| (word(slot), word(value.as_str().expect("parent word"))))
        .collect::<Vec<_>>();
    for (runtime_address, runtime_code) in reviewed_runtimes(family) {
        let slots = if runtime_address == pool {
            parent_slots.clone()
        } else {
            fixture["fee_parent_state"]
                .get(format!("{runtime_address:#x}"))
                .and_then(Value::as_object)
                .map(|slots| {
                    slots
                        .iter()
                        .map(|(slot, value)| {
                            (word(slot), word(value.as_str().expect("fee parent word")))
                        })
                        .collect()
                })
                .unwrap_or_default()
        };
        install(&mut quote_cache, runtime_address, runtime_code, &slots);
    }
    quote_cache
        .db_mut()
        .insert_account_info(Address::ZERO, AccountInfo::default());
    let token_code = runtime(include_str!("fixtures/reference_swap_token_runtime.hex"));
    let pool_balance_slot = mapping_slot(pool, U256::ZERO);
    let pool_balance = U256::from(1) << 200_usize;
    install(
        &mut quote_cache,
        token0,
        token_code.clone(),
        &[(pool_balance_slot, pool_balance)],
    );
    install(
        &mut quote_cache,
        token1,
        token_code,
        &[(pool_balance_slot, pool_balance)],
    );
    install(
        &mut quote_cache,
        LOCAL_SLIPSTREAM_QUOTER,
        runtime(include_str!(
            "fixtures/v3_reference_swap_harness_runtime.hex"
        )),
        &[
            (U256::ZERO, address_word(token0)),
            (U256::from(1), address_word(token1)),
            (U256::from(2), address_word(pool)),
        ],
    );

    let adapter = Arc::new(ConcentratedLiquidityAdapter::default());
    let registration_with_sources = registration
        .clone()
        .with_event_sources(adapter.event_sources(&registration));
    let mut registry = AdapterRegistry::new();
    registry
        .register_adapter(adapter.clone())
        .expect("register Slipstream adapter");
    registry
        .register_pool(registration_with_sources)
        .expect("register reviewed pool");
    let routing = AmmReactiveRoutingContext::new(Arc::new(registry));
    let instance = PoolInstanceId::new(registration.key.clone(), PoolGeneration::new(1));
    let handler = Arc::new(
        AmmPoolReactiveHandler::with_routing_context(routing, instance)
            .expect("build evidence-free pool-scoped handler"),
    );
    let mut quote_reactive = ReactiveRuntime::<Ethereum>::new(ReactiveConfig::default());
    quote_reactive
        .register_handler(handler)
        .expect("register evidence-free pool-scoped handler");
    let event_started = Instant::now();
    let quote_report = quote_reactive
        .ingest_batch(
            &mut quote_cache,
            ReactiveInputBatch::new(vec![ReactiveInputRecord::new(
                ReactiveInput::Log(rpc_log),
                reactive_context,
            )]),
        )
        .expect("evidence-free Slipstream event must remain provider-disconnected");
    let event_elapsed = event_started.elapsed();
    assert_eq!(quote_report.applied.len(), 1);
    assert_eq!(
        quote_report.applied[0].quality,
        StateEffectQuality::ExactFromInput,
    );
    assert!(quote_report.applied[0].resyncs.is_empty());
    assert!(quote_report.applied[0].invalidations.is_empty());
    assert!(quote_report.resyncs.is_empty());
    assert!(quote_reactive.pending_resyncs().is_empty());
    assert_eq!(
        quote_cache.cached_storage_value(pool, U256::from(6)),
        fixture["expected_writes"]
            .get("0x06")
            .and_then(Value::as_str)
            .map(word),
        "evidence-free transition must publish exact packed price/tick/oracle state",
    );
    let word_128_mask = (U256::from(1) << 128_usize) - U256::from(1);
    for (slot, fixture_key) in [(U256::from(15), "0x0f"), (U256::from(16), "0x10")] {
        assert_eq!(
            quote_cache
                .cached_storage_value(pool, slot)
                .map(|value| value & word_128_mask),
            fixture["expected_writes"]
                .get(fixture_key)
                .and_then(Value::as_str)
                .map(word)
                .map(|value| value & word_128_mask),
            "evidence-free transition must publish exact executable liquidity at {slot}",
        );
    }
    for (slot, value) in fixture["expected_writes"]
        .as_object()
        .expect("expected deployed-runtime writes")
    {
        assert_eq!(
            fee_cache.cached_storage_value(pool, word(slot)),
            Some(word(value.as_str().expect("expected write word"))),
            "event transition must match the deployed-runtime write at {slot}",
        );
    }
    assert_eq!(
        ConcentratedLiquidityAdapter::infer_slipstream_swap_fee(
            &registration,
            &log,
            &state,
            &context,
        )
        .expect("fixture fee must be uniquely event-derived"),
        evidence.effective_swap_fee,
    );
    let decoded = ConcentratedLiquidityAdapter::default().decode_event_with_context(
        &registration,
        &log,
        &state,
        &context,
    );
    assert_eq!(decoded.error, None);
    let event = decoded.event.expect("recognized Slipstream swap");
    assert_eq!(event.quality, UpdateQuality::Exact);
    let actual_writes = event
        .updates
        .into_iter()
        .map(|update| match update {
            StateUpdate::Slot {
                address,
                slot,
                value,
            } => {
                assert_eq!(address, pool);
                (slot, value)
            }
            other => panic!("exact Slipstream replay emitted a non-slot update: {other:?}"),
        })
        .collect::<BTreeMap<_, _>>();
    let expected_writes = fixture["expected_writes"]
        .as_object()
        .expect("expected deployed-runtime writes")
        .iter()
        .map(|(slot, value)| {
            (
                word(slot),
                word(value.as_str().expect("expected write word")),
            )
        })
        .collect::<BTreeMap<_, _>>();
    for (slot, expected) in &expected_writes {
        assert_eq!(
            actual_writes.get(slot),
            Some(expected),
            "event transition must reproduce deployed-runtime write {slot}",
        );
    }
    for (slot, actual) in actual_writes {
        if !expected_writes.contains_key(&slot) {
            assert_eq!(
                Some(actual),
                state.storage(pool, slot),
                "an update omitted by the deployed write trace must be a parent-state no-op at {slot}",
            );
        }
    }

    let quote_amounts = if chain_id == 10 {
        [
            U256::from(1_000_000_u64),
            U256::from(10_000_000_000_000_000_u64),
        ]
    } else {
        [
            U256::from(1_000_000_000_000_000_u64),
            U256::from(1_000_000_000_000_000_u64),
        ]
    };
    let sample_count = std::env::var("SLIPSTREAM_E2E_SAMPLES")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|samples| *samples > 0)
        .unwrap_or(32);
    let mut end_to_end = Vec::with_capacity(sample_count);
    let mut expected_outputs = None;
    for _ in 0..sample_count {
        let quote_started = Instant::now();
        let token0_to_token1 = adapter
            .simulate_swap(
                &registration,
                &mut quote_cache,
                token0,
                token1,
                quote_amounts[0],
                &SimConfig::default(),
            )
            .expect("post-event token0-to-token1 full pool simulation");
        let token1_to_token0 = adapter
            .simulate_swap(
                &registration,
                &mut quote_cache,
                token1,
                token0,
                quote_amounts[1],
                &SimConfig::default(),
            )
            .expect("post-event token1-to-token0 full pool simulation");
        let outputs = [token0_to_token1.amount_out, token1_to_token0.amount_out];
        assert!(outputs.iter().all(|amount| !amount.is_zero()));
        if let Some(expected) = expected_outputs {
            assert_eq!(outputs, expected, "post-event quotes must be deterministic");
        } else {
            expected_outputs = Some(outputs);
        }
        end_to_end.push(event_elapsed + quote_started.elapsed());
    }
    end_to_end.sort_unstable();
    let percentile = |percent: usize| -> Duration {
        let index = (sample_count * percent).div_ceil(100).saturating_sub(1);
        end_to_end[index]
    };
    let p50 = percentile(50);
    let p95 = percentile(95);
    let p99 = percentile(99);
    let max = *end_to_end.last().expect("positive sample count");
    let outputs = expected_outputs.expect("at least one simulation sample");
    eprintln!(
        "Slipstream event-to-full-simulation chain={chain_id} family={family:?} block={block_number} samples={sample_count} event_apply={event_elapsed:?} token0_to_token1_out={} token1_to_token0_out={} p50={p50:?} p95={p95:?} p99={p99:?} max={max:?} provider_calls=0 invalidations=0 resyncs=0",
        outputs[0], outputs[1],
    );
    assert!(
        max < Duration::from_secs(1),
        "every event-to-full-simulation sample must stay below one second; max={max:?}",
    );
    assert_eq!(
        quote_provider.read_q().len(),
        1,
        "the untouched failure sentinel proves zero provider calls",
    );

    if let Some(sample_count) = std::env::var("SLIPSTREAM_PERF_SAMPLES")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|samples| *samples > 0)
    {
        let adapter = ConcentratedLiquidityAdapter::default();
        let mut samples = Vec::with_capacity(sample_count);
        for _ in 0..sample_count {
            let started = Instant::now();
            let decoded =
                adapter.decode_event_with_context(&registration, &log, &state, &base_context);
            samples.push(started.elapsed());
            assert_eq!(decoded.error, None);
            assert_eq!(
                decoded.event.expect("timed transition event").quality,
                UpdateQuality::Exact,
            );
        }
        samples.sort_unstable();
        let percentile = |percent: usize| -> Duration {
            let index = (sample_count * percent).div_ceil(100).saturating_sub(1);
            samples[index]
        };
        let p50 = percentile(50);
        let p95 = percentile(95);
        let p99 = percentile(99);
        let max = *samples.last().expect("positive sample count");
        eprintln!(
            "Slipstream event transition chain={chain_id} samples={sample_count} p50={p50:?} p95={p95:?} p99={p99:?} max={max:?} rpc_batch_elements=0 retries=0 fallbacks=0"
        );
        assert!(
            p95 < Duration::from_millis(10),
            "Slipstream event-transition p95 exceeded the 10ms decision gate: {p95:?}",
        );
    }
}

#[tokio::test]
async fn base_aerodrome_trace_proves_accounting_and_evidence_free_quote_paths() {
    assert_fixture(include_str!(
        "fixtures/base_slipstream_initialized_crossing.json"
    ))
    .await;
}

#[tokio::test]
async fn optimism_velodrome_trace_proves_accounting_and_evidence_free_quote_paths() {
    assert_fixture(include_str!(
        "fixtures/optimism_slipstream_initialized_crossing.json"
    ))
    .await;
}
