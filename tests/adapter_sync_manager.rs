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
    AdapterRegistry, AmmAdapter, AmmSyncEngine, CurveAdapter, CurveMetadata, CurveVariant, PoolKey,
    PoolRegistration, PoolStatus, ProtocolMetadata,
};
use evm_fork_cache::StorageFetchError;
use evm_fork_cache::cache::{
    BlockStateAccountDiff, BlockStateDiff, BlockStateStorageDiff, EvmCache,
};
use evm_fork_cache::reactive::{
    BlockRef, ChainStatus, InputSource, ReactiveContext, ReactiveInput, ReactiveInputBatch,
    ReactiveInputRecord,
};

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

fn curve_registry(pool: Address, slot: U256) -> Result<AdapterRegistry> {
    let dai = Address::repeat_byte(0x01);
    let usdc = Address::repeat_byte(0x02);
    let mut registration = PoolRegistration::new(PoolKey::Curve(pool))
        .with_state_address(pool)
        .with_status(PoolStatus::Ready)
        .with_metadata(ProtocolMetadata::Curve(
            CurveMetadata::default()
                .with_coins([dai, usdc])
                .with_discovered_slots([slot])
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

    let report = engine.ingest_batch(
        &mut cache,
        batch(curve_log(pool, Address::repeat_byte(0x01), 91), 91),
    )?;

    assert_eq!(report.reactive.applied.len(), 1);
    assert_eq!(report.resync_state_updates, 0);
    assert_eq!(report.resync_failures, 1);
    assert_eq!(report.degraded_pools, vec![PoolKey::Curve(pool)]);
    assert_eq!(cache.cached_storage_value(pool, slot), None);
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
