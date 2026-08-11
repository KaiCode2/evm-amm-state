use std::{collections::BTreeMap, str::FromStr, sync::Arc};

use alloy_eips::BlockId;
use alloy_network::Ethereum;
use alloy_primitives::{Address, B256, Bytes, Log, U256, keccak256};
use alloy_provider::{RootProvider, network::AnyNetwork};
use alloy_rpc_client::RpcClient;
use alloy_rpc_types_eth::Log as RpcLog;
use alloy_transport::mock::Asserter;
use evm_amm_state::adapters::{
    AdapterEventContext, AdapterEventError, AdapterRegistry, AmmAdapter, AmmPoolReactiveHandler,
    AmmReactiveRoutingContext, ConcentratedLiquidityAdapter, PoolGeneration, PoolInstanceId,
    PoolKey, PoolRegistration, ProtocolMetadata, PurgeScope, SlipstreamFeeEvidenceError,
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

fn runtime(hex_runtime: &str) -> Bytes {
    Bytes::from(
        alloy_primitives::hex::decode(hex_runtime.trim()).expect("checked-in deployed runtime"),
    )
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
                .with_tick_spacing(spacing)
                .with_storage_layout(V3StorageLayout::slipstream(spacing)),
        ));
    assert_eq!(
        ConcentratedLiquidityAdapter::swap_transition_capability_with_context(
            &registration,
            &context,
        ),
        V3SwapTransitionCapability::Unsupported,
        "reviewed evidence must not elevate Slipstream before the complete parity matrix",
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
                ReactiveInput::Log(rpc_log),
                reactive_context,
            )]),
        )
        .expect("real reactive handler fail-closes a Slipstream event");
    assert_eq!(report.applied.len(), 1);
    assert_eq!(
        report.applied[0].quality,
        StateEffectQuality::RequiresRepair,
    );
    assert_ne!(
        fee_cache.cached_storage_value(pool, U256::ZERO),
        Some(U256::from_be_slice(factory.as_slice())),
        "unsupported Slipstream must purge the stale parent state",
    );
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
    assert_eq!(
        decoded.error,
        Some(AdapterEventError::Unsupported(
            evm_amm_state::adapters::UnsupportedReason::Protocol(
                evm_amm_state::adapters::ProtocolId::Slipstream,
            )
        )),
    );
    let event = decoded.event.expect("recognized Slipstream swap");
    assert_eq!(event.quality, UpdateQuality::RequiresRepair);
    assert_eq!(
        event.updates,
        vec![StateUpdate::purge(pool, PurgeScope::AllStorage)],
    );
}

#[tokio::test]
async fn base_aerodrome_trace_pins_fee_evidence_and_publicly_fails_closed() {
    assert_fixture(include_str!(
        "fixtures/base_slipstream_initialized_crossing.json"
    ))
    .await;
}

#[tokio::test]
async fn optimism_velodrome_trace_pins_fee_evidence_and_publicly_fails_closed() {
    assert_fixture(include_str!(
        "fixtures/optimism_slipstream_initialized_crossing.json"
    ))
    .await;
}
