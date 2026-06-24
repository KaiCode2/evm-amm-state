//! End-to-end pipeline tests on the adapters path: register → cold-start
//! (snapshot) → reactive event → cache mutation, composed on a SHARED
//! `EvmCache` + registry. The existing `adapter_reactive.rs` covers reactive
//! apply in isolation and `cold_start_adoption.rs` covers cold-start in
//! isolation; this file pins that they compose — the gap the readiness audit
//! flagged. (WS1b will extend this file with register→cold-start→react→simulate
//! once the swap-sim surface lands.)

use std::collections::HashMap;
use std::sync::Arc;

use alloy_eips::BlockId;
use alloy_network::Ethereum;
use alloy_primitives::{Address, B256, Bytes, Log as PrimitiveLog, U256, keccak256};
use alloy_provider::{RootProvider, network::AnyNetwork};
use alloy_rpc_client::RpcClient;
use alloy_rpc_types_eth::Log as RpcLog;
use alloy_transport::mock::Asserter;
use anyhow::Result;
use evm_amm_state::adapters::storage::{V2_RESERVES_SLOT, V2_TOKEN0_SLOT, V2_TOKEN1_SLOT};
use evm_amm_state::adapters::{
    AdapterRegistry, AmmAdapter, AmmReactiveHandler, ColdStartOutcome, ColdStartPolicy, PoolKey,
    PoolRegistration, PoolStatus, UniswapV2Adapter,
};
use evm_fork_cache::cache::{EvmCache, StorageBatchFetchFn};
use evm_fork_cache::reactive::{
    BlockRef, ChainStatus, InputSource, ReactiveConfig, ReactiveContext, ReactiveInput,
    ReactiveInputBatch, ReactiveInputRecord, ReactiveRuntime,
};

// --- harness (mirrors tests/adapter_reactive.rs conventions) ---

fn block_hash(block_number: u64) -> B256 {
    B256::repeat_byte(block_number as u8)
}

fn rpc_log(address: Address, topics: Vec<B256>, data: Vec<u8>, block_number: u64) -> RpcLog {
    RpcLog {
        inner: PrimitiveLog::new_unchecked(address, topics, Bytes::from(data)),
        block_hash: Some(block_hash(block_number)),
        block_number: Some(block_number),
        block_timestamp: Some(1_700_000_000 + block_number),
        transaction_hash: Some(B256::repeat_byte(1)),
        transaction_index: Some(0),
        log_index: Some(0),
        removed: false,
    }
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

fn stub_fetcher(values: HashMap<(Address, U256), U256>) -> StorageBatchFetchFn {
    Arc::new(
        move |requests: Vec<(Address, U256)>, _block: Option<BlockId>| {
            requests
                .into_iter()
                .map(|(address, slot)| {
                    let value = values.get(&(address, slot)).copied().unwrap_or_default();
                    (address, slot, Ok(value))
                })
                .collect()
        },
    )
}

fn v2_sync_topic() -> B256 {
    keccak256("Sync(uint112,uint112)")
}

fn token_word(address: Address) -> U256 {
    U256::from_be_slice(address.as_slice())
}

fn abi_words(values: impl IntoIterator<Item = U256>) -> Vec<u8> {
    values
        .into_iter()
        .flat_map(|v| v.to_be_bytes::<32>().to_vec())
        .collect()
}

fn low_mask(bits: usize) -> U256 {
    (U256::from(1) << bits) - U256::from(1)
}

/// Cold-start warms a V2 pool's reserves slot to its on-chain value, then a
/// reactive `Sync` event mutates that SAME warmed slot in the SAME cache.
#[tokio::test(flavor = "multi_thread")]
async fn v2_cold_start_then_reactive_sync_updates_warmed_reserves() -> Result<()> {
    let pool = Address::repeat_byte(0x91);
    let token0 = Address::repeat_byte(0xa0);
    let token1 = Address::repeat_byte(0xa1);

    // On-chain reserves the cold-start snapshot warms (timestamp in the top 32 bits).
    let cs_reserve0 = U256::from(1_000_u64);
    let cs_reserve1 = U256::from(2_000_u64);
    let cs_timestamp = U256::from(0x1234_u64);
    let cs_slot = cs_reserve0 | (cs_reserve1 << 112) | (cs_timestamp << 224);

    let adapter = Arc::new(UniswapV2Adapter::default());
    let mut registry = AdapterRegistry::new();
    registry.register_adapter(adapter.clone()).unwrap();

    let mut cache = setup_cache().await?;
    cache.set_storage_batch_fetcher(stub_fetcher(HashMap::from([
        ((pool, V2_RESERVES_SLOT), cs_slot),
        ((pool, V2_TOKEN0_SLOT), token_word(token0)),
        ((pool, V2_TOKEN1_SLOT), token_word(token1)),
    ])));

    // 1) Cold-start (snapshot) on an external registration.
    let mut registration = PoolRegistration::new(PoolKey::UniswapV2(pool)).with_state_address(pool);
    let outcome = registry.cold_start(&mut registration, &mut cache, ColdStartPolicy::Eager)?;
    assert!(
        matches!(outcome, ColdStartOutcome::Ready(_)),
        "cold-start should reach Ready, got {outcome:?}"
    );
    assert_eq!(registration.status, PoolStatus::Ready);
    assert_eq!(
        cache.cached_storage_value(pool, V2_RESERVES_SLOT),
        Some(cs_slot),
        "cold-start must warm the reserves slot to the on-chain value"
    );

    // 2) Register the cold-started pool for reactive routing and ingest a Sync
    //    carrying NEW reserves on the SAME cache + registry.
    let sources = adapter.event_sources(&registration);
    registry
        .register_pool(registration.with_event_sources(sources))
        .unwrap();

    let new_reserve0 = U256::from(1_500_u64);
    let new_reserve1 = U256::from(2_500_u64);
    let log = rpc_log(
        pool,
        vec![v2_sync_topic()],
        abi_words([new_reserve0, new_reserve1]),
        12,
    );

    let mut runtime = ReactiveRuntime::<Ethereum>::new(ReactiveConfig::default());
    runtime.register_handler(Arc::new(AmmReactiveHandler::new(registry)))?;
    let report = runtime.ingest_batch(&mut cache, batch(log, 12))?;
    assert_eq!(report.applied.len(), 1, "the Sync must apply exactly once");

    // 3) The cold-start-warmed slot now reflects the reactive event: low 224 bits
    //    overwritten with the new reserves, the timestamp high bits preserved.
    let raw = cache
        .cached_storage_value(pool, V2_RESERVES_SLOT)
        .expect("reserves slot is warm");
    assert_eq!(
        raw & low_mask(112),
        new_reserve0,
        "reserve0 updated by Sync"
    );
    assert_eq!(
        (raw >> 112) & low_mask(112),
        new_reserve1,
        "reserve1 updated by Sync"
    );
    assert_eq!(
        raw >> 224,
        cs_timestamp,
        "the masked Sync update preserves the cold-start timestamp bits"
    );
    Ok(())
}
