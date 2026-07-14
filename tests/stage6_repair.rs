use std::sync::Arc;

use alloy_network::{AnyNetwork, Ethereum};
use alloy_primitives::{Address, B256, Bytes, Log as PrimitiveLog};
use alloy_provider::RootProvider;
use alloy_rpc_client::RpcClient;
use alloy_rpc_types_eth::Log as RpcLog;
use alloy_transport::mock::Asserter;
use anyhow::Result;
use evm_amm_state::adapters::{
    AdapterEvent, AdapterEventKind, AdapterEventResult, AdapterRegistry, AmmAdapter, AmmSyncEngine,
    ColdStartPolicy, CustomPoolKey, EventSource, PoolKey, PoolRegistration, PoolStatus, ProtocolId,
    RepairAction, StateView, UpdateQuality,
};
use evm_fork_cache::cache::EvmCache;
use evm_fork_cache::reactive::{
    BlockRef, ChainStatus, InputSource, ReactiveContext, ReactiveInput, ReactiveInputBatch,
    ReactiveInputRecord,
};

const PROTOCOL: &str = "stage6-repair";

struct ColdStartRepairAdapter {
    topic: B256,
}

impl AmmAdapter for ColdStartRepairAdapter {
    fn protocol(&self) -> ProtocolId {
        ProtocolId::Custom(PROTOCOL)
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
                UpdateQuality::RequiresRepair,
            )
            .with_repair(RepairAction::ColdStart {
                pool: pool.key.clone(),
                policy: ColdStartPolicy::Eager,
            }),
        )
    }
}

async fn cache() -> EvmCache {
    let provider = RootProvider::<AnyNetwork>::new(RpcClient::mocked(Asserter::new()));
    EvmCache::new(Arc::new(provider)).await
}

fn pool_key(address: Address) -> PoolKey {
    PoolKey::Custom(CustomPoolKey::Address {
        protocol: PROTOCOL,
        address,
    })
}

fn record(
    address: Address,
    topic: B256,
    block_number: u64,
    log_index: u64,
) -> ReactiveInputRecord<Ethereum> {
    let block = BlockRef {
        number: block_number,
        hash: B256::repeat_byte(block_number as u8),
        parent_hash: Some(B256::repeat_byte(block_number.saturating_sub(1) as u8)),
        timestamp: Some(1_700_000_000 + block_number),
    };
    let log = RpcLog {
        inner: PrimitiveLog::new_unchecked(address, vec![topic], Bytes::new()),
        block_hash: Some(block.hash),
        block_number: Some(block.number),
        block_timestamp: block.timestamp,
        transaction_hash: Some(B256::repeat_byte(0x44)),
        transaction_index: Some(0),
        log_index: Some(log_index),
        removed: false,
    };
    ReactiveInputRecord::new(
        ReactiveInput::Log(log),
        ReactiveContext {
            chain_id: Some(1),
            source: InputSource::Synthetic,
            chain_status: ChainStatus::Included {
                block: block.clone(),
                confirmations: 0,
            },
            block: Some(block),
            transaction_index: Some(0),
            log_index: Some(log_index),
        },
    )
}

#[tokio::test]
async fn sync_report_exposes_generation_scoped_cold_start_repair() -> Result<()> {
    let address = Address::repeat_byte(0x71);
    let topic = B256::repeat_byte(0x72);
    let key = pool_key(address);
    let mut registry = AdapterRegistry::new();
    registry.register_adapter(Arc::new(ColdStartRepairAdapter { topic }))?;
    registry.register_pool(
        PoolRegistration::new(key.clone())
            .with_state_address(address)
            .with_status(PoolStatus::Ready),
    )?;
    let mut engine = AmmSyncEngine::new(registry)?;
    let instance = engine
        .ownership()
        .active_pool(&key)
        .expect("the tracked pool has an exact generation")
        .clone();

    let report = engine.ingest_batch(
        &mut cache().await,
        ReactiveInputBatch::new(vec![record(address, topic, 1, 0)]),
    )?;

    assert_eq!(report.pending_repairs().len(), 1);
    assert_eq!(report.pending_repairs()[0].pool(), &instance);
    assert_eq!(
        report.pending_repairs()[0].action(),
        &RepairAction::ColdStart {
            pool: key,
            policy: ColdStartPolicy::Eager,
        }
    );
    Ok(())
}

#[tokio::test]
async fn sync_report_deduplicates_identical_generation_scoped_repairs() -> Result<()> {
    let address = Address::repeat_byte(0x73);
    let topic = B256::repeat_byte(0x74);
    let key = pool_key(address);
    let mut registry = AdapterRegistry::new();
    registry.register_adapter(Arc::new(ColdStartRepairAdapter { topic }))?;
    registry.register_pool(
        PoolRegistration::new(key.clone())
            .with_state_address(address)
            .with_status(PoolStatus::Ready),
    )?;
    let mut engine = AmmSyncEngine::new(registry)?;

    let report = engine.ingest_batch(
        &mut cache().await,
        ReactiveInputBatch::new(vec![
            record(address, topic, 1, 0),
            record(address, topic, 1, 1),
        ]),
    )?;

    assert_eq!(report.pending_repairs().len(), 1);
    assert_eq!(
        report.pending_repairs()[0].action(),
        &RepairAction::ColdStart {
            pool: key,
            policy: ColdStartPolicy::Eager,
        }
    );
    Ok(())
}

#[tokio::test]
async fn sync_report_orders_pending_repairs_by_exact_pool_generation() -> Result<()> {
    let address_a = Address::repeat_byte(0x75);
    let address_b = Address::repeat_byte(0x76);
    let topic = B256::repeat_byte(0x77);
    let key_a = pool_key(address_a);
    let key_b = pool_key(address_b);
    let mut registry = AdapterRegistry::new();
    registry.register_adapter(Arc::new(ColdStartRepairAdapter { topic }))?;
    for (key, address) in [(&key_b, address_b), (&key_a, address_a)] {
        registry.register_pool(
            PoolRegistration::new(key.clone())
                .with_state_address(address)
                .with_status(PoolStatus::Ready),
        )?;
    }
    let mut engine = AmmSyncEngine::new(registry)?;
    let expected = [
        engine
            .ownership()
            .active_pool(&key_a)
            .expect("pool A is active")
            .clone(),
        engine
            .ownership()
            .active_pool(&key_b)
            .expect("pool B is active")
            .clone(),
    ];

    let report = engine.ingest_batch(
        &mut cache().await,
        ReactiveInputBatch::new(vec![
            record(address_b, topic, 1, 0),
            record(address_a, topic, 1, 1),
        ]),
    )?;

    assert_eq!(
        report
            .pending_repairs()
            .iter()
            .map(|repair| repair.pool().clone())
            .collect::<Vec<_>>(),
        expected
    );
    Ok(())
}
