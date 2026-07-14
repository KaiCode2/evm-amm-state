use std::sync::{Arc, Mutex};

use alloy_eips::BlockId;
use alloy_network::Ethereum;
use alloy_primitives::{Address, B256, Bytes, Log as PrimitiveLog, U256, keccak256};
use alloy_provider::{RootProvider, network::AnyNetwork};
use alloy_rpc_client::RpcClient;
use alloy_rpc_types_eth::Log as RpcLog;
use alloy_transport::mock::Asserter;
use anyhow::Result;
use evm_amm_state::adapters::{
    AdapterEvent, AdapterEventKind, AdapterEventResult, AdapterRegistry, AmmAdapter,
    AmmChangeImpact, AmmPoolReactiveHandler, AmmSyncChangeSource, AmmSyncEngine, AmmSyncIncident,
    AmmSyncPoolChange, AmmSyncPoolChangeKind, BalancerTokenBalance, BalancerV2Adapter,
    BalancerV2Metadata, CurveAdapter, CurveMetadata, CurveVariant, CustomPoolKey, EventSource,
    OwnerRuntimeState, PoolGeneration, PoolInstanceId, PoolKey, PoolRegistration, PoolRuntimeState,
    PoolStateDependencies, PoolStatus, ProtocolId, ProtocolMetadata, PurgeScope,
    SolidlyStorageLayout, SolidlyV2Adapter, SolidlyV2Metadata, StateSlot, StateView, UpdateQuality,
};
use evm_fork_cache::cache::{
    BlockStateAccountDiff, BlockStateDiff, BlockStateStorageDiff, EvmCache,
};
use evm_fork_cache::reactive::{
    BlockRef, ChainStatus, InputSource, ReactiveConfig, ReactiveContext, ReactiveInput,
    ReactiveInputBatch, ReactiveInputRecord, ReactiveRuntime,
};
use evm_fork_cache::{StateUpdate, StorageFetchError};

fn block_hash(block_number: u64) -> B256 {
    B256::repeat_byte(block_number as u8)
}

fn included_context(block_number: u64) -> ReactiveContext {
    let block = BlockRef {
        number: block_number,
        hash: block_hash(block_number),
        parent_hash: Some(block_hash(block_number.saturating_sub(1))),
        timestamp: Some(1_700_000_000 + block_number),
    };

    ReactiveContext {
        chain_id: Some(1),
        source: InputSource::Synthetic,
        chain_status: ChainStatus::Included {
            block: block.clone(),
            confirmations: 0,
        },
        block: Some(block),
        transaction_index: Some(0),
        log_index: Some(0),
    }
}

fn rpc_log(address: Address, topics: Vec<B256>, data: Vec<u8>, block_number: u64) -> RpcLog {
    RpcLog {
        inner: PrimitiveLog::new_unchecked(address, topics, Bytes::from(data)),
        block_hash: Some(block_hash(block_number)),
        block_number: Some(block_number),
        block_timestamp: Some(1_700_000_000 + block_number),
        transaction_hash: Some(B256::repeat_byte(0x44)),
        transaction_index: Some(0),
        log_index: Some(0),
        removed: false,
    }
}

fn batch(log: RpcLog, block_number: u64) -> ReactiveInputBatch<Ethereum> {
    ReactiveInputBatch::new(vec![ReactiveInputRecord::new(
        ReactiveInput::Log(log),
        included_context(block_number),
    )])
}

fn reorged_batch(mut log: RpcLog, block_number: u64) -> ReactiveInputBatch<Ethereum> {
    let dropped_from = BlockRef {
        number: block_number,
        hash: block_hash(block_number),
        parent_hash: Some(block_hash(block_number.saturating_sub(1))),
        timestamp: Some(1_700_000_000 + block_number),
    };
    log.removed = true;
    ReactiveInputBatch::new(vec![ReactiveInputRecord::new(
        ReactiveInput::Log(log),
        ReactiveContext {
            chain_id: Some(1),
            source: InputSource::Synthetic,
            chain_status: ChainStatus::Reorged {
                dropped_from: dropped_from.clone(),
            },
            block: Some(dropped_from),
            transaction_index: Some(0),
            log_index: Some(0),
        },
    )])
}

async fn setup_cache() -> Result<EvmCache> {
    let asserter = Asserter::new();
    let client = RpcClient::mocked(asserter);
    let provider = RootProvider::<AnyNetwork>::new(client);
    Ok(EvmCache::new(Arc::new(provider)).await)
}

fn word(value: U256) -> Vec<u8> {
    value.to_be_bytes::<32>().to_vec()
}

fn abi_words(values: impl IntoIterator<Item = U256>) -> Vec<u8> {
    values.into_iter().flat_map(word).collect()
}

fn topic_address(address: Address) -> B256 {
    let mut bytes = [0u8; 32];
    bytes[12..].copy_from_slice(address.as_slice());
    B256::from(bytes)
}

fn curve_token_exchange_topic() -> B256 {
    keccak256("TokenExchange(address,int128,uint256,int128,uint256)")
}

fn balancer_pool_id(specialization: u16, seed: u8) -> B256 {
    let mut bytes = [seed; 32];
    bytes[20..22].copy_from_slice(&specialization.to_be_bytes());
    B256::from(bytes)
}

fn balancer_swap_topic() -> B256 {
    keccak256("Swap(bytes32,address,address,uint256,uint256)")
}

fn purge_one_topic() -> B256 {
    keccak256("PurgeOne()")
}

struct SlotPurgeAdapter;

const WRONG_OWNER_PROTOCOL: &str = "stage2-wrong-owner";

struct WrongOwnerAdapter {
    topic: B256,
    wrong_pool: PoolKey,
}

impl AmmAdapter for WrongOwnerAdapter {
    fn protocol(&self) -> ProtocolId {
        ProtocolId::Custom(WRONG_OWNER_PROTOCOL)
    }

    fn event_sources(&self, pool: &PoolRegistration) -> Vec<EventSource> {
        pool.key
            .address()
            .map(|address| EventSource::direct(address, vec![self.topic]))
            .into_iter()
            .collect()
    }

    fn decode_event(
        &self,
        _pool: &PoolRegistration,
        log: &PrimitiveLog,
        _view: &dyn StateView,
    ) -> AdapterEventResult {
        AdapterEventResult::event(AdapterEvent::new(
            self.wrong_pool.clone(),
            log.address,
            self.topic,
            AdapterEventKind::Unknown,
            UpdateQuality::RequiresRepair,
        ))
    }
}

impl AmmAdapter for SlotPurgeAdapter {
    fn protocol(&self) -> ProtocolId {
        ProtocolId::BalancerV2
    }

    fn event_sources(&self, pool: &PoolRegistration) -> Vec<EventSource> {
        let ProtocolMetadata::BalancerV2(metadata) = &pool.metadata else {
            return Vec::new();
        };
        metadata
            .pool_address
            .map(|address| EventSource::direct(address, vec![purge_one_topic()]))
            .into_iter()
            .collect()
    }

    fn state_dependencies(&self, pool: &PoolRegistration) -> PoolStateDependencies {
        let ProtocolMetadata::BalancerV2(metadata) = &pool.metadata else {
            return PoolStateDependencies::default();
        };
        let Some(vault) = metadata.vault else {
            return PoolStateDependencies::default();
        };
        PoolStateDependencies::default()
            .with_associated_addresses([vault])
            .with_slots(
                metadata
                    .balance_slots
                    .iter()
                    .copied()
                    .map(|slot| StateSlot::new(vault, slot)),
            )
    }

    fn decode_event(
        &self,
        pool: &PoolRegistration,
        log: &PrimitiveLog,
        _view: &dyn StateView,
    ) -> AdapterEventResult {
        let ProtocolMetadata::BalancerV2(metadata) = &pool.metadata else {
            return AdapterEventResult::ignored();
        };
        let Some(vault) = metadata.vault else {
            return AdapterEventResult::ignored();
        };
        if metadata.balance_slots.is_empty() {
            return AdapterEventResult::ignored();
        }
        AdapterEventResult::event(
            AdapterEvent::new(
                pool.key.clone(),
                log.address,
                purge_one_topic(),
                AdapterEventKind::Unknown,
                UpdateQuality::Exact,
            )
            .with_updates([evm_amm_state::adapters::StateUpdate::purge(
                vault,
                PurgeScope::Slots(metadata.balance_slots.clone()),
            )]),
        )
    }
}

fn balancer_registration(
    adapter: &Arc<BalancerV2Adapter>,
    vault: Address,
    pool_id: B256,
    slot: U256,
    token_in: Address,
    token_out: Address,
) -> PoolRegistration {
    let metadata = BalancerV2Metadata::default()
        .with_vault(vault)
        .with_tokens([token_in, token_out])
        .with_balance_slots([slot])
        .with_token_cash([
            BalancerTokenBalance::new(token_in, slot, false),
            BalancerTokenBalance::new(token_out, slot, true),
        ]);
    let mut registration = PoolRegistration::new(PoolKey::BalancerV2(pool_id))
        .with_state_address(vault)
        .with_status(PoolStatus::Ready)
        .with_metadata(ProtocolMetadata::BalancerV2(metadata));
    let sources = adapter.event_sources(&registration);
    registration = registration.with_event_sources(sources);
    registration
}

fn curve_log(pool: Address, buyer: Address, block: u64) -> RpcLog {
    rpc_log(
        pool,
        vec![curve_token_exchange_topic(), topic_address(buyer)],
        abi_words([
            U256::ZERO,
            U256::from(1_u64),
            U256::from(1_u64),
            U256::from(1_u64),
        ]),
        block,
    )
}

fn curve_registry_with_status(
    pool: Address,
    slots: Vec<U256>,
    status: PoolStatus,
) -> Result<AdapterRegistry> {
    let dai = Address::repeat_byte(0x01);
    let usdc = Address::repeat_byte(0x02);
    let mut registration = PoolRegistration::new(PoolKey::Curve(pool))
        .with_state_address(pool)
        .with_status(status)
        .with_metadata(ProtocolMetadata::Curve(
            CurveMetadata::default()
                .with_coins([dai, usdc])
                .with_discovered_slots(slots)
                .with_variant(CurveVariant::StableSwap),
        ));
    let adapter = CurveAdapter::default();
    let sources = adapter.event_sources(&registration);
    registration = registration.with_event_sources(sources);

    let mut registry = AdapterRegistry::new();
    registry.register_adapter(Arc::new(CurveAdapter::default()))?;
    registry.register_pool(registration)?;
    Ok(registry)
}

fn curve_registry(pool: Address, slot: U256) -> Result<AdapterRegistry> {
    curve_registry_with_status(pool, vec![slot], PoolStatus::Ready)
}

fn diff_for_slot(address: Address, slot: U256, value: U256) -> BlockStateDiff {
    BlockStateDiff {
        accounts: vec![BlockStateAccountDiff {
            address,
            balance: None,
            nonce: None,
            code: None,
            storage: vec![BlockStateStorageDiff { slot, value }],
        }],
    }
}

#[tokio::test]
async fn sync_engine_executes_trace_resync_without_storage_fallback() -> Result<()> {
    let pool = Address::repeat_byte(0xc1);
    let slot = U256::from(7);
    let fresh = U256::from(900_001_u64);
    let seen_trace_blocks = Arc::new(Mutex::new(Vec::new()));
    let mut cache = setup_cache().await?;
    cache.set_block_state_diff_fetcher({
        let seen_trace_blocks = seen_trace_blocks.clone();
        Arc::new(move |block| {
            seen_trace_blocks.lock().unwrap().push(block);
            Ok(diff_for_slot(pool, slot, fresh))
        })
    });
    cache.set_storage_batch_fetcher(Arc::new(|requests, _block| {
        panic!("storage fallback should not run when trace resolves all slots: {requests:?}")
    }));

    let registry = curve_registry(pool, slot)?;
    let mut engine = AmmSyncEngine::new(registry)?;

    let report = engine.ingest_batch(
        &mut cache,
        batch(curve_log(pool, Address::repeat_byte(0x01), 90), 90),
    )?;

    assert_eq!(report.reactive.applied.len(), 1);
    assert_eq!(report.resync_state_updates, 1);
    assert_eq!(report.resync_failures, 0);
    assert!(report.degraded_pools.is_empty());
    assert_eq!(report.affected_pools, vec![PoolKey::Curve(pool)]);
    assert_eq!(report.pool_changes.len(), 1);
    assert_eq!(
        report.pool_changes[0].source(),
        AmmSyncChangeSource::AuthoritativeResync,
        "the typed change is classified only after the resync update commits"
    );
    assert_eq!(cache.cached_storage_value(pool, slot), Some(fresh));
    assert_eq!(
        engine
            .registry()
            .pool(&PoolKey::Curve(pool))
            .expect("registered pool")
            .status,
        PoolStatus::Ready
    );

    let seen = seen_trace_blocks.lock().unwrap();
    assert_eq!(
        seen.len(),
        1,
        "one block trace should serve the event batch"
    );
    match &seen[0] {
        BlockId::Hash(hash) => {
            assert_eq!(hash.block_hash, block_hash(90));
            assert_eq!(hash.require_canonical, Some(true));
        }
        other => panic!("expected hash-pinned trace request, got {other:?}"),
    }

    Ok(())
}

#[tokio::test]
async fn successful_resync_with_unchanged_ready_state_is_not_a_pool_change() -> Result<()> {
    let pool = Address::repeat_byte(0xc0);
    let slot = U256::from(6);
    let current = U256::from(700_001_u64);
    let mut cache = setup_cache().await?;
    cache.apply_updates(&[StateUpdate::slot(pool, slot, current)]);
    cache.set_block_state_diff_fetcher(Arc::new(move |_block| {
        Ok(diff_for_slot(pool, slot, current))
    }));

    let registry = curve_registry(pool, slot)?;
    let mut engine = AmmSyncEngine::new(registry)?;
    let report = engine.ingest_batch(
        &mut cache,
        batch(curve_log(pool, Address::repeat_byte(0x01), 89), 89),
    )?;

    assert_eq!(report.resync_state_updates, 1);
    assert!(report.affected_pools.is_empty());
    assert!(report.pool_changes.is_empty());
    assert!(report.degraded_pools.is_empty());
    assert!(report.recovered_pools.is_empty());
    Ok(())
}

#[tokio::test]
async fn sync_engine_installs_one_handler_per_pool_and_executes_only_the_routed_owner() -> Result<()>
{
    let pool_a = Address::repeat_byte(0x81);
    let pool_b = Address::repeat_byte(0x82);
    let (slot_a0, slot_a1) = (U256::from(1), U256::from(2));
    let (slot_b0, slot_b1) = (U256::from(3), U256::from(4));
    let adapter = Arc::new(SolidlyV2Adapter::default());
    let mut registry = AdapterRegistry::new();
    registry.register_adapter(adapter.clone())?;
    registry.register_pool(solidly_registration(&adapter, pool_b, slot_b0, slot_b1))?;
    registry.register_pool(solidly_registration(&adapter, pool_a, slot_a0, slot_a1))?;

    let mut engine = AmmSyncEngine::new(registry)?;
    assert_eq!(
        engine.runtime().handler_ids(),
        vec![
            AmmPoolReactiveHandler::handler_id(&PoolInstanceId::new(
                PoolKey::SolidlyV2(pool_a),
                PoolGeneration::new(0),
            )),
            AmmPoolReactiveHandler::handler_id(&PoolInstanceId::new(
                PoolKey::SolidlyV2(pool_b),
                PoolGeneration::new(0),
            )),
        ],
        "handler order is canonical, not registry insertion order"
    );

    let mut cache = setup_cache().await?;
    let log = rpc_log(
        pool_a,
        vec![solidly_sync_topic()],
        abi_words([U256::from(10), U256::from(20)]),
        89,
    );
    let report = engine.ingest_batch(&mut cache, batch(log, 89))?;

    assert_eq!(report.reactive.applied.len(), 1);
    assert_eq!(
        report.reactive.applied[0].handler_id,
        AmmPoolReactiveHandler::handler_id(&PoolInstanceId::new(
            PoolKey::SolidlyV2(pool_a),
            PoolGeneration::new(0),
        ))
    );
    assert_eq!(
        cache.cached_storage_value(pool_a, slot_a0),
        Some(U256::from(10))
    );
    assert_eq!(cache.cached_storage_value(pool_b, slot_b0), None);
    Ok(())
}

#[tokio::test]
async fn pool_handler_identity_overrides_a_misattributed_adapter_event_payload() -> Result<()> {
    let topic = B256::repeat_byte(0x67);
    let actual = PoolKey::Custom(CustomPoolKey::Address {
        protocol: WRONG_OWNER_PROTOCOL,
        address: Address::repeat_byte(0x68),
    });
    let wrong = PoolKey::Custom(CustomPoolKey::Address {
        protocol: WRONG_OWNER_PROTOCOL,
        address: Address::repeat_byte(0x69),
    });
    let adapter = Arc::new(WrongOwnerAdapter {
        topic,
        wrong_pool: wrong.clone(),
    });
    let mut registry = AdapterRegistry::new();
    registry.register_adapter(adapter.clone())?;
    for key in [actual.clone(), wrong.clone()] {
        let mut registration = PoolRegistration::new(key).with_status(PoolStatus::Ready);
        registration.event_sources = adapter.event_sources(&registration);
        registry.register_pool(registration)?;
    }

    let mut cache = setup_cache().await?;
    let log = rpc_log(actual.address().unwrap(), vec![topic], Vec::new(), 61);
    let mut engine = AmmSyncEngine::new(registry)?;
    let report = engine.ingest_batch(&mut cache, batch(log, 61))?;

    assert_eq!(report.degraded_pools, vec![actual.clone()]);
    assert_eq!(
        engine.registry().pool(&actual).unwrap().status,
        PoolStatus::Degraded
    );
    assert_eq!(
        engine.registry().pool(&wrong).unwrap().status,
        PoolStatus::Ready
    );
    Ok(())
}

#[tokio::test]
async fn old_generation_handler_id_cannot_unregister_or_alias_its_replacement() -> Result<()> {
    let pool = Address::repeat_byte(0x83);
    let key = PoolKey::SolidlyV2(pool);
    let adapter = Arc::new(SolidlyV2Adapter::default());
    let mut registry = AdapterRegistry::new();
    registry.register_adapter(adapter.clone())?;
    registry.register_pool(solidly_registration(
        &adapter,
        pool,
        U256::from(1),
        U256::from(2),
    ))?;
    let registry = Arc::new(registry);
    let old_instance = PoolInstanceId::new(key.clone(), PoolGeneration::new(1));
    let replacement_instance = PoolInstanceId::new(key, PoolGeneration::new(2));
    let old_id = AmmPoolReactiveHandler::handler_id(&old_instance);
    let replacement_id = AmmPoolReactiveHandler::handler_id(&replacement_instance);
    assert_ne!(old_id, replacement_id);

    let old = Arc::new(AmmPoolReactiveHandler::new(registry.clone(), old_instance)?);
    let replacement = Arc::new(AmmPoolReactiveHandler::new(registry, replacement_instance)?);
    let mut runtime = ReactiveRuntime::<Ethereum>::new(ReactiveConfig::default());
    runtime.register_handler(old)?;
    runtime.register_handler(replacement)?;
    assert!(runtime.unregister_handler(&old_id).is_some());
    assert!(runtime.contains_handler(&replacement_id));

    let mut cache = setup_cache().await?;
    let log = rpc_log(
        pool,
        vec![solidly_sync_topic()],
        abi_words([U256::from(30), U256::from(40)]),
        88,
    );
    let report = runtime.ingest_batch(&mut cache, batch(log, 88))?;
    assert_eq!(report.applied.len(), 1);
    assert_eq!(report.applied[0].handler_id, replacement_id);
    Ok(())
}

#[tokio::test]
async fn sync_engine_marks_pool_degraded_when_resync_fails() -> Result<()> {
    let pool = Address::repeat_byte(0xc2);
    let slot = U256::from(8);
    let mut cache = setup_cache().await?;
    cache.set_block_state_diff_fetcher(Arc::new(|_block| {
        Ok(BlockStateDiff {
            accounts: Vec::new(),
        })
    }));
    cache.set_storage_batch_fetcher(Arc::new(|requests, _block| {
        requests
            .into_iter()
            .map(|(address, slot)| {
                (
                    address,
                    slot,
                    Err(StorageFetchError::custom("forced resync failure")),
                )
            })
            .collect()
    }));

    let registry = curve_registry(pool, slot)?;
    let mut engine = AmmSyncEngine::new(registry)?;
    let instance = engine
        .ownership()
        .active_pool(&PoolKey::Curve(pool))
        .cloned()
        .expect("pool instance");

    let report = engine.ingest_batch(
        &mut cache,
        batch(curve_log(pool, Address::repeat_byte(0x01), 91), 91),
    )?;

    assert_eq!(report.reactive.applied.len(), 1);
    assert_eq!(report.resync_state_updates, 0);
    assert_eq!(report.resync_failures, 1);
    assert_eq!(report.degraded_pools, vec![PoolKey::Curve(pool)]);
    assert_eq!(report.affected_pools, vec![PoolKey::Curve(pool)]);
    assert_eq!(report.pool_changes.len(), 1);
    assert_eq!(
        report.pool_changes[0].kind(),
        AmmSyncPoolChangeKind::Degraded
    );
    assert_eq!(cache.cached_storage_value(pool, slot), None);
    assert_eq!(
        engine
            .registry()
            .pool(&PoolKey::Curve(pool))
            .expect("registered pool")
            .status,
        PoolStatus::Degraded
    );
    assert_eq!(
        engine.lifecycles().pool(&instance),
        Some(PoolRuntimeState::Degraded)
    );

    Ok(())
}

#[tokio::test]
async fn sync_engine_recovers_degraded_pool_after_successful_resync() -> Result<()> {
    let pool = Address::repeat_byte(0xc3);
    let slot = U256::from(9);
    let fresh = U256::from(1_234_567_u64);
    let mut cache = setup_cache().await?;
    cache
        .set_block_state_diff_fetcher(Arc::new(move |_block| Ok(diff_for_slot(pool, slot, fresh))));

    // A pool previously marked Degraded (e.g. a transient resync failure)
    // whose next event resync succeeds must flip back to Ready.
    let registry = curve_registry_with_status(pool, vec![slot], PoolStatus::Degraded)?;
    let mut engine = AmmSyncEngine::new(registry)?;
    let instance = engine
        .ownership()
        .active_pool(&PoolKey::Curve(pool))
        .cloned()
        .expect("pool instance");

    let report = engine.ingest_batch(
        &mut cache,
        batch(curve_log(pool, Address::repeat_byte(0x01), 93), 93),
    )?;

    assert_eq!(report.resync_failures, 0);
    assert!(report.degraded_pools.is_empty());
    assert_eq!(report.recovered_pools, vec![PoolKey::Curve(pool)]);
    assert_eq!(report.affected_pools, vec![PoolKey::Curve(pool)]);
    assert_eq!(report.pool_changes.len(), 1);
    assert_eq!(
        report.pool_changes[0].kind(),
        AmmSyncPoolChangeKind::Recovered
    );
    assert_eq!(
        report.pool_changes[0].source(),
        AmmSyncChangeSource::AuthoritativeResync
    );
    assert_eq!(cache.cached_storage_value(pool, slot), Some(fresh));
    assert_eq!(
        engine
            .registry()
            .pool(&PoolKey::Curve(pool))
            .expect("registered pool")
            .status,
        PoolStatus::Ready
    );
    assert_eq!(
        engine.lifecycles().pool(&instance),
        Some(PoolRuntimeState::Searchable)
    );

    Ok(())
}

#[tokio::test]
async fn explicit_replacement_does_not_inherit_old_generation_recovery_targets() -> Result<()> {
    let pool = Address::repeat_byte(0xc4);
    let old_slot = U256::from(91);
    let replacement_slot = U256::from(92);
    let fresh = U256::from(4242);
    let mut cache = setup_cache().await?;
    cache.set_block_state_diff_fetcher(Arc::new(|_block| {
        Ok(BlockStateDiff {
            accounts: Vec::new(),
        })
    }));
    cache.set_storage_batch_fetcher(Arc::new(|requests, _block| {
        requests
            .into_iter()
            .map(|(address, slot)| {
                (
                    address,
                    slot,
                    Err(StorageFetchError::custom("forced old-generation failure")),
                )
            })
            .collect()
    }));
    let mut engine = AmmSyncEngine::new(curve_registry(pool, old_slot)?)?;
    engine.ingest_batch(
        &mut cache,
        batch(curve_log(pool, Address::repeat_byte(0x01), 93), 93),
    )?;

    engine.replace_registry(curve_registry_with_status(
        pool,
        vec![replacement_slot],
        PoolStatus::Degraded,
    )?)?;
    cache.set_block_state_diff_fetcher(Arc::new(move |_block| {
        Ok(diff_for_slot(pool, replacement_slot, fresh))
    }));
    let report = engine.ingest_batch(
        &mut cache,
        batch(curve_log(pool, Address::repeat_byte(0x01), 94), 94),
    )?;

    assert_eq!(report.recovered_pools, vec![PoolKey::Curve(pool)]);
    assert_eq!(
        cache.cached_storage_value(pool, replacement_slot),
        Some(fresh)
    );
    Ok(())
}

#[tokio::test]
async fn unchanged_authoritative_resync_reports_recovery_without_false_state_change() -> Result<()>
{
    let pool = Address::repeat_byte(0xc6);
    let slot = U256::from(10);
    let current = U256::from(1_111_111_u64);
    let mut cache = setup_cache().await?;
    cache.apply_updates(&[StateUpdate::slot(pool, slot, current)]);
    cache.set_block_state_diff_fetcher(Arc::new(move |_block| {
        Ok(diff_for_slot(pool, slot, current))
    }));

    let registry = curve_registry_with_status(pool, vec![slot], PoolStatus::Degraded)?;
    let mut engine = AmmSyncEngine::new(registry)?;
    let report = engine.ingest_batch(
        &mut cache,
        batch(curve_log(pool, Address::repeat_byte(0x01), 93), 93),
    )?;

    assert_eq!(report.recovered_pools, vec![PoolKey::Curve(pool)]);
    assert_eq!(report.pool_changes.len(), 1);
    assert_eq!(
        report.pool_changes[0].kind(),
        AmmSyncPoolChangeKind::Recovered
    );
    assert_eq!(
        report.pool_changes[0].impact(),
        AmmChangeImpact::quoteability(),
        "an unchanged verification must not claim a state mutation"
    );
    assert_eq!(
        report.pool_changes[0].source(),
        AmmSyncChangeSource::AuthoritativeResync,
        "authoritative verification is what justified recovery"
    );
    Ok(())
}

#[tokio::test]
async fn sync_engine_leaves_degraded_pool_without_read_set_untouched() -> Result<()> {
    let pool = Address::repeat_byte(0xc4);
    let mut cache = setup_cache().await?;
    cache.set_block_state_diff_fetcher(Arc::new(|_block| {
        Ok(BlockStateDiff {
            accounts: Vec::new(),
        })
    }));

    // Degraded from a failed cold-start: no discovered slots, so its events
    // carry no repair and no resync can vouch for it — it must stay Degraded.
    let registry = curve_registry_with_status(pool, Vec::new(), PoolStatus::Degraded)?;
    let mut engine = AmmSyncEngine::new(registry)?;

    let report = engine.ingest_batch(
        &mut cache,
        batch(curve_log(pool, Address::repeat_byte(0x01), 94), 94),
    )?;

    assert_eq!(report.reactive.applied.len(), 1);
    assert_eq!(report.resync_state_updates, 0);
    assert!(report.recovered_pools.is_empty());
    assert_eq!(
        engine
            .registry()
            .pool(&PoolKey::Curve(pool))
            .expect("registered pool")
            .status,
        PoolStatus::Degraded
    );

    Ok(())
}

#[tokio::test]
async fn sync_engine_degrades_unrepairable_routed_event_instead_of_reporting_clean_noop()
-> Result<()> {
    let pool = Address::repeat_byte(0xc5);
    let mut cache = setup_cache().await?;
    let registry = curve_registry_with_status(pool, Vec::new(), PoolStatus::Ready)?;
    let mut engine = AmmSyncEngine::new(registry)?;

    // Curve recognizes this event as state-changing, but the empty read set
    // leaves it with no executable authoritative repair target.
    let report = engine.ingest_batch(
        &mut cache,
        batch(curve_log(pool, Address::repeat_byte(0x01), 95), 95),
    )?;

    assert_eq!(report.resync_state_updates, 0);
    assert_eq!(report.resync_failures, 0);
    assert_eq!(report.affected_pools, vec![PoolKey::Curve(pool)]);
    assert_eq!(report.degraded_pools, vec![PoolKey::Curve(pool)]);
    assert_eq!(report.pool_changes.len(), 1);
    assert_eq!(
        report.pool_changes[0].kind(),
        AmmSyncPoolChangeKind::Degraded
    );
    assert_eq!(
        report.pool_changes[0].source(),
        AmmSyncChangeSource::Unknown
    );
    assert_eq!(
        engine
            .registry()
            .pool(&PoolKey::Curve(pool))
            .expect("registered pool")
            .status,
        PoolStatus::Degraded
    );
    Ok(())
}

#[tokio::test]
async fn unknown_impact_cannot_auto_recover_from_a_later_partial_resync() -> Result<()> {
    let pool = Address::repeat_byte(0xc7);
    let slot = U256::from(11);
    let fresh = U256::from(9_999_u64);
    let mut cache = setup_cache().await?;
    cache
        .set_block_state_diff_fetcher(Arc::new(move |_block| Ok(diff_for_slot(pool, slot, fresh))));
    let registry = curve_registry(pool, slot)?;
    let mut engine = AmmSyncEngine::new(registry)?;

    let malformed = rpc_log(
        pool,
        vec![
            curve_token_exchange_topic(),
            topic_address(Address::repeat_byte(0x01)),
        ],
        abi_words([U256::ZERO]),
        95,
    );
    let degraded = engine.ingest_batch(&mut cache, batch(malformed, 95))?;
    assert_eq!(degraded.degraded_pools, vec![PoolKey::Curve(pool)]);

    let repaired_slot = engine.ingest_batch(
        &mut cache,
        batch(curve_log(pool, Address::repeat_byte(0x01), 96), 96),
    )?;
    assert_eq!(repaired_slot.resync_state_updates, 1);
    assert!(repaired_slot.recovered_pools.is_empty());
    assert_eq!(
        engine
            .registry()
            .pool(&PoolKey::Curve(pool))
            .expect("registered pool")
            .status,
        PoolStatus::Degraded,
        "unknown-impact trust loss needs an explicit verified refresh fence"
    );
    Ok(())
}

#[tokio::test]
async fn sync_engine_degrades_only_pool_that_owns_failed_shared_slot() -> Result<()> {
    let slot_a = U256::from(1);
    let slot_b = U256::from(2);
    let pool_a = Address::repeat_byte(0xa1);
    let pool_b = Address::repeat_byte(0xb1);
    let mut registry = curve_registry(pool_a, slot_a)?;
    let extra = curve_registry(pool_b, slot_b)?;
    registry.register_pool(
        extra
            .pool(&PoolKey::Curve(pool_b))
            .expect("extra pool")
            .clone(),
    )?;

    let mut cache = setup_cache().await?;
    cache.set_block_state_diff_fetcher(Arc::new(|_block| {
        Ok(BlockStateDiff {
            accounts: Vec::new(),
        })
    }));
    cache.set_storage_batch_fetcher(Arc::new(move |requests, _block| {
        requests
            .into_iter()
            .map(|(address, slot)| {
                let result = if address == pool_a && slot == slot_a {
                    Err(StorageFetchError::custom("forced pool-a failure"))
                } else {
                    Ok(U256::from(10))
                };
                (address, slot, result)
            })
            .collect()
    }));

    let mut engine = AmmSyncEngine::new(registry)?;
    let report = engine.ingest_batch(
        &mut cache,
        batch(curve_log(pool_a, Address::repeat_byte(0x01), 92), 92),
    )?;

    assert_eq!(report.degraded_pools, vec![PoolKey::Curve(pool_a)]);
    assert_eq!(
        engine
            .registry()
            .pool(&PoolKey::Curve(pool_a))
            .expect("pool a")
            .status,
        PoolStatus::Degraded
    );
    assert_eq!(
        engine
            .registry()
            .pool(&PoolKey::Curve(pool_b))
            .expect("pool b")
            .status,
        PoolStatus::Ready
    );

    Ok(())
}

#[tokio::test]
async fn sync_report_propagates_shared_slot_change_to_every_owner() -> Result<()> {
    let vault = Address::repeat_byte(0xe5);
    let shared_slot = U256::from(0x77_u64);
    let pool_a = balancer_pool_id(2, 0xa1);
    let pool_b = balancer_pool_id(2, 0xb1);
    let token_a0 = Address::repeat_byte(0x01);
    let token_a1 = Address::repeat_byte(0x02);
    let token_b0 = Address::repeat_byte(0x03);
    let token_b1 = Address::repeat_byte(0x04);

    let adapter = Arc::new(BalancerV2Adapter::default());
    let mut registry = AdapterRegistry::new();
    registry.register_adapter(adapter.clone())?;
    registry.register_pool(balancer_registration(
        &adapter,
        vault,
        pool_a,
        shared_slot,
        token_a0,
        token_a1,
    ))?;
    registry.register_pool(balancer_registration(
        &adapter,
        vault,
        pool_b,
        shared_slot,
        token_b0,
        token_b1,
    ))?;

    let mut cache = setup_cache().await?;
    let warm = (U256::from(7_u64) << 224) | (U256::from(1_000_u64) << 112) | U256::from(500_u64);
    cache.apply_updates(&[StateUpdate::slot(vault, shared_slot, warm)]);

    let log = rpc_log(
        vault,
        vec![
            balancer_swap_topic(),
            pool_b,
            topic_address(token_b0),
            topic_address(token_b1),
        ],
        abi_words([U256::from(30_u64), U256::from(20_u64)]),
        96,
    );
    let mut engine = AmmSyncEngine::new(registry)?;
    let report = engine.ingest_batch(&mut cache, batch(log, 96))?;

    assert_eq!(report.resync_state_updates, 0);
    assert_eq!(
        report.affected_pools,
        vec![PoolKey::BalancerV2(pool_a), PoolKey::BalancerV2(pool_b)],
        "a typed routed event identifies B, while the committed shared-slot diff also affects A"
    );
    Ok(())
}

#[tokio::test]
async fn failed_shared_slot_resync_is_attributed_to_the_requesting_pool_handler() -> Result<()> {
    let vault = Address::repeat_byte(0xe3);
    let shared_slot = U256::from(0x73_u64);
    let pool_a = balancer_pool_id(2, 0xa3);
    let pool_b = balancer_pool_id(2, 0xb3);
    let token_a0 = Address::repeat_byte(0x21);
    let token_a1 = Address::repeat_byte(0x22);
    let token_b0 = Address::repeat_byte(0x23);
    let token_b1 = Address::repeat_byte(0x24);
    let adapter = Arc::new(BalancerV2Adapter::default());
    let mut registry = AdapterRegistry::new();
    registry.register_adapter(adapter.clone())?;
    registry.register_pool(balancer_registration(
        &adapter,
        vault,
        pool_a,
        shared_slot,
        token_a0,
        token_a1,
    ))?;
    registry.register_pool(balancer_registration(
        &adapter,
        vault,
        pool_b,
        shared_slot,
        token_b0,
        token_b1,
    ))?;

    let mut cache = setup_cache().await?;
    cache.set_block_state_diff_fetcher(Arc::new(|_block| {
        Ok(BlockStateDiff {
            accounts: Vec::new(),
        })
    }));
    cache.set_storage_batch_fetcher(Arc::new(|requests, _block| {
        requests
            .into_iter()
            .map(|(address, slot)| {
                (
                    address,
                    slot,
                    Err(StorageFetchError::custom("forced shared-slot failure")),
                )
            })
            .collect()
    }));
    let log = rpc_log(
        vault,
        vec![
            balancer_swap_topic(),
            pool_b,
            topic_address(token_b0),
            topic_address(token_b1),
        ],
        abi_words([U256::from(30_u64), U256::from(20_u64)]),
        95,
    );
    let mut engine = AmmSyncEngine::new(registry)?;
    let report = engine.ingest_batch(&mut cache, batch(log, 95))?;

    assert_eq!(report.degraded_pools, vec![PoolKey::BalancerV2(pool_b)]);
    assert_eq!(
        engine
            .registry()
            .pool(&PoolKey::BalancerV2(pool_a))
            .unwrap()
            .status,
        PoolStatus::Ready
    );
    let request = &report.reactive.applied[0].resyncs[0].id;
    assert!(
        engine.ownership().resync_owner(request).is_none(),
        "synchronous completion clears the active resync ownership ledger"
    );
    Ok(())
}

#[test]
fn balancer_dependencies_scope_vault_slots_but_cover_the_pool_contract() -> Result<()> {
    let vault = Address::repeat_byte(0xe4);
    let pool_address = Address::repeat_byte(0xc4);
    let pool_id = balancer_pool_id(2, 0xc4);
    let balance_slot = U256::from(0x44_u64);
    let adapter = Arc::new(BalancerV2Adapter::default());
    let mut registry = AdapterRegistry::new();
    registry.register_adapter(adapter.clone())?;
    let mut registration = PoolRegistration::new(PoolKey::BalancerV2(pool_id))
        .with_state_address(vault)
        .with_metadata(ProtocolMetadata::BalancerV2(
            BalancerV2Metadata::default()
                .with_vault(vault)
                .with_pool_address(pool_address)
                .with_balance_slots([balance_slot]),
        ));
    registration.event_sources = adapter.event_sources(&registration);
    registry.register_pool(registration)?;

    let engine = AmmSyncEngine::new(registry)?;
    let instance = PoolInstanceId::new(PoolKey::BalancerV2(pool_id), PoolGeneration::new(0));
    assert_eq!(
        engine
            .ownership()
            .pools_for_slot(StateSlot::new(vault, balance_slot)),
        vec![instance.clone()]
    );
    assert!(
        engine
            .ownership()
            .pools_for_slot(StateSlot::new(vault, U256::from(999)))
            .is_empty(),
        "an unknown shared-vault slot is not owned by every pool"
    );
    assert_eq!(
        engine
            .ownership()
            .pools_for_slot(StateSlot::new(pool_address, U256::from(999))),
        vec![instance],
        "the exclusive pool contract is a whole-account dependency"
    );
    Ok(())
}

#[tokio::test]
async fn sync_report_uses_pool_id_to_isolate_distinct_state_on_a_shared_emitter() -> Result<()> {
    let vault = Address::repeat_byte(0xe6);
    let slot_a = U256::from(0x70_u64);
    let slot_b = U256::from(0x71_u64);
    let pool_a = balancer_pool_id(2, 0xa2);
    let pool_b = balancer_pool_id(2, 0xb2);
    let token_a0 = Address::repeat_byte(0x11);
    let token_a1 = Address::repeat_byte(0x12);
    let token_b0 = Address::repeat_byte(0x13);
    let token_b1 = Address::repeat_byte(0x14);

    let adapter = Arc::new(BalancerV2Adapter::default());
    let mut registry = AdapterRegistry::new();
    registry.register_adapter(adapter.clone())?;
    registry.register_pool(balancer_registration(
        &adapter, vault, pool_a, slot_a, token_a0, token_a1,
    ))?;
    registry.register_pool(balancer_registration(
        &adapter, vault, pool_b, slot_b, token_b0, token_b1,
    ))?;

    let mut cache = setup_cache().await?;
    let warm_a = (U256::from(7_u64) << 224) | (U256::from(1_000_u64) << 112) | U256::from(500_u64);
    let warm_b = (U256::from(8_u64) << 224) | (U256::from(2_000_u64) << 112) | U256::from(800_u64);
    cache.apply_updates(&[
        StateUpdate::slot(vault, slot_a, warm_a),
        StateUpdate::slot(vault, slot_b, warm_b),
    ]);

    let log = rpc_log(
        vault,
        vec![
            balancer_swap_topic(),
            pool_b,
            topic_address(token_b0),
            topic_address(token_b1),
        ],
        abi_words([U256::from(30_u64), U256::from(20_u64)]),
        97,
    );
    let mut engine = AmmSyncEngine::new(registry)?;
    let report = engine.ingest_batch(&mut cache, batch(log, 97))?;

    assert_eq!(report.reactive.applied.len(), 1);
    assert_eq!(
        report.reactive.applied[0].handler_id,
        AmmPoolReactiveHandler::handler_id(&PoolInstanceId::new(
            PoolKey::BalancerV2(pool_b),
            PoolGeneration::new(0),
        ))
    );
    assert_eq!(report.affected_pools, vec![PoolKey::BalancerV2(pool_b)]);
    assert_eq!(report.pool_changes.len(), 1);
    assert_eq!(report.pool_changes[0].pool(), &PoolKey::BalancerV2(pool_b));
    assert_eq!(report.pool_changes[0].source(), AmmSyncChangeSource::Direct);
    assert_eq!(cache.cached_storage_value(vault, slot_a), Some(warm_a));
    Ok(())
}

#[tokio::test]
async fn slot_scoped_purge_affects_only_shared_address_pools_that_cover_the_slot() -> Result<()> {
    let vault = Address::repeat_byte(0xe7);
    let emitter_a = Address::repeat_byte(0xa7);
    let emitter_b = Address::repeat_byte(0xb7);
    let slot_a = U256::from(0x80_u64);
    let slot_b = U256::from(0x81_u64);
    let pool_a = balancer_pool_id(2, 0xa7);
    let pool_b = balancer_pool_id(2, 0xb7);
    let adapter = Arc::new(SlotPurgeAdapter);
    let mut registry = AdapterRegistry::new();
    registry.register_adapter(adapter.clone())?;

    for (pool, emitter, slot) in [(pool_a, emitter_a, slot_a), (pool_b, emitter_b, slot_b)] {
        let mut registration = PoolRegistration::new(PoolKey::BalancerV2(pool))
            .with_state_address(vault)
            .with_status(PoolStatus::Ready)
            .with_metadata(ProtocolMetadata::BalancerV2(
                BalancerV2Metadata::default()
                    .with_vault(vault)
                    .with_pool_address(emitter)
                    .with_balance_slots([slot]),
            ));
        let sources = adapter.event_sources(&registration);
        registration = registration.with_event_sources(sources);
        registry.register_pool(registration)?;
    }

    let mut cache = setup_cache().await?;
    cache.apply_updates(&[
        StateUpdate::slot(vault, slot_a, U256::from(100)),
        StateUpdate::slot(vault, slot_b, U256::from(200)),
    ]);
    let log = rpc_log(emitter_a, vec![purge_one_topic()], Vec::new(), 98);
    let mut engine = AmmSyncEngine::new(registry)?;
    let report = engine.ingest_batch(&mut cache, batch(log, 98))?;

    assert_eq!(report.affected_pools, vec![PoolKey::BalancerV2(pool_a)]);
    assert_eq!(cache.cached_storage_value(vault, slot_a), None);
    assert_eq!(
        cache.cached_storage_value(vault, slot_b),
        Some(U256::from(200))
    );

    let no_op = rpc_log(emitter_a, vec![purge_one_topic()], Vec::new(), 99);
    let no_op_report = engine.ingest_batch(&mut cache, batch(no_op, 99))?;
    assert!(
        no_op_report.affected_pools.is_empty(),
        "re-purging an absent slot is not a state change"
    );
    Ok(())
}

#[tokio::test]
async fn partially_effective_multi_slot_purge_degrades_every_candidate_owner() -> Result<()> {
    let vault = Address::repeat_byte(0xe8);
    let emitter_a = Address::repeat_byte(0xa8);
    let emitter_b = Address::repeat_byte(0xb8);
    let slot_a = U256::from(0x90_u64);
    let slot_b = U256::from(0x91_u64);
    let pool_a = balancer_pool_id(2, 0xa8);
    let pool_b = balancer_pool_id(2, 0xb8);
    let adapter = Arc::new(SlotPurgeAdapter);
    let mut registry = AdapterRegistry::new();
    registry.register_adapter(adapter.clone())?;

    for (pool, emitter, slots) in [
        (pool_a, emitter_a, vec![slot_a, slot_b]),
        (pool_b, emitter_b, vec![slot_b]),
    ] {
        let mut registration = PoolRegistration::new(PoolKey::BalancerV2(pool))
            .with_state_address(vault)
            .with_status(PoolStatus::Ready)
            .with_metadata(ProtocolMetadata::BalancerV2(
                BalancerV2Metadata::default()
                    .with_vault(vault)
                    .with_pool_address(emitter)
                    .with_balance_slots(slots),
            ));
        registration.event_sources = adapter.event_sources(&registration);
        registry.register_pool(registration)?;
    }

    let mut cache = setup_cache().await?;
    cache.apply_updates(&[StateUpdate::slot(vault, slot_a, U256::from(100))]);
    let mut engine = AmmSyncEngine::new(registry)?;
    let report = engine.ingest_batch(
        &mut cache,
        batch(
            rpc_log(emitter_a, vec![purge_one_topic()], Vec::new(), 99),
            99,
        ),
    )?;

    assert_eq!(
        report.degraded_pools,
        vec![PoolKey::BalancerV2(pool_a), PoolKey::BalancerV2(pool_b)]
    );
    assert!(report.pool_changes.iter().all(|change| {
        change.kind() == AmmSyncPoolChangeKind::Degraded
            && change.source() == AmmSyncChangeSource::Unknown
    }));
    assert_eq!(
        engine
            .registry()
            .pool(&PoolKey::BalancerV2(pool_a))
            .unwrap()
            .status,
        PoolStatus::Degraded
    );
    assert_eq!(
        engine
            .registry()
            .pool(&PoolKey::BalancerV2(pool_b))
            .unwrap()
            .status,
        PoolStatus::Degraded
    );
    Ok(())
}

#[tokio::test]
async fn sync_engine_registers_and_unregisters_pools_mid_lifecycle() -> Result<()> {
    let pool_a = Address::repeat_byte(0xd1);
    let slot_a = U256::from(11);
    let pool_b = Address::repeat_byte(0xd2);
    let slot_b = U256::from(12);
    let fresh = U256::from(777_u64);

    let mut cache = setup_cache().await?;
    cache.set_block_state_diff_fetcher(Arc::new(move |_block| {
        Ok(diff_for_slot(pool_b, slot_b, fresh))
    }));

    // The engine starts tracking only pool A.
    let registry = curve_registry(pool_a, slot_a)?;
    let mut engine = AmmSyncEngine::new(registry)?;

    // A pool-B event before registration is not routed: no resync executes.
    let report = engine.ingest_batch(
        &mut cache,
        batch(curve_log(pool_b, Address::repeat_byte(0x01), 95), 95),
    )?;
    assert_eq!(report.resync_state_updates, 0);
    assert_eq!(cache.cached_storage_value(pool_b, slot_b), None);

    // Register pool B mid-lifecycle; its events now execute resyncs.
    let registration = curve_registry(pool_b, slot_b)?
        .pool(&PoolKey::Curve(pool_b))
        .cloned()
        .expect("built registration");
    engine.register_pools([registration])?;
    let report = engine.ingest_batch(
        &mut cache,
        batch(curve_log(pool_b, Address::repeat_byte(0x01), 96), 96),
    )?;
    assert_eq!(report.resync_state_updates, 1);
    assert_eq!(cache.cached_storage_value(pool_b, slot_b), Some(fresh));

    // Unregister pool B: the registration comes back, tracking stops, and the
    // warmed value stays (no eviction requested).
    let removed = engine.unregister_pools(std::slice::from_ref(&PoolKey::Curve(pool_b)))?;
    assert_eq!(removed.len(), 1);
    assert_eq!(removed[0].key, PoolKey::Curve(pool_b));
    assert!(engine.registry().pool(&PoolKey::Curve(pool_b)).is_none());
    assert_eq!(cache.cached_storage_value(pool_b, slot_b), Some(fresh));

    let report = engine.ingest_batch(
        &mut cache,
        batch(curve_log(pool_b, Address::repeat_byte(0x01), 97), 97),
    )?;
    assert_eq!(report.resync_state_updates, 0);

    // Unregistering an unknown key is a clean no-op (no rebuild, empty vec).
    assert!(
        engine
            .unregister_pools(std::slice::from_ref(&PoolKey::Curve(pool_b)))?
            .is_empty()
    );
    Ok(())
}

#[tokio::test]
async fn sync_engine_adds_a_pool_in_place_without_losing_runtime_continuity() -> Result<()> {
    let pool_a = Address::repeat_byte(0xd3);
    let pool_b = Address::repeat_byte(0xd4);
    let slot = U256::from(31);
    let mut cache = setup_cache().await?;
    let mut engine = AmmSyncEngine::new(curve_registry(pool_a, slot)?)?;

    engine.ingest_batch(
        &mut cache,
        batch(curve_log(pool_a, Address::repeat_byte(0x01), 110), 110),
    )?;
    let head = engine.runtime().last_canonical_block();
    let existing_handlers = engine.runtime().handler_ids();

    let registration = curve_registry(pool_b, slot)?
        .pool(&PoolKey::Curve(pool_b))
        .cloned()
        .expect("built registration");
    let report = engine.add_pools([registration])?;

    let instance = report
        .registered_pools()
        .first()
        .expect("one accepted pool");
    assert_eq!(report.registered_pools().len(), 1);
    assert_eq!(instance.key(), &PoolKey::Curve(pool_b));
    assert_eq!(instance.generation(), PoolGeneration::new(0));
    assert_eq!(engine.runtime().last_canonical_block(), head);
    assert_eq!(
        engine.runtime().handler_ids().len(),
        existing_handlers.len() + 1
    );
    assert!(
        existing_handlers
            .iter()
            .all(|handler| engine.runtime().handler_ids().contains(handler))
    );
    assert_eq!(
        engine.lifecycles().pool(instance),
        Some(PoolRuntimeState::Searchable)
    );
    let acknowledged = engine.acknowledge_live_delivery(std::slice::from_ref(instance))?;
    assert_eq!(acknowledged, vec![instance.clone()]);
    assert_eq!(
        engine.lifecycles().pool(instance),
        Some(PoolRuntimeState::Live)
    );
    assert!(
        engine
            .acknowledge_live_delivery(std::slice::from_ref(instance))?
            .is_empty(),
        "repeated subscriber acknowledgement is idempotent"
    );
    Ok(())
}

#[test]
fn sync_engine_previews_exact_subscriber_owners_without_mutating_topology() -> Result<()> {
    let existing = Address::repeat_byte(0xe3);
    let candidate = Address::repeat_byte(0xe4);
    let slot = U256::from(41);
    let mut engine = AmmSyncEngine::new(curve_registry(existing, slot)?)?;
    let registration = curve_registry(candidate, slot)?
        .pool(&PoolKey::Curve(candidate))
        .cloned()
        .expect("built registration");

    let plans = engine.preview_pool_subscriptions(std::slice::from_ref(&registration))?;
    assert_eq!(plans.len(), 1);
    assert_eq!(plans[0].instance().key(), &registration.key);
    assert_eq!(plans[0].instance().generation(), PoolGeneration::new(0));
    assert_eq!(
        plans[0].handler(),
        &AmmPoolReactiveHandler::handler_id(plans[0].instance())
    );
    assert!(!plans[0].interests().is_empty());
    assert!(engine.registry().pool(&registration.key).is_none());

    let report = engine.add_pools([registration])?;
    assert_eq!(report.registered_pools(), &[plans[0].instance().clone()]);
    let active = engine.active_pool_subscriptions()?;
    assert_eq!(active.len(), 2);
    assert!(
        active
            .iter()
            .any(|plan| plan.instance() == &plans[0].instance().clone())
    );
    Ok(())
}

#[test]
fn sync_engine_commits_a_ready_pool_into_its_exact_reserved_generation() -> Result<()> {
    let existing = Address::repeat_byte(0xe5);
    let candidate = Address::repeat_byte(0xe6);
    let slot = U256::from(42);
    let mut engine = AmmSyncEngine::new(curve_registry(existing, slot)?)?;
    let registration = curve_registry(candidate, slot)?
        .pool(&PoolKey::Curve(candidate))
        .cloned()
        .expect("built registration");
    let handlers = engine.runtime().handler_ids();

    let reservation = engine.reserve_pool_generation(registration.key.clone())?;

    assert_eq!(reservation.instance().key(), &registration.key);
    assert_eq!(reservation.instance().generation(), PoolGeneration::new(0));
    assert!(engine.registry().pool(&registration.key).is_none());
    assert_eq!(engine.runtime().handler_ids(), handlers);
    assert!(engine.ownership().active_pool(&registration.key).is_none());
    assert_eq!(
        engine.lifecycles().pool(reservation.instance()),
        Some(PoolRuntimeState::Queued)
    );

    let report = engine.commit_pool_reservation(&reservation, registration)?;

    assert_eq!(report.registered_pools(), &[reservation.instance().clone()]);
    assert_eq!(
        engine.ownership().active_pool(reservation.instance().key()),
        Some(reservation.instance())
    );
    assert_eq!(
        engine.lifecycles().pool(reservation.instance()),
        Some(PoolRuntimeState::Searchable)
    );
    Ok(())
}

#[test]
fn sync_engine_previews_the_exact_subscriber_owner_for_a_reserved_commit() -> Result<()> {
    let existing = Address::repeat_byte(0xf0);
    let candidate = Address::repeat_byte(0xf1);
    let slot = U256::from(47);
    let mut engine = AmmSyncEngine::new(curve_registry(existing, slot)?)?;
    let registration = curve_registry(candidate, slot)?
        .pool(&PoolKey::Curve(candidate))
        .cloned()
        .expect("built registration");
    let reservation = engine.reserve_pool_generation(registration.key.clone())?;

    let plan = engine.preview_reserved_pool_subscription(&reservation, &registration)?;

    assert_eq!(plan.instance(), reservation.instance());
    assert_eq!(
        plan.handler(),
        &AmmPoolReactiveHandler::handler_id(reservation.instance())
    );
    assert!(!plan.interests().is_empty());
    assert!(engine.registry().pool(&registration.key).is_none());

    engine.commit_pool_reservation(&reservation, registration)?;
    let active = engine.active_pool_subscriptions()?;
    let active = active
        .iter()
        .find(|candidate| candidate.instance() == reservation.instance())
        .expect("reserved generation became active");
    assert_eq!(active.handler(), plan.handler());
    assert_eq!(active.interests().len(), plan.interests().len());
    Ok(())
}

#[test]
fn sync_engine_rejects_duplicate_reservation_and_normal_add_without_consuming_the_claim()
-> Result<()> {
    let existing = Address::repeat_byte(0xe7);
    let candidate = Address::repeat_byte(0xe8);
    let slot = U256::from(43);
    let mut engine = AmmSyncEngine::new(curve_registry(existing, slot)?)?;
    let registration = curve_registry(candidate, slot)?
        .pool(&PoolKey::Curve(candidate))
        .cloned()
        .expect("built registration");

    let reservation = engine.reserve_pool_generation(registration.key.clone())?;
    assert!(
        engine
            .reserve_pool_generation(registration.key.clone())
            .is_err()
    );
    assert!(engine.add_pools([registration.clone()]).is_err());
    assert!(engine.registry().pool(&registration.key).is_none());
    assert!(engine.ownership().active_pool(&registration.key).is_none());

    let report = engine.commit_pool_reservation(&reservation, registration)?;
    assert_eq!(report.registered_pools(), &[reservation.instance().clone()]);
    assert_eq!(reservation.instance().generation(), PoolGeneration::new(0));
    Ok(())
}

#[test]
fn cancelled_pool_reservation_is_tombstoned_and_retry_gets_a_fresh_generation() -> Result<()> {
    let existing = Address::repeat_byte(0xe9);
    let candidate = Address::repeat_byte(0xea);
    let slot = U256::from(44);
    let mut engine = AmmSyncEngine::new(curve_registry(existing, slot)?)?;
    let registration = curve_registry(candidate, slot)?
        .pool(&PoolKey::Curve(candidate))
        .cloned()
        .expect("built registration");

    let cancelled = engine.reserve_pool_generation(registration.key.clone())?;
    assert!(engine.cancel_pool_reservation(&cancelled));
    assert_eq!(
        engine.lifecycles().pool(cancelled.instance()),
        Some(PoolRuntimeState::Removed)
    );
    assert!(
        engine
            .commit_pool_reservation(&cancelled, registration.clone())
            .is_err()
    );

    let retry = engine.reserve_pool_generation(registration.key.clone())?;
    assert_eq!(retry.instance().generation(), PoolGeneration::new(1));
    assert!(!engine.cancel_pool_reservation(&cancelled));
    let report = engine.commit_pool_reservation(&retry, registration)?;
    assert_eq!(report.registered_pools(), &[retry.instance().clone()]);
    assert_eq!(
        engine.lifecycles().pool(cancelled.instance()),
        Some(PoolRuntimeState::Removed)
    );
    Ok(())
}

#[test]
fn invalid_reserved_commit_keeps_the_exact_claim_available_for_correction() -> Result<()> {
    let existing = Address::repeat_byte(0xeb);
    let candidate = Address::repeat_byte(0xec);
    let wrong = Address::repeat_byte(0xed);
    let slot = U256::from(45);
    let mut engine = AmmSyncEngine::new(curve_registry(existing, slot)?)?;
    let registration = curve_registry(candidate, slot)?
        .pool(&PoolKey::Curve(candidate))
        .cloned()
        .expect("built registration");
    let wrong_registration = curve_registry(wrong, slot)?
        .pool(&PoolKey::Curve(wrong))
        .cloned()
        .expect("built wrong registration");
    let reservation = engine.reserve_pool_generation(registration.key.clone())?;

    assert!(
        engine
            .commit_pool_reservation(&reservation, wrong_registration)
            .is_err()
    );
    let mut cold = registration.clone();
    cold.status = PoolStatus::Cold;
    assert!(engine.commit_pool_reservation(&reservation, cold).is_err());
    assert_eq!(
        engine.lifecycles().pool(reservation.instance()),
        Some(PoolRuntimeState::Queued)
    );

    let report = engine.commit_pool_reservation(&reservation, registration)?;
    assert_eq!(report.registered_pools(), &[reservation.instance().clone()]);
    Ok(())
}

#[test]
fn registry_replacement_cannot_bypass_an_outstanding_pool_reservation() -> Result<()> {
    let existing = Address::repeat_byte(0xee);
    let candidate = Address::repeat_byte(0xef);
    let slot = U256::from(46);
    let mut engine = AmmSyncEngine::new(curve_registry(existing, slot)?)?;
    let registration = curve_registry(candidate, slot)?
        .pool(&PoolKey::Curve(candidate))
        .cloned()
        .expect("built registration");
    let reservation = engine.reserve_pool_generation(registration.key.clone())?;
    let replacement = engine.registry().clone();

    assert!(engine.replace_registry(replacement).is_err());
    assert_eq!(
        engine.lifecycles().pool(reservation.instance()),
        Some(PoolRuntimeState::Queued)
    );
    let report = engine.commit_pool_reservation(&reservation, registration)?;
    assert_eq!(report.registered_pools(), &[reservation.instance().clone()]);
    Ok(())
}

#[test]
fn sync_engine_rejects_an_invalid_add_batch_without_partial_state_or_generation_use() -> Result<()>
{
    let existing = Address::repeat_byte(0xd5);
    let candidate = Address::repeat_byte(0xd6);
    let slot = U256::from(32);
    let mut engine = AmmSyncEngine::new(curve_registry(existing, slot)?)?;
    let candidate_registration = curve_registry(candidate, slot)?
        .pool(&PoolKey::Curve(candidate))
        .cloned()
        .expect("built candidate");
    let duplicate = engine
        .registry()
        .pool(&PoolKey::Curve(existing))
        .cloned()
        .expect("existing registration");
    let handlers = engine.runtime().handler_ids();

    assert!(
        engine
            .add_pools([candidate_registration.clone(), duplicate])
            .is_err()
    );
    assert!(engine.registry().pool(&PoolKey::Curve(candidate)).is_none());
    assert_eq!(engine.runtime().handler_ids(), handlers);

    let report = engine.add_pools([candidate_registration])?;
    assert_eq!(
        report.registered_pools()[0].generation(),
        PoolGeneration::new(0)
    );
    Ok(())
}

#[tokio::test]
async fn sync_engine_remove_and_readd_fences_generations_and_preserves_the_journal() -> Result<()> {
    let pool = Address::repeat_byte(0xd7);
    let slot = U256::from(33);
    let key = PoolKey::Curve(pool);
    let mut cache = setup_cache().await?;
    let mut engine = AmmSyncEngine::new(curve_registry(pool, slot)?)?;
    engine.ingest_batch(
        &mut cache,
        batch(curve_log(pool, Address::repeat_byte(0x01), 120), 120),
    )?;
    let head = engine.runtime().last_canonical_block();
    let old = engine
        .ownership()
        .active_pool(&key)
        .cloned()
        .expect("initial generation");
    let old_handler = AmmPoolReactiveHandler::handler_id(&old);
    let registration = engine.registry().pool(&key).cloned().expect("registration");

    let removed = engine.remove_pools(std::slice::from_ref(&key))?;

    assert_eq!(removed.removed_pools().len(), 1);
    assert_eq!(removed.removed_pools()[0].instance(), &old);
    assert_eq!(
        engine.lifecycles().pool(&old),
        Some(PoolRuntimeState::Removed)
    );
    assert!(!engine.runtime().handler_ids().contains(&old_handler));
    assert_eq!(engine.runtime().last_canonical_block(), head);

    let added = engine.add_pools([registration])?;
    let replacement = &added.registered_pools()[0];
    assert_eq!(replacement.key(), &key);
    assert_eq!(replacement.generation(), PoolGeneration::new(1));
    assert_ne!(AmmPoolReactiveHandler::handler_id(replacement), old_handler);
    assert_eq!(engine.runtime().last_canonical_block(), head);
    Ok(())
}

#[tokio::test]
async fn pool_lifecycle_churn_preserves_existing_reorg_rollback_history() -> Result<()> {
    let pool_a = Address::repeat_byte(0xdd);
    let pool_b = Address::repeat_byte(0xde);
    let slot_a0 = U256::from(38);
    let slot_a1 = U256::from(39);
    let slot_b0 = U256::from(40);
    let slot_b1 = U256::from(41);
    let adapter = Arc::new(SolidlyV2Adapter::default());
    let mut registry = AdapterRegistry::new();
    registry.register_adapter(adapter.clone())?;
    registry.register_pool(solidly_registration(&adapter, pool_a, slot_a0, slot_a1))?;
    let mut engine = AmmSyncEngine::new(registry)?;
    let mut cache = setup_cache().await?;
    let canonical = rpc_log(
        pool_a,
        vec![solidly_sync_topic()],
        abi_words([U256::from(100), U256::from(200)]),
        125,
    );
    engine.ingest_batch(&mut cache, batch(canonical.clone(), 125))?;
    assert_eq!(
        cache.cached_storage_value(pool_a, slot_a0),
        Some(U256::from(100))
    );

    engine.add_pools([solidly_registration(&adapter, pool_b, slot_b0, slot_b1)])?;
    engine.remove_pools(&[PoolKey::SolidlyV2(pool_b)])?;
    engine.ingest_batch(&mut cache, reorged_batch(canonical, 125))?;

    assert_eq!(
        cache.cached_storage_value(pool_a, slot_a0),
        Some(U256::ZERO)
    );
    assert_eq!(
        cache.cached_storage_value(pool_a, slot_a1),
        Some(U256::ZERO)
    );
    Ok(())
}

#[tokio::test]
async fn exclusive_eviction_survives_reorg_of_removed_pool_generation() -> Result<()> {
    let pool = Address::repeat_byte(0xdf);
    let slot_a = U256::from(42);
    let slot_b = U256::from(43);
    let mut engine = AmmSyncEngine::new(solidly_registry(pool, slot_a, slot_b)?)?;
    let mut cache = setup_cache().await?;
    let canonical = rpc_log(
        pool,
        vec![solidly_sync_topic()],
        abi_words([U256::from(100), U256::from(200)]),
        126,
    );
    engine.ingest_batch(&mut cache, batch(canonical.clone(), 126))?;
    assert_eq!(
        cache.cached_storage_value(pool, slot_a),
        Some(U256::from(100))
    );

    engine.remove_pools_evicting(&[PoolKey::SolidlyV2(pool)], &mut cache)?;
    assert_eq!(cache.cached_storage_value(pool, slot_a), None);
    assert_eq!(cache.cached_storage_value(pool, slot_b), None);

    engine.ingest_batch(&mut cache, reorged_batch(canonical, 126))?;

    assert_eq!(
        cache.cached_storage_value(pool, slot_a),
        None,
        "a retained rollback journal must not rematerialize explicitly evicted state"
    );
    assert_eq!(cache.cached_storage_value(pool, slot_b), None);
    Ok(())
}

#[tokio::test]
async fn ordinary_ingest_ages_eviction_fence_without_cache_mutation() -> Result<()> {
    let pool = Address::repeat_byte(0xd6);
    let slot_a = U256::from(46);
    let slot_b = U256::from(47);
    let mut engine = AmmSyncEngine::with_config(
        solidly_registry(pool, slot_a, slot_b)?,
        ReactiveConfig {
            journal_depth: 1,
            ..ReactiveConfig::default()
        },
    )?;
    let mut cache = setup_cache().await?;
    engine.ingest_batch(
        &mut cache,
        batch(
            rpc_log(
                pool,
                vec![solidly_sync_topic()],
                abi_words([U256::from(100), U256::from(200)]),
                126,
            ),
            126,
        ),
    )?;
    engine.remove_pools_evicting(&[PoolKey::SolidlyV2(pool)], &mut cache)?;
    let evicted_generation = cache.snapshot_generation();

    engine.ingest_batch(
        &mut cache,
        batch(
            rpc_log(pool, vec![B256::repeat_byte(0xab)], Vec::new(), 127),
            127,
        ),
    )?;
    assert_eq!(
        cache.snapshot_generation(),
        evicted_generation,
        "ordinary fence aging must not issue a redundant purge"
    );

    cache.apply_updates(&[
        StateUpdate::slot(pool, slot_a, U256::from(300)),
        StateUpdate::slot(pool, slot_b, U256::from(400)),
    ]);
    let rewarmed_generation = cache.snapshot_generation();
    engine.ingest_batch(
        &mut cache,
        batch(
            rpc_log(pool, vec![B256::repeat_byte(0xac)], Vec::new(), 128),
            128,
        ),
    )?;

    assert_eq!(cache.snapshot_generation(), rewarmed_generation);
    assert_eq!(
        cache.cached_storage_value(pool, slot_a),
        Some(U256::from(300)),
        "a fence must retire when its handler leaves the bounded journal"
    );
    assert_eq!(
        cache.cached_storage_value(pool, slot_b),
        Some(U256::from(400))
    );
    Ok(())
}

#[tokio::test]
async fn readded_pool_retires_prior_generation_eviction_fence() -> Result<()> {
    let pool = Address::repeat_byte(0xd7);
    let slot_a = U256::from(44);
    let slot_b = U256::from(45);
    let registry = solidly_registry(pool, slot_a, slot_b)?;
    let registration = registry
        .pool(&PoolKey::SolidlyV2(pool))
        .cloned()
        .expect("registration");
    let mut engine = AmmSyncEngine::new(registry)?;
    let mut cache = setup_cache().await?;
    engine.ingest_batch(
        &mut cache,
        batch(
            rpc_log(
                pool,
                vec![solidly_sync_topic()],
                abi_words([U256::from(100), U256::from(200)]),
                127,
            ),
            127,
        ),
    )?;
    engine.remove_pools_evicting(&[PoolKey::SolidlyV2(pool)], &mut cache)?;

    engine.add_pools([registration])?;
    cache.apply_updates(&[
        StateUpdate::slot(pool, slot_a, U256::from(300)),
        StateUpdate::slot(pool, slot_b, U256::from(400)),
    ]);
    engine.remove_pools(&[PoolKey::SolidlyV2(pool)])?;
    engine.ingest_batch(
        &mut cache,
        batch(
            rpc_log(pool, vec![B256::repeat_byte(0xaa)], Vec::new(), 128),
            128,
        ),
    )?;

    assert_eq!(
        cache.cached_storage_value(pool, slot_a),
        Some(U256::from(300)),
        "re-adoption retires an older eviction fence, so replacement Retain semantics win"
    );
    assert_eq!(
        cache.cached_storage_value(pool, slot_b),
        Some(U256::from(400))
    );
    Ok(())
}

#[test]
fn sync_engine_adapter_lifecycle_is_generation_scoped_and_rejects_in_use_removal() -> Result<()> {
    let pool = Address::repeat_byte(0xd8);
    let slot = U256::from(34);
    let registration = curve_registry(pool, slot)?
        .pool(&PoolKey::Curve(pool))
        .cloned()
        .expect("registration");
    let mut engine = AmmSyncEngine::new(AdapterRegistry::new())?;

    let added = engine.add_adapter(Arc::new(CurveAdapter::default()))?;
    let first = added.registered_adapters()[0].clone();
    assert_eq!(first.generation().get(), 0);
    assert_eq!(
        engine.lifecycles().adapter(&first),
        Some(OwnerRuntimeState::Active)
    );

    engine.add_pools([registration.clone()])?;
    assert!(engine.remove_adapter(ProtocolId::Curve).is_err());
    assert!(engine.registry().adapter(ProtocolId::Curve).is_some());
    assert!(engine.registry().pool(&PoolKey::Curve(pool)).is_some());

    engine.remove_pools(&[PoolKey::Curve(pool)])?;
    let removed = engine.remove_adapter(ProtocolId::Curve)?;
    assert_eq!(removed.removed_adapters(), std::slice::from_ref(&first));
    assert_eq!(
        engine.lifecycles().adapter(&first),
        Some(OwnerRuntimeState::Removed)
    );

    let readded = engine.add_adapter(Arc::new(CurveAdapter::default()))?;
    assert_eq!(readded.registered_adapters()[0].generation().get(), 1);
    Ok(())
}

#[tokio::test]
async fn sync_engine_adapter_cascade_isolated_to_one_family() -> Result<()> {
    let curve_pool = Address::repeat_byte(0xd9);
    let curve_key = PoolKey::Curve(curve_pool);
    let balancer_vault = Address::repeat_byte(0xda);
    let balancer_id = B256::repeat_byte(0xdb);
    let balancer_key = PoolKey::BalancerV2(balancer_id);
    let balancer = Arc::new(BalancerV2Adapter::default());
    let mut registry = AdapterRegistry::new();
    registry.register_adapter(Arc::new(CurveAdapter::default()))?;
    registry.register_adapter(balancer.clone())?;
    registry.register_pool(
        curve_registry(curve_pool, U256::from(35))?
            .pool(&curve_key)
            .cloned()
            .expect("curve registration"),
    )?;
    registry.register_pool(balancer_registration(
        &balancer,
        balancer_vault,
        balancer_id,
        U256::from(36),
        Address::repeat_byte(0x01),
        Address::repeat_byte(0x02),
    ))?;
    let mut engine = AmmSyncEngine::new(registry)?;
    let mut cache = setup_cache().await?;
    cache.apply_updates(&[
        StateUpdate::slot(curve_pool, U256::from(35), U256::from(1)),
        StateUpdate::slot(balancer_vault, U256::from(36), U256::from(2)),
    ]);
    let balancer_instance = engine
        .ownership()
        .active_pool(&balancer_key)
        .cloned()
        .expect("balancer instance");
    let balancer_handler = AmmPoolReactiveHandler::handler_id(&balancer_instance);

    let report = engine.remove_adapter_cascade_evicting(ProtocolId::Curve, &mut cache)?;

    assert_eq!(report.removed_pools().len(), 1);
    assert_eq!(report.removed_pools()[0].instance().key(), &curve_key);
    assert_eq!(report.removed_adapters().len(), 1);
    assert!(engine.registry().pool(&curve_key).is_none());
    assert!(engine.registry().adapter(ProtocolId::Curve).is_none());
    assert!(engine.registry().pool(&balancer_key).is_some());
    assert!(engine.registry().adapter(ProtocolId::BalancerV2).is_some());
    assert!(engine.runtime().handler_ids().contains(&balancer_handler));
    assert_eq!(
        report.eviction().policy(),
        evm_amm_state::adapters::AmmEvictionPolicy::Exclusive
    );
    assert_eq!(cache.cached_storage_value(curve_pool, U256::from(35)), None);
    assert_eq!(
        cache.cached_storage_value(balancer_vault, U256::from(36)),
        Some(U256::from(2))
    );
    Ok(())
}

#[test]
fn explicit_registry_replacement_never_reuses_pool_or_adapter_generations() -> Result<()> {
    let pool = Address::repeat_byte(0xdc);
    let key = PoolKey::Curve(pool);
    let mut engine = AmmSyncEngine::new(curve_registry(pool, U256::from(37))?)?;
    let old_pool = engine
        .ownership()
        .active_pool(&key)
        .cloned()
        .expect("old pool generation");
    let old_adapter = engine
        .ownership()
        .adapters()
        .next()
        .cloned()
        .expect("old adapter generation");

    engine.replace_registry(engine.registry().clone())?;

    let new_pool = engine
        .ownership()
        .active_pool(&key)
        .expect("new pool generation");
    let new_adapter = engine
        .ownership()
        .adapters()
        .next()
        .expect("new adapter generation");
    assert_eq!(new_pool.generation().get(), old_pool.generation().get() + 1);
    assert_eq!(
        new_adapter.generation().get(),
        old_adapter.generation().get() + 1
    );
    assert_eq!(
        engine.lifecycles().pool(&old_pool),
        Some(PoolRuntimeState::Removed)
    );
    assert_eq!(
        engine.lifecycles().adapter(&old_adapter),
        Some(OwnerRuntimeState::Removed)
    );
    Ok(())
}

#[tokio::test]
async fn sync_engine_eviction_purges_exclusive_state_only() -> Result<()> {
    // Two Balancer pools sharing one vault, plus a Curve pool with its own
    // exclusive address: evicting pool A + the Curve pool must purge A's
    // balance slot and the whole Curve pool storage, but never co-tenant B's
    // vault slot.
    let vault = Address::repeat_byte(0xe0);
    let key_a = PoolKey::BalancerV2(B256::repeat_byte(0xa0));
    let key_b = PoolKey::BalancerV2(B256::repeat_byte(0xb0));
    let slot_a = U256::from(21);
    let slot_b = U256::from(22);

    let mut registry = AdapterRegistry::new();
    registry.register_adapter(Arc::new(BalancerV2Adapter::default()))?;
    registry.register_adapter(Arc::new(CurveAdapter::default()))?;
    for (key, slot) in [(key_a.clone(), slot_a), (key_b.clone(), slot_b)] {
        let mut registration = PoolRegistration::new(key)
            .with_state_address(vault)
            .with_status(PoolStatus::Ready)
            .with_metadata(ProtocolMetadata::BalancerV2(
                BalancerV2Metadata::default()
                    .with_vault(vault)
                    .with_balance_slots([slot]),
            ));
        let adapter = BalancerV2Adapter::default();
        let sources = adapter.event_sources(&registration);
        registration = registration.with_event_sources(sources);
        registry.register_pool(registration)?;
    }
    let curve_pool = Address::repeat_byte(0xe1);
    let curve_slot = U256::from(23);
    let curve = curve_registry(curve_pool, curve_slot)?
        .pool(&PoolKey::Curve(curve_pool))
        .cloned()
        .expect("curve registration");
    registry.register_pool(curve)?;

    let mut cache = setup_cache().await?;
    cache.apply_updates(&[
        StateUpdate::slot(vault, slot_a, U256::from(1_u64)),
        StateUpdate::slot(vault, slot_b, U256::from(2_u64)),
        StateUpdate::slot(curve_pool, curve_slot, U256::from(3_u64)),
    ]);

    let mut engine = AmmSyncEngine::new(registry)?;
    let removed =
        engine.remove_pools_evicting(&[key_a.clone(), PoolKey::Curve(curve_pool)], &mut cache)?;
    assert_eq!(removed.removed_pools().len(), 2);
    assert_eq!(
        removed.eviction().policy(),
        evm_amm_state::adapters::AmmEvictionPolicy::Exclusive
    );
    assert_eq!(removed.eviction().purged_accounts(), &[curve_pool]);
    assert_eq!(
        removed.eviction().purged_slots(),
        &[StateSlot::new(vault, slot_a)]
    );

    // Shared vault: only A's slot is purged; co-tenant B's survives.
    assert_eq!(cache.cached_storage_value(vault, slot_a), None);
    assert_eq!(
        cache.cached_storage_value(vault, slot_b),
        Some(U256::from(2_u64))
    );
    // Exclusive Curve address: whole storage purged.
    assert_eq!(cache.cached_storage_value(curve_pool, curve_slot), None);
    // The co-tenant pool is still registered and Ready.
    assert_eq!(
        engine.registry().pool(&key_b).expect("pool b").status,
        PoolStatus::Ready
    );
    Ok(())
}

fn solidly_sync_topic() -> B256 {
    keccak256(b"Sync(uint256,uint256)")
}

fn solidly_registry(pool: Address, r0_slot: U256, r1_slot: U256) -> Result<AdapterRegistry> {
    let layout =
        SolidlyStorageLayout::new(r0_slot, r1_slot, U256::from(12_u64), U256::from(13_u64));
    let adapter = Arc::new(SolidlyV2Adapter::default());
    let mut registration = PoolRegistration::new(PoolKey::SolidlyV2(pool))
        .with_state_address(pool)
        .with_status(PoolStatus::Ready)
        .with_metadata(ProtocolMetadata::SolidlyV2(
            SolidlyV2Metadata::default()
                .with_stable(false)
                .with_storage_layout(layout),
        ));
    let sources = adapter.event_sources(&registration);
    registration = registration.with_event_sources(sources);

    let mut registry = AdapterRegistry::new();
    registry.register_adapter(adapter)?;
    registry.register_pool(registration)?;
    Ok(registry)
}

fn solidly_registration(
    adapter: &Arc<SolidlyV2Adapter>,
    pool: Address,
    r0_slot: U256,
    r1_slot: U256,
) -> PoolRegistration {
    let layout =
        SolidlyStorageLayout::new(r0_slot, r1_slot, U256::from(12_u64), U256::from(13_u64));
    let mut registration = PoolRegistration::new(PoolKey::SolidlyV2(pool))
        .with_state_address(pool)
        .with_status(PoolStatus::Ready)
        .with_metadata(ProtocolMetadata::SolidlyV2(
            SolidlyV2Metadata::default()
                .with_stable(false)
                .with_storage_layout(layout),
        ));
    let sources = adapter.event_sources(&registration);
    registration = registration.with_event_sources(sources);
    registration
}

#[test]
fn degraded_pool_count_tracks_incremental_lifecycle_without_registry_scans() -> Result<()> {
    let adapter = Arc::new(SolidlyV2Adapter::default());
    let pool = Address::repeat_byte(0x50);
    let registration = solidly_registration(&adapter, pool, U256::from(10_u64), U256::from(11_u64))
        .with_status(PoolStatus::Degraded);
    let key = registration.key.clone();
    let mut registry = AdapterRegistry::new();
    registry.register_adapter(adapter)?;
    registry.register_pool(registration)?;
    let mut engine = AmmSyncEngine::new(registry)?;

    assert_eq!(engine.degraded_pool_count(), 1);
    assert!(!engine.has_other_degraded_pool(&key));
    engine.remove_pools(std::slice::from_ref(&key))?;
    assert_eq!(engine.degraded_pool_count(), 0);

    Ok(())
}

// Solidly is the exact-write protocol on the engine: a `Sync` event carries the
// absolute reserves, so `AmmSyncEngine` must apply both slot writes directly
// from the payload with ZERO resync work — no block trace, no storage fetch —
// and leave the pool `Ready`. (The panicking fetchers make any fallback loud.)
#[tokio::test]
async fn sync_engine_applies_solidly_sync_exactly_with_no_resync() -> Result<()> {
    let pool = Address::repeat_byte(0x51);
    let (r0_slot, r1_slot) = (U256::from(10_u64), U256::from(11_u64));
    let mut cache = setup_cache().await?;
    cache.set_block_state_diff_fetcher(Arc::new(|block| {
        panic!("exact event-sourcing must not request a block trace: {block:?}")
    }));
    cache.set_storage_batch_fetcher(Arc::new(|requests, _block| {
        panic!("exact event-sourcing must not fetch storage: {requests:?}")
    }));

    let registry = solidly_registry(pool, r0_slot, r1_slot)?;
    let mut engine = AmmSyncEngine::new(registry)?;

    let (reserve0, reserve1) = (U256::from(123_456_u64), U256::from(789_012_u64));
    let log = rpc_log(
        pool,
        vec![solidly_sync_topic()],
        abi_words([reserve0, reserve1]),
        90,
    );
    let report = engine.ingest_batch(&mut cache, batch(log, 90))?;

    assert_eq!(report.reactive.applied.len(), 1);
    assert_eq!(
        report.affected_pools,
        vec![PoolKey::SolidlyV2(pool)],
        "typed attribution is available only after the exact writes committed"
    );
    assert_eq!(
        report.pool_changes,
        vec![AmmSyncPoolChange::new(
            PoolKey::SolidlyV2(pool),
            AmmSyncPoolChangeKind::Updated,
            AmmChangeImpact::state_only(),
        )]
    );
    assert_eq!(report.resync_state_updates, 0, "Sync is exact: no resync");
    assert_eq!(report.resync_failures, 0);
    assert!(report.degraded_pools.is_empty());
    assert_eq!(cache.cached_storage_value(pool, r0_slot), Some(reserve0));
    assert_eq!(cache.cached_storage_value(pool, r1_slot), Some(reserve1));
    assert_eq!(
        engine
            .registry()
            .pool(&PoolKey::SolidlyV2(pool))
            .expect("registered pool")
            .status,
        PoolStatus::Ready
    );
    Ok(())
}

#[tokio::test]
async fn sync_engine_degrades_malformed_routed_event_as_unknown_impact() -> Result<()> {
    let pool = Address::repeat_byte(0x52);
    let (r0_slot, r1_slot) = (U256::from(10_u64), U256::from(11_u64));
    let mut cache = setup_cache().await?;
    let registry = solidly_registry(pool, r0_slot, r1_slot)?;
    let mut engine = AmmSyncEngine::new(registry)?;

    // `Sync` requires two ABI words; one word proves the watched event was
    // routed but its state transition could not be decoded.
    let malformed = rpc_log(
        pool,
        vec![solidly_sync_topic()],
        abi_words([U256::from(123_u64)]),
        91,
    );
    let report = engine.ingest_batch(&mut cache, batch(malformed, 91))?;

    assert_eq!(report.affected_pools, vec![PoolKey::SolidlyV2(pool)]);
    assert_eq!(report.degraded_pools, vec![PoolKey::SolidlyV2(pool)]);
    assert_eq!(report.pool_changes.len(), 1);
    assert_eq!(
        report.pool_changes[0].kind(),
        AmmSyncPoolChangeKind::Degraded
    );
    assert_eq!(
        report.pool_changes[0].source(),
        AmmSyncChangeSource::Unknown
    );
    assert_eq!(
        engine
            .registry()
            .pool(&PoolKey::SolidlyV2(pool))
            .expect("registered pool")
            .status,
        PoolStatus::Degraded
    );
    assert_eq!(cache.cached_storage_value(pool, r0_slot), None);
    assert_eq!(cache.cached_storage_value(pool, r1_slot), None);
    Ok(())
}

#[tokio::test]
async fn sync_engine_surfaces_forward_gap_and_conservatively_degrades_all_tracked_pools()
-> Result<()> {
    let pool_a = Address::repeat_byte(0x61);
    let pool_b = Address::repeat_byte(0x62);
    let adapter = Arc::new(SolidlyV2Adapter::default());
    let mut registry = AdapterRegistry::new();
    registry.register_adapter(adapter.clone())?;
    // Register in reverse key order: public report vectors must still be
    // canonical and independent of registry insertion order.
    registry.register_pool(solidly_registration(
        &adapter,
        pool_b,
        U256::from(3),
        U256::from(4),
    ))?;
    registry.register_pool(solidly_registration(
        &adapter,
        pool_a,
        U256::from(1),
        U256::from(2),
    ))?;

    let mut cache = setup_cache().await?;
    let mut engine = AmmSyncEngine::new(registry)?;
    let first = rpc_log(
        pool_a,
        vec![solidly_sync_topic()],
        abi_words([U256::from(10), U256::from(20)]),
        100,
    );
    engine.ingest_batch(&mut cache, batch(first, 100))?;

    let after_gap = rpc_log(
        pool_a,
        vec![solidly_sync_topic()],
        abi_words([U256::from(11), U256::from(21)]),
        103,
    );
    let report = engine.ingest_batch(&mut cache, batch(after_gap, 103))?;

    assert_eq!(
        report.incidents,
        vec![AmmSyncIncident::Gap { from: 101, to: 102 }]
    );
    assert!(report.requires_full_refresh);
    assert_eq!(
        report.affected_pools,
        vec![PoolKey::SolidlyV2(pool_a), PoolKey::SolidlyV2(pool_b)]
    );
    assert!(
        report
            .pool_changes
            .iter()
            .all(|change| change.kind() == AmmSyncPoolChangeKind::Degraded
                && change.source() == AmmSyncChangeSource::Unknown)
    );
    assert!(
        engine
            .registry()
            .pools()
            .all(|pool| pool.status == PoolStatus::Degraded)
    );
    Ok(())
}

#[tokio::test]
async fn gap_refresh_requirement_persists_across_later_ordinary_batches() -> Result<()> {
    let pool = Address::repeat_byte(0x63);
    let slot = U256::from(5);
    let current = U256::from(55_u64);
    let mut cache = setup_cache().await?;
    cache.set_block_state_diff_fetcher(Arc::new(move |_block| {
        Ok(diff_for_slot(pool, slot, current))
    }));
    let mut engine = AmmSyncEngine::new(curve_registry(pool, slot)?)?;

    engine.ingest_batch(
        &mut cache,
        batch(curve_log(pool, Address::repeat_byte(0x01), 100), 100),
    )?;
    let gap = engine.ingest_batch(
        &mut cache,
        batch(curve_log(pool, Address::repeat_byte(0x01), 103), 103),
    )?;
    assert!(gap.requires_full_refresh);

    let later = engine.ingest_batch(
        &mut cache,
        batch(curve_log(pool, Address::repeat_byte(0x01), 104), 104),
    )?;
    assert!(!later.requires_full_refresh, "the incident is not repeated");
    assert!(later.recovered_pools.is_empty());
    assert_eq!(
        engine
            .registry()
            .pool(&PoolKey::Curve(pool))
            .expect("registered pool")
            .status,
        PoolStatus::Degraded,
        "the engine-level explicit-refresh fence survives the report boundary"
    );
    Ok(())
}

#[tokio::test]
async fn sync_engine_surfaces_reorg_without_fabricating_a_post_block_commit() -> Result<()> {
    let pool_a = Address::repeat_byte(0x71);
    let pool_b = Address::repeat_byte(0x72);
    let slot_a0 = U256::from(1);
    let slot_a1 = U256::from(2);
    let adapter = Arc::new(SolidlyV2Adapter::default());
    let mut registry = AdapterRegistry::new();
    registry.register_adapter(adapter.clone())?;
    registry.register_pool(solidly_registration(&adapter, pool_a, slot_a0, slot_a1))?;
    registry.register_pool(solidly_registration(
        &adapter,
        pool_b,
        U256::from(3),
        U256::from(4),
    ))?;

    let mut cache = setup_cache().await?;
    let mut engine = AmmSyncEngine::new(registry)?;
    let canonical = rpc_log(
        pool_a,
        vec![solidly_sync_topic()],
        abi_words([U256::from(100), U256::from(200)]),
        110,
    );
    engine.ingest_batch(&mut cache, batch(canonical.clone(), 110))?;

    let report = engine.ingest_batch(&mut cache, reorged_batch(canonical, 110))?;

    assert_eq!(
        report.incidents,
        vec![AmmSyncIncident::Reorg {
            dropped_blocks: vec![BlockRef {
                number: 110,
                hash: block_hash(110),
                parent_hash: Some(block_hash(109)),
                timestamp: Some(1_700_000_110),
            }],
        }]
    );
    assert!(report.requires_full_refresh);
    assert_eq!(
        report.affected_pools,
        vec![PoolKey::SolidlyV2(pool_a), PoolKey::SolidlyV2(pool_b)]
    );
    assert!(
        report
            .pool_changes
            .iter()
            .all(|change| change.kind() == AmmSyncPoolChangeKind::Degraded
                && change.source() == AmmSyncChangeSource::Unknown)
    );
    assert!(report.reactive.reports.iter().all(|entry| !matches!(
        entry.as_ref(),
        evm_fork_cache::reactive::ReactiveReport::BlockCommitted(_)
    )));
    assert_eq!(
        cache.cached_storage_value(pool_a, slot_a0),
        Some(U256::ZERO)
    );
    assert_eq!(
        cache.cached_storage_value(pool_a, slot_a1),
        Some(U256::ZERO)
    );
    Ok(())
}
