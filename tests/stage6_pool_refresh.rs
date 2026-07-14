use std::sync::Arc;

use alloy_network::{AnyNetwork, Ethereum};
use alloy_primitives::{Address, B256, Bytes, Log as PrimitiveLog, U256};
use alloy_provider::RootProvider;
use alloy_rpc_client::RpcClient;
use alloy_rpc_types_eth::Log as RpcLog;
use alloy_transport::mock::Asserter;
use anyhow::Result;
use evm_amm_state::adapters::{
    AdapterEvent, AdapterEventKind, AdapterEventResult, AdapterRegistry, AmmAdapter, AmmSyncEngine,
    AmmSyncError, CustomPoolKey, EventSource, PoolKey, PoolRegistration, PoolStateDependencies,
    PoolStatus, ProtocolId, ProtocolMetadata, StateSlot, StateUpdate, StateView, UpdateQuality,
};
use evm_fork_cache::cache::EvmCache;
use evm_fork_cache::reactive::{
    BlockRef, ChainStatus, InputSource, ReactiveContext, ReactiveInput, ReactiveInputBatch,
    ReactiveInputRecord,
};

const PROTOCOL: &str = "stage6-pool-refresh";

#[derive(Debug)]
struct RefreshMetadata {
    emitter: Address,
    state: Address,
    slot: U256,
}

struct RefreshAdapter {
    topic: B256,
}

impl RefreshAdapter {
    fn metadata(pool: &PoolRegistration) -> &RefreshMetadata {
        let ProtocolMetadata::Custom(metadata) = &pool.metadata else {
            panic!("refresh test registration has custom metadata");
        };
        metadata
            .downcast_ref::<RefreshMetadata>()
            .expect("refresh test metadata has the expected type")
    }
}

impl AmmAdapter for RefreshAdapter {
    fn protocol(&self) -> ProtocolId {
        ProtocolId::Custom(PROTOCOL)
    }

    fn event_sources(&self, pool: &PoolRegistration) -> Vec<EventSource> {
        vec![EventSource::direct(
            Self::metadata(pool).emitter,
            vec![self.topic],
        )]
    }

    fn state_dependencies(&self, pool: &PoolRegistration) -> PoolStateDependencies {
        let metadata = Self::metadata(pool);
        PoolStateDependencies::default()
            .with_associated_addresses([metadata.state])
            .with_slots([StateSlot::new(metadata.state, metadata.slot)])
    }

    fn decode_event(
        &self,
        pool: &PoolRegistration,
        log: &PrimitiveLog,
        _view: &dyn StateView,
    ) -> AdapterEventResult {
        let metadata = Self::metadata(pool);
        AdapterEventResult::event(
            AdapterEvent::new(
                pool.key.clone(),
                log.address,
                self.topic,
                AdapterEventKind::Unknown,
                UpdateQuality::Exact,
            )
            .with_updates([StateUpdate::slot(
                metadata.state,
                metadata.slot,
                U256::from(1),
            )]),
        )
    }
}

fn key(address: Address) -> PoolKey {
    PoolKey::Custom(CustomPoolKey::Address {
        protocol: PROTOCOL,
        address,
    })
}

fn registration(key: PoolKey, emitter: Address, state: Address, slot: U256) -> PoolRegistration {
    PoolRegistration::new(key)
        .with_metadata(ProtocolMetadata::Custom(Arc::new(RefreshMetadata {
            emitter,
            state,
            slot,
        })))
        .with_status(PoolStatus::Ready)
}

async fn cache() -> EvmCache {
    let provider = RootProvider::<AnyNetwork>::new(RpcClient::mocked(Asserter::new()));
    EvmCache::new(Arc::new(provider)).await
}

fn log_record(
    emitter: Address,
    topic: B256,
    block_number: u64,
    removed: bool,
) -> ReactiveInputRecord<Ethereum> {
    let block = BlockRef {
        number: block_number,
        hash: B256::repeat_byte(block_number as u8),
        parent_hash: Some(B256::repeat_byte(block_number.saturating_sub(1) as u8)),
        timestamp: Some(block_number),
    };
    let log = RpcLog {
        inner: PrimitiveLog::new_unchecked(emitter, vec![topic], Bytes::new()),
        block_hash: Some(block.hash),
        block_number: Some(block.number),
        block_timestamp: block.timestamp,
        transaction_hash: Some(B256::repeat_byte(0x91)),
        transaction_index: Some(0),
        log_index: Some(0),
        removed,
    };
    ReactiveInputRecord::new(
        ReactiveInput::Log(log),
        ReactiveContext {
            chain_id: Some(1),
            source: InputSource::Synthetic,
            chain_status: if removed {
                ChainStatus::Reorged {
                    dropped_from: block.clone(),
                }
            } else {
                ChainStatus::Included {
                    block: block.clone(),
                    confirmations: 0,
                }
            },
            block: Some(block),
            transaction_index: Some(0),
            log_index: Some(0),
        },
    )
}

#[tokio::test]
async fn refresh_rebuilds_same_generation_metadata_handler_and_ownership() -> Result<()> {
    let pool = Address::repeat_byte(0x81);
    let old_emitter = Address::repeat_byte(0x82);
    let new_emitter = Address::repeat_byte(0x83);
    let old_state = Address::repeat_byte(0x84);
    let new_state = Address::repeat_byte(0x85);
    let old_slot = U256::from(1);
    let new_slot = U256::from(2);
    let topic = B256::repeat_byte(0x86);
    let key = key(pool);
    let mut registry = AdapterRegistry::new();
    registry.register_adapter(Arc::new(RefreshAdapter { topic }))?;
    registry.register_pool(registration(key.clone(), old_emitter, old_state, old_slot))?;
    let mut engine = AmmSyncEngine::new(registry)?;
    let instance = engine
        .ownership()
        .active_pool(&key)
        .expect("pool is active")
        .clone();
    let replacement = registration(key.clone(), new_emitter, new_state, new_slot);

    let prepared = engine.prepare_pool_refresh(instance.clone(), replacement)?;
    assert_eq!(prepared.instance(), &instance);
    assert_eq!(
        prepared.previous_subscription().handler(),
        prepared.replacement_subscription().handler()
    );
    assert_eq!(prepared.previous_subscription().interests().len(), 1);
    assert_eq!(prepared.replacement_subscription().interests().len(), 1);

    let refreshed = engine.commit_pool_refresh(prepared)?;

    assert_eq!(refreshed.instance(), &instance);
    assert_eq!(engine.ownership().active_pool(&key), Some(&instance));
    assert!(
        engine
            .ownership()
            .pools_for_slot(StateSlot::new(old_state, old_slot))
            .is_empty()
    );
    assert_eq!(
        engine
            .ownership()
            .pools_for_slot(StateSlot::new(new_state, new_slot)),
        vec![instance.clone()]
    );
    assert!(engine.ownership().pools_for_emitter(old_emitter).is_empty());
    assert_eq!(
        engine.ownership().pools_for_emitter(new_emitter),
        vec![instance]
    );
    let active = engine
        .registry()
        .pool(&key)
        .expect("pool remains registered");
    let metadata = RefreshAdapter::metadata(active);
    assert_eq!(metadata.emitter, new_emitter);
    assert_eq!(metadata.state, new_state);
    assert_eq!(metadata.slot, new_slot);

    let mut cache = cache().await;
    let old = engine.ingest_batch(
        &mut cache,
        ReactiveInputBatch::new(vec![log_record(old_emitter, topic, 1, false)]),
    )?;
    assert!(old.affected_pools.is_empty());
    let new = engine.ingest_batch(
        &mut cache,
        ReactiveInputBatch::new(vec![log_record(new_emitter, topic, 2, false)]),
    )?;
    assert_eq!(new.affected_pools, vec![key]);
    assert_eq!(
        cache.cached_storage_value(new_state, new_slot),
        Some(U256::from(1))
    );
    Ok(())
}

#[test]
fn prepared_refresh_cannot_mutate_a_replacement_generation() -> Result<()> {
    let pool = Address::repeat_byte(0x87);
    let emitter = Address::repeat_byte(0x88);
    let state = Address::repeat_byte(0x89);
    let topic = B256::repeat_byte(0x8a);
    let key = key(pool);
    let mut registry = AdapterRegistry::new();
    registry.register_adapter(Arc::new(RefreshAdapter { topic }))?;
    registry.register_pool(registration(key.clone(), emitter, state, U256::from(1)))?;
    let mut engine = AmmSyncEngine::new(registry)?;
    let old = engine
        .ownership()
        .active_pool(&key)
        .expect("original generation is active")
        .clone();
    let prepared = engine.prepare_pool_refresh(
        old.clone(),
        registration(
            key.clone(),
            Address::repeat_byte(0x8b),
            Address::repeat_byte(0x8c),
            U256::from(2),
        ),
    )?;

    engine.remove_pools(std::slice::from_ref(&key))?;
    let replacement_registration = registration(
        key.clone(),
        Address::repeat_byte(0x8d),
        Address::repeat_byte(0x8e),
        U256::from(3),
    );
    engine.add_pools([replacement_registration])?;
    let replacement = engine
        .ownership()
        .active_pool(&key)
        .expect("replacement generation is active")
        .clone();
    assert_ne!(replacement, old);

    assert!(matches!(
        engine.commit_pool_refresh(prepared),
        Err(AmmSyncError::StalePoolRefresh(instance)) if instance == old
    ));
    assert_eq!(engine.ownership().active_pool(&key), Some(&replacement));
    let active = engine
        .registry()
        .pool(&key)
        .expect("replacement remains active");
    assert_eq!(RefreshAdapter::metadata(active).slot, U256::from(3));
    Ok(())
}

#[test]
fn invalid_refresh_validation_leaves_every_active_view_unchanged() -> Result<()> {
    let pool = Address::repeat_byte(0x97);
    let emitter = Address::repeat_byte(0x98);
    let state = Address::repeat_byte(0x99);
    let slot = U256::from(1);
    let topic = B256::repeat_byte(0x9a);
    let key = key(pool);
    let mut registry = AdapterRegistry::new();
    registry.register_adapter(Arc::new(RefreshAdapter { topic }))?;
    registry.register_pool(registration(key.clone(), emitter, state, slot))?;
    let engine = AmmSyncEngine::new(registry)?;
    let instance = engine
        .ownership()
        .active_pool(&key)
        .expect("pool is active")
        .clone();
    let invalid = registration(
        key.clone(),
        Address::repeat_byte(0x9b),
        Address::repeat_byte(0x9c),
        U256::from(2),
    )
    .with_status(PoolStatus::Degraded);

    assert!(matches!(
        engine.prepare_pool_refresh(instance.clone(), invalid),
        Err(AmmSyncError::PoolRefreshNotReady(failed)) if failed == key
    ));
    assert_eq!(engine.ownership().active_pool(&key), Some(&instance));
    assert_eq!(
        engine
            .ownership()
            .pools_for_slot(StateSlot::new(state, slot)),
        vec![instance]
    );
    let active = engine
        .registry()
        .pool(&key)
        .expect("original remains active");
    assert_eq!(RefreshAdapter::metadata(active).emitter, emitter);
    assert_eq!(RefreshAdapter::metadata(active).state, state);
    assert_eq!(RefreshAdapter::metadata(active).slot, slot);
    Ok(())
}

#[tokio::test]
async fn refresh_preserves_the_existing_reorg_journal() -> Result<()> {
    let pool = Address::repeat_byte(0x92);
    let old_emitter = Address::repeat_byte(0x93);
    let new_emitter = Address::repeat_byte(0x94);
    let state = Address::repeat_byte(0x95);
    let old_slot = U256::from(1);
    let new_slot = U256::from(2);
    let topic = B256::repeat_byte(0x96);
    let key = key(pool);
    let mut registry = AdapterRegistry::new();
    registry.register_adapter(Arc::new(RefreshAdapter { topic }))?;
    registry.register_pool(registration(key.clone(), old_emitter, state, old_slot))?;
    let mut engine = AmmSyncEngine::new(registry)?;
    let instance = engine
        .ownership()
        .active_pool(&key)
        .expect("pool is active")
        .clone();
    let mut cache = cache().await;

    engine.ingest_batch(
        &mut cache,
        ReactiveInputBatch::new(vec![log_record(old_emitter, topic, 1, false)]),
    )?;
    assert_eq!(
        cache.cached_storage_value(state, old_slot),
        Some(U256::from(1))
    );

    let prepared =
        engine.prepare_pool_refresh(instance, registration(key, new_emitter, state, new_slot))?;
    engine.commit_pool_refresh(prepared)?;
    engine.ingest_batch(
        &mut cache,
        ReactiveInputBatch::new(vec![log_record(old_emitter, topic, 1, true)]),
    )?;

    assert_eq!(
        cache.cached_storage_value(state, old_slot),
        Some(U256::ZERO)
    );
    Ok(())
}
