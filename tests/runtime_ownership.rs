use std::collections::BTreeMap;
use std::sync::Arc;

use alloy_network::Ethereum;
use alloy_primitives::{Address, B256, Bytes, Log as PrimitiveLog, U256};
use alloy_provider::{RootProvider, network::AnyNetwork};
use alloy_rpc_client::RpcClient;
use alloy_rpc_types_eth::Log as RpcLog;
use alloy_transport::mock::Asserter;
use anyhow::Result;
use evm_amm_state::adapters::{
    AdapterEvent, AdapterEventKind, AdapterEventResult, AdapterGeneration, AdapterInstanceId,
    AdapterKey, AdapterRegistry, AmmAdapter, AmmOwnershipError, AmmOwnershipIndex,
    AmmPoolReactiveHandler, AmmPoolReactiveHandlerError, AmmReactiveRoutingContext,
    AmmReactiveSignal, AmmSyncEngine, CustomPoolKey, DiscoveryGeneration, DiscoveryOwnerId,
    DiscoveryOwnerKey, DiscoveryOwnership, EventSource, PoolGeneration, PoolInstanceId, PoolKey,
    PoolOwnership, PoolRegistration, PoolStateDependencies, ProtocolId, RepairAction,
    RuntimeOwnerId, RuntimeWorkId, StateSlot, StateUpdate, StateView, UpdateQuality, WorkId,
};
use evm_fork_cache::cache::EvmCache;
use evm_fork_cache::reactive::{
    BlockRef, ChainStatus, InputSource, ReactiveConfig, ReactiveContext, ReactiveInput,
    ReactiveInputBatch, ReactiveInputRecord, ReactiveRuntime, ResyncId,
};

const CUSTOM_PROTOCOL: &str = "stage2-routing";
const OWNERSHIP_PROTOCOL: &str = "stage2-ownership";
const INDEXED_PROTOCOL: &str = "stage2-indexed-address";

fn block_hash(number: u64) -> B256 {
    B256::repeat_byte(number as u8)
}

fn context(number: u64) -> ReactiveContext {
    let block = BlockRef {
        number,
        hash: block_hash(number),
        parent_hash: Some(block_hash(number.saturating_sub(1))),
        timestamp: Some(1_700_000_000 + number),
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

fn rpc_log(emitter: Address, topic: B256, number: u64) -> RpcLog {
    RpcLog {
        inner: PrimitiveLog::new_unchecked(emitter, vec![topic], Bytes::new()),
        block_hash: Some(block_hash(number)),
        block_number: Some(number),
        block_timestamp: Some(1_700_000_000 + number),
        transaction_hash: Some(B256::repeat_byte(0x44)),
        transaction_index: Some(0),
        log_index: Some(0),
        removed: false,
    }
}

fn rpc_log_with_topics(emitter: Address, topics: Vec<B256>, number: u64) -> RpcLog {
    RpcLog {
        inner: PrimitiveLog::new_unchecked(emitter, topics, Bytes::new()),
        block_hash: Some(block_hash(number)),
        block_number: Some(number),
        block_timestamp: Some(1_700_000_000 + number),
        transaction_hash: Some(B256::repeat_byte(0x44)),
        transaction_index: Some(0),
        log_index: Some(0),
        removed: false,
    }
}

fn topic_address(address: Address) -> B256 {
    let mut bytes = [0_u8; 32];
    bytes[12..].copy_from_slice(address.as_slice());
    B256::from(bytes)
}

fn batch(log: RpcLog, number: u64) -> ReactiveInputBatch<Ethereum> {
    ReactiveInputBatch::new(vec![ReactiveInputRecord::new(
        ReactiveInput::Log(log),
        context(number),
    )])
}

async fn cache() -> EvmCache {
    let client = RpcClient::mocked(Asserter::new());
    EvmCache::new(Arc::new(RootProvider::<AnyNetwork>::new(client))).await
}

fn custom_key(address: Address) -> PoolKey {
    PoolKey::Custom(CustomPoolKey::Address {
        protocol: CUSTOM_PROTOCOL,
        address,
    })
}

fn indexed_key(address: Address) -> PoolKey {
    PoolKey::Custom(CustomPoolKey::Address {
        protocol: INDEXED_PROTOCOL,
        address,
    })
}

struct IndexedAddressAdapter;

impl AmmAdapter for IndexedAddressAdapter {
    fn protocol(&self) -> ProtocolId {
        ProtocolId::Custom(INDEXED_PROTOCOL)
    }

    fn decode_event(
        &self,
        pool: &PoolRegistration,
        log: &PrimitiveLog,
        _view: &dyn StateView,
    ) -> AdapterEventResult {
        AdapterEventResult::event(AdapterEvent::new(
            pool.key.clone(),
            log.address,
            log.topics()[0],
            AdapterEventKind::Unknown,
            UpdateQuality::Exact,
        ))
    }
}

struct RegistrySensitiveRouter {
    emitter: Address,
    topic: B256,
    preferred: PoolKey,
    fallback: PoolKey,
    repair: RepairAction,
}

impl AmmAdapter for RegistrySensitiveRouter {
    fn protocol(&self) -> ProtocolId {
        ProtocolId::Custom(CUSTOM_PROTOCOL)
    }

    fn route_log(&self, log: &PrimitiveLog, registry: &AdapterRegistry) -> Option<PoolKey> {
        if log.address != self.emitter || log.topics().first() != Some(&self.topic) {
            return None;
        }
        if registry.pool(&self.preferred).is_some() {
            Some(self.preferred.clone())
        } else if registry.pool(&self.fallback).is_some() {
            Some(self.fallback.clone())
        } else {
            None
        }
    }

    fn decode_event(
        &self,
        pool: &PoolRegistration,
        log: &PrimitiveLog,
        _view: &dyn StateView,
    ) -> AdapterEventResult {
        AdapterEventResult::event(
            AdapterEvent::new(
                pool.key.clone(),
                log.address,
                self.topic,
                AdapterEventKind::Unknown,
                UpdateQuality::Exact,
            )
            .with_updates([StateUpdate::slot(
                pool.key.address().expect("address-keyed custom pool"),
                U256::ZERO,
                U256::from(1),
            )])
            .with_repair(self.repair.clone()),
        )
    }
}

fn registration(key: PoolKey, emitter: Address, topic: B256) -> PoolRegistration {
    PoolRegistration::new(key).with_event_source(EventSource::adapter_defined(emitter, vec![topic]))
}

#[test]
fn pool_handler_and_ownership_construction_failures_are_typed_and_atomic() -> Result<()> {
    let key = custom_key(Address::repeat_byte(0xc1));
    let instance = PoolInstanceId::new(key.clone(), PoolGeneration::new(7));
    let unknown = AmmPoolReactiveHandler::new(Arc::new(AdapterRegistry::new()), instance.clone())
        .expect_err("unknown pool must be rejected");
    assert_eq!(
        unknown,
        AmmPoolReactiveHandlerError::UnknownPool(key.clone())
    );

    let mut missing_adapter = AdapterRegistry::new();
    missing_adapter.register_pool(PoolRegistration::new(key.clone()))?;
    let missing = AmmPoolReactiveHandler::new(Arc::new(missing_adapter.clone()), instance)
        .expect_err("pool without adapter must be rejected");
    assert_eq!(
        missing,
        AmmPoolReactiveHandlerError::MissingAdapter(key.clone())
    );
    assert_eq!(
        AmmOwnershipIndex::from_registry(&missing_adapter).unwrap_err(),
        AmmOwnershipError::MissingAdapter(key.clone())
    );

    let adapter = AdapterInstanceId::new(
        AdapterKey::new(ProtocolId::Custom(CUSTOM_PROTOCOL), []),
        AdapterGeneration::new(1),
    );
    let first = PoolInstanceId::new(key.clone(), PoolGeneration::new(1));
    let duplicate = PoolInstanceId::new(key.clone(), PoolGeneration::new(2));
    let state = StateSlot::new(Address::repeat_byte(0xc2), U256::from(2));
    let mut index = AmmOwnershipIndex::default();
    index.insert_adapter(adapter.clone())?;
    assert_eq!(index.active_adapter(adapter.key()), Some(&adapter));
    assert_eq!(
        index.adapters().cloned().collect::<Vec<_>>(),
        vec![adapter.clone()]
    );
    index.insert_pool(PoolOwnership::new(
        first.clone(),
        adapter.clone(),
        PoolStateDependencies::default().with_slots([state]),
        [Address::repeat_byte(0xc3)],
    )?)?;
    assert_eq!(
        index.insert_pool(PoolOwnership::new(
            duplicate,
            adapter.clone(),
            PoolStateDependencies::default().with_whole_accounts([Address::repeat_byte(0xff)]),
            [Address::repeat_byte(0xfe)],
        )?),
        Err(AmmOwnershipError::DuplicatePool(key.clone()))
    );
    assert_eq!(index.active_pool(&key), Some(&first));
    assert_eq!(index.pools_for_slot(state), vec![first.clone()]);
    assert!(
        index
            .pools_for_address(Address::repeat_byte(0xff))
            .is_empty()
    );
    assert!(
        index
            .pools_for_emitter(Address::repeat_byte(0xfe))
            .is_empty()
    );
    let discovery_owner = RuntimeOwnerId::Discovery(DiscoveryOwnerId::new(
        DiscoveryOwnerKey::new("not-yet-registered"),
        DiscoveryGeneration::new(1),
    ));
    let discovery_work = RuntimeWorkId::new(discovery_owner.clone(), WorkId::new(9));
    assert_eq!(
        index.track_work(discovery_work),
        Err(AmmOwnershipError::UnknownWorkOwner(discovery_owner))
    );

    let unknown_key = custom_key(Address::repeat_byte(0xca));
    let unknown_pool = PoolInstanceId::new(unknown_key.clone(), PoolGeneration::new(1));
    let unknown_adapter = AdapterInstanceId::new(
        AdapterKey::new(ProtocolId::Custom(CUSTOM_PROTOCOL), []),
        AdapterGeneration::new(99),
    );
    let unknown_ownership = PoolOwnership::new(
        unknown_pool,
        unknown_adapter.clone(),
        PoolStateDependencies::default(),
        [],
    )?;
    assert_eq!(
        index.insert_pool(unknown_ownership),
        Err(AmmOwnershipError::UnknownAdapter(unknown_adapter))
    );
    assert!(index.active_pool(&unknown_key).is_none());

    let mismatched_pool = PoolInstanceId::new(
        PoolKey::UniswapV2(Address::repeat_byte(0xcb)),
        PoolGeneration::new(1),
    );
    assert_eq!(
        PoolOwnership::new(
            mismatched_pool.clone(),
            adapter.clone(),
            PoolStateDependencies::default(),
            [],
        )
        .unwrap_err(),
        AmmOwnershipError::AdapterProtocolMismatch {
            pool: mismatched_pool,
            adapter,
        }
    );
    Ok(())
}

#[test]
fn discovery_ownership_is_generation_exact_and_adapter_scoped() -> Result<()> {
    let adapter = AdapterInstanceId::new(
        AdapterKey::new(ProtocolId::Custom(OWNERSHIP_PROTOCOL), []),
        AdapterGeneration::new(3),
    );
    let key = DiscoveryOwnerKey::new("ethereum.factory.0xfeed");
    let first = DiscoveryOwnerId::new(key.clone(), DiscoveryGeneration::new(4));
    let replacement = DiscoveryOwnerId::new(key.clone(), DiscoveryGeneration::new(5));
    let first_work = RuntimeWorkId::new(RuntimeOwnerId::Discovery(first.clone()), WorkId::new(11));
    let replacement_work = RuntimeWorkId::new(
        RuntimeOwnerId::Discovery(replacement.clone()),
        WorkId::new(12),
    );

    let mut index = AmmOwnershipIndex::default();
    index.insert_adapter(adapter.clone())?;
    index.insert_discovery(DiscoveryOwnership::new(first.clone(), adapter.clone()))?;

    assert_eq!(index.active_discovery(&key), Some(&first));
    assert_eq!(index.adapter_for_discovery(&first), Some(&adapter));
    assert_eq!(index.discovery_for_adapter(&adapter), vec![first.clone()]);
    index.track_work(first_work.clone())?;

    let removed = index
        .remove_discovery(&first)
        .expect("the exact watcher generation is active");
    assert_eq!(removed.ownership().owner(), &first);
    assert_eq!(removed.cancelled_work(), std::slice::from_ref(&first_work));
    assert!(index.active_discovery(&key).is_none());

    index.insert_discovery(DiscoveryOwnership::new(
        replacement.clone(),
        adapter.clone(),
    ))?;
    index.track_work(replacement_work.clone())?;

    assert!(index.remove_discovery(&first).is_none());
    assert_eq!(index.active_discovery(&key), Some(&replacement));
    assert_eq!(
        index.track_work(first_work),
        Err(AmmOwnershipError::UnknownWorkOwner(
            RuntimeOwnerId::Discovery(first)
        ))
    );
    assert_eq!(
        index.work_for_owner(&RuntimeOwnerId::Discovery(replacement)),
        vec![replacement_work]
    );
    Ok(())
}

#[test]
fn adapter_removal_waits_for_every_discovery_owner() -> Result<()> {
    let adapter = AdapterInstanceId::new(
        AdapterKey::new(ProtocolId::Custom(OWNERSHIP_PROTOCOL), []),
        AdapterGeneration::new(6),
    );
    let watcher = DiscoveryOwnerId::new(
        DiscoveryOwnerKey::new("ethereum.factory.0xbeef"),
        DiscoveryGeneration::new(2),
    );
    let mut index = AmmOwnershipIndex::default();
    index.insert_adapter(adapter.clone())?;
    index.insert_discovery(DiscoveryOwnership::new(watcher.clone(), adapter.clone()))?;

    assert_eq!(
        index.remove_adapter(&adapter),
        Err(AmmOwnershipError::AdapterInUse(adapter.clone()))
    );
    assert_eq!(index.active_adapter(adapter.key()), Some(&adapter));
    assert_eq!(index.active_discovery(watcher.key()), Some(&watcher));

    index.remove_discovery(&watcher);
    assert!(index.remove_adapter(&adapter)?);
    assert!(index.active_adapter(adapter.key()).is_none());
    Ok(())
}

#[tokio::test]
async fn adapter_defined_pool_handlers_observe_the_current_routing_universe() -> Result<()> {
    let emitter = Address::repeat_byte(0xe1);
    let topic = B256::repeat_byte(0xe2);
    let pool_a = custom_key(Address::repeat_byte(0xa1));
    let pool_b = custom_key(Address::repeat_byte(0xb1));
    let adapter = Arc::new(RegistrySensitiveRouter {
        emitter,
        topic,
        preferred: pool_b.clone(),
        fallback: pool_a.clone(),
        repair: RepairAction::V3Incremental {
            pool: pool_b.clone(),
        },
    });

    let mut initial = AdapterRegistry::new();
    initial.register_adapter(adapter)?;
    initial.register_pool(registration(pool_a.clone(), emitter, topic))?;
    let routing = AmmReactiveRoutingContext::new(Arc::new(initial));
    let handler_a = Arc::new(AmmPoolReactiveHandler::with_routing_context(
        routing.clone(),
        PoolInstanceId::new(pool_a.clone(), PoolGeneration::new(1)),
    )?);

    let mut current = routing.registry().as_ref().clone();
    current.register_pool(registration(pool_b.clone(), emitter, topic))?;
    routing.replace_registry(Arc::new(current));
    let instance_b = PoolInstanceId::new(pool_b.clone(), PoolGeneration::new(1));
    let handler_b = Arc::new(AmmPoolReactiveHandler::with_routing_context(
        routing,
        instance_b.clone(),
    )?);

    let mut runtime = ReactiveRuntime::<Ethereum>::new(ReactiveConfig::default());
    runtime.register_handler(handler_a)?;
    runtime.register_handler(handler_b)?;
    let mut cache = cache().await;
    let report = runtime.ingest_batch(&mut cache, batch(rpc_log(emitter, topic, 50), 50))?;

    assert_eq!(report.applied.len(), 1);
    assert_eq!(
        report.applied[0].handler_id,
        AmmPoolReactiveHandler::handler_id(&instance_b)
    );
    let signal = report.applied[0]
        .hook_signals
        .iter()
        .filter_map(|signal| signal.payload.as_deref())
        .filter_map(|payload| payload.downcast_ref::<AmmReactiveSignal>())
        .find(|signal| matches!(signal, AmmReactiveSignal::PoolEvent { .. }))
        .expect("pool handler emits a typed signal");
    assert!(matches!(
        signal,
        AmmReactiveSignal::PoolEvent { instance, event }
            if instance == &instance_b && event.pool == pool_b
    ));
    assert!(report.applied[0].hook_signals.iter().any(|signal| {
        matches!(
            signal
                .payload
                .as_deref()
                .and_then(|payload| payload.downcast_ref::<AmmReactiveSignal>()),
            Some(AmmReactiveSignal::PoolRepair { instance, action })
                if instance == &instance_b
                    && matches!(
                        action,
                        RepairAction::V3Incremental { pool } if pool == &pool_b
                    )
        )
    }));
    assert_eq!(
        cache.cached_storage_value(pool_a.address().unwrap(), U256::ZERO),
        None
    );
    Ok(())
}

#[tokio::test]
async fn sync_engine_lifecycle_updates_adapter_defined_routing_without_rebuild() -> Result<()> {
    let emitter = Address::repeat_byte(0xe3);
    let topic = B256::repeat_byte(0xe4);
    let pool_a = custom_key(Address::repeat_byte(0xa2));
    let pool_b = custom_key(Address::repeat_byte(0xb2));
    let adapter = Arc::new(RegistrySensitiveRouter {
        emitter,
        topic,
        preferred: pool_b.clone(),
        fallback: pool_a.clone(),
        repair: RepairAction::None,
    });
    let mut registry = AdapterRegistry::new();
    registry.register_adapter(adapter)?;
    registry.register_pool(registration(pool_a.clone(), emitter, topic))?;
    let mut engine = AmmSyncEngine::new(registry)?;
    let instance_a = engine
        .ownership()
        .active_pool(&pool_a)
        .cloned()
        .expect("pool A instance");
    let mut cache = cache().await;

    let before = engine.ingest_batch(&mut cache, batch(rpc_log(emitter, topic, 70), 70))?;
    assert_eq!(
        before.reactive.applied[0].handler_id,
        AmmPoolReactiveHandler::handler_id(&instance_a)
    );

    let added = engine.add_pools([registration(pool_b.clone(), emitter, topic)])?;
    let instance_b = added.registered_pools()[0].clone();
    let after_add = engine.ingest_batch(&mut cache, batch(rpc_log(emitter, topic, 71), 71))?;
    assert_eq!(after_add.reactive.applied.len(), 1);
    assert_eq!(
        after_add.reactive.applied[0].handler_id,
        AmmPoolReactiveHandler::handler_id(&instance_b)
    );

    engine.remove_pools(std::slice::from_ref(&pool_b))?;
    let after_remove = engine.ingest_batch(&mut cache, batch(rpc_log(emitter, topic, 72), 72))?;
    assert_eq!(after_remove.reactive.applied.len(), 1);
    assert_eq!(
        after_remove.reactive.applied[0].handler_id,
        AmmPoolReactiveHandler::handler_id(&instance_a)
    );
    Ok(())
}

#[tokio::test]
async fn indexed_address_pool_handlers_route_exactly_and_reject_short_topics() -> Result<()> {
    let emitter = Address::repeat_byte(0x71);
    let topic = B256::repeat_byte(0x72);
    let pool_a = indexed_key(Address::repeat_byte(0x73));
    let pool_b = indexed_key(Address::repeat_byte(0x74));
    let mut registry = AdapterRegistry::new();
    registry.register_adapter(Arc::new(IndexedAddressAdapter))?;
    for pool in [pool_a.clone(), pool_b.clone()] {
        registry.register_pool(
            PoolRegistration::new(pool).with_event_source(EventSource::indexed_address(
                emitter,
                vec![topic],
                1,
            )),
        )?;
    }
    let registry = Arc::new(registry);
    let routing = AmmReactiveRoutingContext::new(registry.clone());
    let instance_a = PoolInstanceId::new(pool_a, PoolGeneration::new(1));
    let instance_b = PoolInstanceId::new(pool_b, PoolGeneration::new(1));
    let mut runtime = ReactiveRuntime::<Ethereum>::new(ReactiveConfig::default());
    for instance in [instance_a, instance_b.clone()] {
        runtime.register_handler(Arc::new(AmmPoolReactiveHandler::with_routing_context(
            routing.clone(),
            instance,
        )?))?;
    }
    let mut cache = cache().await;
    let routed = runtime.ingest_batch(
        &mut cache,
        batch(
            rpc_log_with_topics(
                emitter,
                vec![topic, topic_address(instance_b.key().address().unwrap())],
                60,
            ),
            60,
        ),
    )?;
    assert_eq!(routed.applied.len(), 1);
    assert_eq!(
        routed.applied[0].handler_id,
        AmmPoolReactiveHandler::handler_id(&instance_b)
    );

    let short = runtime.ingest_batch(
        &mut cache,
        batch(rpc_log_with_topics(emitter, vec![topic], 61), 61),
    )?;
    assert!(short.applied.is_empty());
    Ok(())
}

#[tokio::test]
async fn readded_pool_generation_cannot_alias_old_resync_or_hook_ownership() -> Result<()> {
    let emitter = Address::repeat_byte(0xd1);
    let topic = B256::repeat_byte(0xd2);
    let pool = custom_key(Address::repeat_byte(0xd3));
    let adapter = Arc::new(RegistrySensitiveRouter {
        emitter,
        topic,
        preferred: pool.clone(),
        fallback: pool.clone(),
        repair: RepairAction::VerifySlots(vec![(
            pool.address().expect("address-keyed custom pool"),
            U256::ZERO,
        )]),
    });
    let mut registry = AdapterRegistry::new();
    registry.register_adapter(adapter)?;
    registry.register_pool(registration(pool.clone(), emitter, topic))?;
    let registry = Arc::new(registry);

    let run = |generation| {
        let registry = registry.clone();
        let pool = pool.clone();
        async move {
            let instance = PoolInstanceId::new(pool, PoolGeneration::new(generation));
            let handler = Arc::new(AmmPoolReactiveHandler::new(registry, instance.clone())?);
            let mut runtime = ReactiveRuntime::<Ethereum>::new(ReactiveConfig::default());
            runtime.register_handler(handler)?;
            let mut cache = cache().await;
            let report =
                runtime.ingest_batch(&mut cache, batch(rpc_log(emitter, topic, 51), 51))?;
            let applied = report.applied.first().expect("routed pool handler");
            let resync = applied
                .resyncs
                .first()
                .expect("cold state update is repaired")
                .id
                .clone();
            let signal = applied
                .hook_signals
                .iter()
                .filter_map(|signal| signal.payload.as_deref())
                .find_map(|payload| payload.downcast_ref::<AmmReactiveSignal>())
                .cloned()
                .expect("typed pool signal");
            Ok::<_, anyhow::Error>((instance, resync, signal))
        }
    };

    let (old, old_resync, old_signal) = run(4).await?;
    let (replacement, replacement_resync, replacement_signal) = run(5).await?;
    assert_ne!(old_resync, replacement_resync);
    assert!(matches!(
        old_signal,
        AmmReactiveSignal::PoolEvent { instance, .. } if instance == old
    ));
    assert!(matches!(
        replacement_signal,
        AmmReactiveSignal::PoolEvent { instance, .. } if instance == replacement
    ));
    Ok(())
}

struct ExactSlotAdapter {
    slots: BTreeMap<PoolKey, StateSlot>,
}

impl AmmAdapter for ExactSlotAdapter {
    fn protocol(&self) -> ProtocolId {
        ProtocolId::Custom(OWNERSHIP_PROTOCOL)
    }

    fn state_dependencies(&self, pool: &PoolRegistration) -> PoolStateDependencies {
        let slot = self.slots.get(&pool.key).copied().expect("declared pool");
        PoolStateDependencies::default()
            .with_associated_addresses([slot.address()])
            .with_slots([slot])
    }
}

struct WholeAddressAdapter;

impl AmmAdapter for WholeAddressAdapter {
    fn protocol(&self) -> ProtocolId {
        ProtocolId::UniswapV2
    }
}

fn ownership_key(address: Address) -> PoolKey {
    PoolKey::Custom(CustomPoolKey::Address {
        protocol: OWNERSHIP_PROTOCOL,
        address,
    })
}

#[test]
fn ownership_index_distinguishes_association_exact_slots_and_whole_accounts() -> Result<()> {
    let vault = Address::repeat_byte(0xf0);
    let emitter_a = Address::repeat_byte(0xa0);
    let emitter_b = Address::repeat_byte(0xb0);
    let key_a = ownership_key(Address::repeat_byte(0xa1));
    let key_b = ownership_key(Address::repeat_byte(0xb1));
    let slot_a = StateSlot::new(vault, U256::from(1));
    let slot_b = StateSlot::new(vault, U256::from(2));
    let whole_address = Address::repeat_byte(0xc1);
    let whole_key = PoolKey::UniswapV2(whole_address);

    let exact = Arc::new(ExactSlotAdapter {
        slots: BTreeMap::from([(key_a.clone(), slot_a), (key_b.clone(), slot_b)]),
    });
    let mut registry = AdapterRegistry::new();
    registry.register_adapter(exact)?;
    registry.register_adapter(Arc::new(WholeAddressAdapter))?;
    registry.register_pool(
        PoolRegistration::new(key_b.clone())
            .with_state_address(vault)
            .with_event_source(EventSource::direct(emitter_b, Vec::new())),
    )?;
    registry.register_pool(
        PoolRegistration::new(key_a.clone())
            .with_state_address(vault)
            .with_event_source(EventSource::direct(emitter_a, Vec::new())),
    )?;
    registry.register_pool(PoolRegistration::new(whole_key.clone()))?;

    let index = AmmOwnershipIndex::from_registry(&registry)?;
    let instance_a = PoolInstanceId::new(key_a, PoolGeneration::new(0));
    let instance_b = PoolInstanceId::new(key_b, PoolGeneration::new(0));
    let whole = PoolInstanceId::new(whole_key, PoolGeneration::new(0));

    assert_eq!(
        index.pools_for_address(vault),
        vec![instance_a.clone(), instance_b.clone()]
    );
    assert_eq!(index.pools_for_slot(slot_a), vec![instance_a.clone()]);
    assert_eq!(index.pools_for_slot(slot_b), vec![instance_b.clone()]);
    assert!(
        index
            .pools_for_slot(StateSlot::new(vault, U256::from(999)))
            .is_empty(),
        "association with a shared address is not ownership of every slot"
    );
    assert_eq!(
        index.pools_for_slot(StateSlot::new(whole_address, U256::from(999))),
        vec![whole]
    );
    assert_eq!(index.pools_for_emitter(emitter_a), vec![instance_a]);
    assert_eq!(index.pools_for_emitter(emitter_b), vec![instance_b]);
    Ok(())
}

struct MultiV3FamilyAdapter;

impl AmmAdapter for MultiV3FamilyAdapter {
    fn protocol(&self) -> ProtocolId {
        ProtocolId::UniswapV3
    }

    fn protocols(&self) -> Vec<ProtocolId> {
        vec![
            ProtocolId::Slipstream,
            ProtocolId::UniswapV3,
            ProtocolId::PancakeV3,
        ]
    }
}

#[test]
fn one_canonical_adapter_instance_owns_every_protocol_in_a_family() -> Result<()> {
    let uniswap = PoolKey::UniswapV3(Address::repeat_byte(0x31));
    let pancake = PoolKey::PancakeV3(Address::repeat_byte(0x32));
    let slipstream = PoolKey::Slipstream(Address::repeat_byte(0x33));
    let mut registry = AdapterRegistry::new();
    registry.register_adapter(Arc::new(MultiV3FamilyAdapter))?;
    for pool in [slipstream.clone(), pancake.clone(), uniswap.clone()] {
        registry.register_pool(PoolRegistration::new(pool))?;
    }

    let index = AmmOwnershipIndex::from_registry(&registry)?;
    let instances = [uniswap, pancake, slipstream]
        .map(|pool| PoolInstanceId::new(pool, PoolGeneration::new(0)));
    let adapter = index
        .adapter_for_pool(&instances[0])
        .cloned()
        .expect("family adapter");
    assert_eq!(index.adapter_for_pool(&instances[1]), Some(&adapter));
    assert_eq!(index.adapter_for_pool(&instances[2]), Some(&adapter));
    assert_eq!(
        adapter.key().protocols(),
        &[
            ProtocolId::UniswapV3,
            ProtocolId::PancakeV3,
            ProtocolId::Slipstream,
        ]
    );
    assert_eq!(index.pools_for_adapter(&adapter), instances.to_vec());
    Ok(())
}

#[test]
fn removing_one_pool_partitions_only_its_handler_jobs_and_resyncs() -> Result<()> {
    let vault = Address::repeat_byte(0xf1);
    let adapter = AdapterInstanceId::new(
        AdapterKey::new(ProtocolId::Custom(OWNERSHIP_PROTOCOL), []),
        AdapterGeneration::new(1),
    );
    let key_a = ownership_key(Address::repeat_byte(0xa2));
    let key_b = ownership_key(Address::repeat_byte(0xb2));
    let pool_a = PoolInstanceId::new(key_a.clone(), PoolGeneration::new(1));
    let pool_b = PoolInstanceId::new(key_b, PoolGeneration::new(1));
    let slot_a = StateSlot::new(vault, U256::from(11));
    let slot_b = StateSlot::new(vault, U256::from(12));
    let emitter_a = Address::repeat_byte(0xa3);
    let emitter_b = Address::repeat_byte(0xb3);

    let mut index = AmmOwnershipIndex::default();
    index.insert_adapter(adapter.clone())?;
    index.insert_pool(PoolOwnership::new(
        pool_b.clone(),
        adapter.clone(),
        PoolStateDependencies::default()
            .with_associated_addresses([vault])
            .with_slots([slot_b]),
        [emitter_b],
    )?)?;
    index.insert_pool(PoolOwnership::new(
        pool_a.clone(),
        adapter.clone(),
        PoolStateDependencies::default()
            .with_associated_addresses([vault])
            .with_slots([slot_a]),
        [emitter_a],
    )?)?;

    let work_a = RuntimeWorkId::new(RuntimeOwnerId::Pool(pool_a.clone()), WorkId::new(1));
    let work_b = RuntimeWorkId::new(RuntimeOwnerId::Pool(pool_b.clone()), WorkId::new(2));
    let resync_a = ResyncId::new("pool-a-repair");
    let resync_b = ResyncId::new("pool-b-repair");
    index.track_work(work_a.clone())?;
    index.track_work(work_b.clone())?;
    assert_eq!(
        index.track_work(work_a.clone()),
        Err(AmmOwnershipError::DuplicateWork(work_a.clone()))
    );
    index.track_resync(pool_a.clone(), resync_a.clone())?;
    index.track_resync(pool_b.clone(), resync_b.clone())?;
    assert_eq!(
        index.track_resync(pool_b.clone(), resync_a.clone()),
        Err(AmmOwnershipError::DuplicateResync(resync_a.clone()))
    );
    assert_eq!(index.resync_owner(&resync_a), Some(&pool_a));
    assert_eq!(
        index.remove_adapter(&adapter),
        Err(AmmOwnershipError::AdapterInUse(adapter.clone()))
    );
    assert_eq!(index.active_adapter(adapter.key()), Some(&adapter));

    assert_eq!(index.untrack_resync(&resync_b), Some(pool_b.clone()));
    assert!(index.resync_owner(&resync_b).is_none());
    assert!(index.resyncs_for_pool(&pool_b).is_empty());
    index.track_resync(pool_b.clone(), resync_b.clone())?;

    let removed = index.remove_pool(&pool_a).expect("pool A removed");
    assert_eq!(removed.ownership().instance(), &pool_a);
    assert_eq!(removed.work(), &[work_a]);
    assert_eq!(removed.resyncs(), std::slice::from_ref(&resync_a));
    assert_eq!(index.pools_for_address(vault), vec![pool_b.clone()]);
    assert!(index.pools_for_slot(slot_a).is_empty());
    assert_eq!(index.pools_for_slot(slot_b), vec![pool_b.clone()]);
    assert_eq!(
        index.work_for_owner(&RuntimeOwnerId::Pool(pool_b.clone())),
        vec![work_b]
    );
    assert_eq!(index.resync_owner(&resync_b), Some(&pool_b));
    assert!(index.resync_owner(&resync_a).is_none());

    let replacement = PoolInstanceId::new(key_a, PoolGeneration::new(2));
    index.insert_pool(PoolOwnership::new(
        replacement.clone(),
        adapter,
        PoolStateDependencies::default()
            .with_associated_addresses([vault])
            .with_slots([slot_a]),
        [emitter_a],
    )?)?;
    assert!(index.remove_pool(&pool_a).is_none());
    assert_eq!(index.active_pool(replacement.key()), Some(&replacement));
    Ok(())
}
