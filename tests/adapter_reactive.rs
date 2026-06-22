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
use evm_amm_state::adapters::storage::{
    V2_RESERVES_SLOT, V3_LIQUIDITY_SLOT, V3_SLOT0_SLOT, V3StorageLayout,
};
use evm_amm_state::adapters::{
    AdapterRegistry, AmmAdapter, AmmReactiveHandler, BalancerV2Adapter, BalancerV2Metadata,
    PoolKey, PoolRegistration, ProtocolMetadata, StateUpdate, UniswapV2Adapter, UniswapV3Adapter,
    V3Metadata,
};
use evm_fork_cache::cache::{EvmCache, StorageBatchFetchFn};
use evm_fork_cache::reactive::{
    BlockRef, ChainStatus, HandlerId, InputSource, ReactiveConfig, ReactiveInput,
    ReactiveInputBatch, ReactiveInputRecord, ReactiveInterest, ReactiveReport, ReactiveRuntime,
    ReportTag, ResyncBlock, ResyncTarget, RouteKey, RouteKeySpec, StateEffectQuality,
};

fn rpc_log(
    address: Address,
    topics: Vec<B256>,
    data: Vec<u8>,
    block_number: u64,
    tx_index: u64,
    log_index: u64,
) -> RpcLog {
    RpcLog {
        inner: PrimitiveLog::new_unchecked(address, topics, Bytes::from(data)),
        block_hash: Some(block_hash(block_number)),
        block_number: Some(block_number),
        block_timestamp: Some(1_700_000_000 + block_number),
        transaction_hash: Some(B256::repeat_byte((tx_index + 1) as u8)),
        transaction_index: Some(tx_index),
        log_index: Some(log_index),
        removed: false,
    }
}

fn block_hash(block_number: u64) -> B256 {
    B256::repeat_byte(block_number as u8)
}

fn block_ref(block_number: u64) -> BlockRef {
    BlockRef {
        number: block_number,
        hash: block_hash(block_number),
        parent_hash: Some(block_hash(block_number.saturating_sub(1))),
        timestamp: Some(1_700_000_000 + block_number),
    }
}

fn included_context(
    block_number: u64,
    log_index: u64,
) -> evm_fork_cache::reactive::ReactiveContext {
    let block = block_ref(block_number);
    evm_fork_cache::reactive::ReactiveContext {
        chain_id: Some(1),
        source: InputSource::Synthetic,
        chain_status: ChainStatus::Included {
            block: block.clone(),
            confirmations: 0,
        },
        block: Some(block),
        transaction_index: Some(0),
        log_index: Some(log_index),
    }
}

fn reorged_context(block_number: u64, log_index: u64) -> evm_fork_cache::reactive::ReactiveContext {
    let block = block_ref(block_number);
    evm_fork_cache::reactive::ReactiveContext {
        chain_id: Some(1),
        source: InputSource::Synthetic,
        chain_status: ChainStatus::Reorged {
            dropped_from: block.clone(),
        },
        block: Some(block),
        transaction_index: Some(0),
        log_index: Some(log_index),
    }
}

fn batch(
    inputs: Vec<(
        ReactiveInput<Ethereum>,
        evm_fork_cache::reactive::ReactiveContext,
    )>,
) -> ReactiveInputBatch<Ethereum> {
    ReactiveInputBatch::new(
        inputs
            .into_iter()
            .map(|(input, context)| ReactiveInputRecord::new(input, context))
            .collect(),
    )
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

fn v2_registry(pool: Address, store_sources: bool) -> AdapterRegistry {
    let adapter = Arc::new(UniswapV2Adapter::default());
    let mut registration = PoolRegistration::new(PoolKey::UniswapV2(pool)).with_state_address(pool);
    if store_sources {
        registration = registration.with_event_sources(adapter.event_sources(&registration));
    }

    let mut registry = AdapterRegistry::new();
    registry.register_adapter(adapter).unwrap();
    registry.register_pool(registration).unwrap();
    registry
}

fn v3_registry(pool: Address) -> AdapterRegistry {
    let adapter = Arc::new(UniswapV3Adapter::default());
    let mut registration = PoolRegistration::new(PoolKey::UniswapV3(pool))
        .with_state_address(pool)
        .with_metadata(ProtocolMetadata::UniswapV3(V3Metadata {
            storage_layout: Some(V3StorageLayout::uniswap(60)),
            ..Default::default()
        }));
    registration = registration.with_event_sources(adapter.event_sources(&registration));

    let mut registry = AdapterRegistry::new();
    registry.register_adapter(adapter).unwrap();
    registry.register_pool(registration).unwrap();
    registry
}

fn balancer_registry(vault: Address, pool_ids: impl IntoIterator<Item = B256>) -> AdapterRegistry {
    let adapter = Arc::new(BalancerV2Adapter::default());
    let mut registry = AdapterRegistry::new();
    registry.register_adapter(adapter.clone()).unwrap();

    for pool_id in pool_ids {
        let mut registration = PoolRegistration::new(PoolKey::BalancerV2(pool_id))
            .with_state_address(vault)
            .with_metadata(ProtocolMetadata::BalancerV2(BalancerV2Metadata {
                vault: Some(vault),
                ..Default::default()
            }));
        registration = registration.with_event_sources(adapter.event_sources(&registration));
        registry.register_pool(registration).unwrap();
    }

    registry
}

fn v2_sync_topic() -> B256 {
    keccak256("Sync(uint112,uint112)")
}

fn v3_swap_topic() -> B256 {
    keccak256("Swap(address,address,int256,int256,uint160,uint128,int24)")
}

fn v3_mint_topic() -> B256 {
    keccak256("Mint(address,address,int24,int24,uint128,uint256,uint256)")
}

fn balancer_swap_topic() -> B256 {
    keccak256("Swap(bytes32,address,address,uint256,uint256)")
}

fn word(value: U256) -> Vec<u8> {
    value.to_be_bytes::<32>().to_vec()
}

fn address_word(address: Address) -> Vec<u8> {
    let mut bytes = [0u8; 32];
    bytes[12..].copy_from_slice(address.as_slice());
    bytes.to_vec()
}

fn abi_words(values: impl IntoIterator<Item = U256>) -> Vec<u8> {
    values.into_iter().flat_map(word).collect()
}

fn topic_address(address: Address) -> B256 {
    let mut bytes = [0u8; 32];
    bytes[12..].copy_from_slice(address.as_slice());
    B256::from(bytes)
}

fn topic_i24(value: i32) -> B256 {
    let mut bytes = if value < 0 { [0xff; 32] } else { [0u8; 32] };
    let raw = value.to_be_bytes();
    bytes[29..32].copy_from_slice(&raw[1..4]);
    B256::from(bytes)
}

fn low_mask(bits: usize) -> U256 {
    (U256::from(1) << bits) - U256::from(1)
}

fn reserves_slot(reserve0: U256, reserve1: U256, timestamp: U256) -> U256 {
    reserve0 | (reserve1 << 112) | (timestamp << 224)
}

fn tag_value<'a>(tags: &'a [ReportTag], key: &str) -> Option<&'a str> {
    tags.iter()
        .find(|tag| tag.key == key)
        .map(|tag| tag.value.as_str())
}

#[test]
fn reactive_handler_builds_log_interests_from_adapter_event_sources() {
    let pool = Address::repeat_byte(0x21);
    let handler = AmmReactiveHandler::new(v2_registry(pool, false));
    let log = rpc_log(pool, vec![v2_sync_topic()], Vec::new(), 10, 0, 0);

    let matching_interest = handler
        .interests()
        .into_iter()
        .find_map(|interest| match interest {
            ReactiveInterest::Logs(interest) if interest.matches(&log) => Some(interest),
            _ => None,
        })
        .expect("adapter-derived V2 Sync interest should match the log");

    assert!(matches!(
        &matching_interest.route_key,
        Some(RouteKeySpec::EmitterAddress)
    ));
    assert_eq!(
        matching_interest.route_key(&log),
        Some(RouteKey::Address(pool))
    );
}

#[tokio::test]
async fn v2_sync_applies_masked_update_through_reactive_runtime() -> Result<()> {
    let pool = Address::repeat_byte(0x31);
    let reserve0 = U256::from(123_u64);
    let reserve1 = U256::from(456_u64);
    let timestamp = U256::from(0x1234_u64);
    let initial_slot = timestamp << 224;
    let log = rpc_log(
        pool,
        vec![v2_sync_topic()],
        abi_words([reserve0, reserve1]),
        11,
        0,
        0,
    );

    let mut cache = setup_cache().await?;
    cache.apply_updates(&[StateUpdate::slot(pool, V2_RESERVES_SLOT, initial_slot)]);

    let mut runtime = ReactiveRuntime::<Ethereum>::new(ReactiveConfig::default());
    runtime.register_handler(Arc::new(AmmReactiveHandler::new(v2_registry(pool, true))))?;

    let report = runtime.ingest_batch(
        &mut cache,
        batch(vec![(ReactiveInput::Log(log), included_context(11, 0))]),
    )?;

    let raw = cache.cached_storage_value(pool, V2_RESERVES_SLOT).unwrap();
    assert_eq!(raw & low_mask(112), reserve0);
    assert_eq!((raw >> 112) & low_mask(112), reserve1);
    assert_eq!(raw >> 224, timestamp);
    assert_eq!(report.applied.len(), 1);
    assert_eq!(
        report.applied[0].handler_id,
        HandlerId::new("evm-amm-state.adapters")
    );
    assert_eq!(
        report.applied[0].quality,
        StateEffectQuality::ExactFromInput
    );
    assert!(report.applied[0].resyncs.is_empty());
    assert!(
        report.applied[0]
            .hook_signals
            .iter()
            .any(|signal| signal.namespace.as_ref() == "evm-amm-state"
                && signal.kind.as_ref() == "amm.event")
    );
    Ok(())
}

#[tokio::test]
async fn v2_sync_cold_slot_emits_hash_pinned_resync_and_repairs_cache() -> Result<()> {
    let pool = Address::repeat_byte(0x32);
    let reserve0 = U256::from(1_000_u64);
    let reserve1 = U256::from(2_000_u64);
    let timestamp = U256::from(0xbeef_u64);
    let fetched_slot = reserves_slot(reserve0, reserve1, timestamp);
    let log = rpc_log(
        pool,
        vec![v2_sync_topic()],
        abi_words([reserve0, reserve1]),
        12,
        0,
        0,
    );

    let mut cache = setup_cache().await?;
    cache.set_storage_batch_fetcher(stub_fetcher(HashMap::from([(
        (pool, V2_RESERVES_SLOT),
        fetched_slot,
    )])));

    let mut runtime = ReactiveRuntime::<Ethereum>::new(ReactiveConfig::default());
    runtime.register_handler(Arc::new(AmmReactiveHandler::new(v2_registry(pool, true))))?;

    let report = runtime.ingest_batch_with_resync(
        &mut cache,
        batch(vec![(ReactiveInput::Log(log), included_context(12, 0))]),
    )?;

    assert_eq!(report.applied.len(), 1);
    assert_eq!(
        report.applied[0].quality,
        StateEffectQuality::AppliedWithPendingResync
    );
    assert!(report.applied[0].diff.has_skipped());
    assert_eq!(report.applied[0].resyncs.len(), 1);
    let resync = &report.applied[0].resyncs[0];
    assert!(matches!(
        resync.block,
        ResyncBlock::Hash {
            number: 12,
            hash,
            require_canonical: true,
        } if hash == block_hash(12)
    ));
    assert!(matches!(
        resync.targets.as_slice(),
        [ResyncTarget::StorageSlots { address, slots }]
            if *address == pool && slots == &vec![V2_RESERVES_SLOT]
    ));

    let resynced = report
        .reports
        .iter()
        .find_map(|report| match report.as_ref() {
            ReactiveReport::Resynced(report) => Some(report),
            _ => None,
        })
        .expect("ingest_batch_with_resync should execute the adapter resync");
    assert_eq!(
        resynced.state_updates,
        vec![StateUpdate::slot(pool, V2_RESERVES_SLOT, fetched_slot)]
    );
    assert_eq!(
        cache.cached_storage_value(pool, V2_RESERVES_SLOT),
        Some(fetched_slot)
    );
    Ok(())
}

#[tokio::test]
async fn v3_swap_applies_slot0_and_liquidity_updates_through_runtime() -> Result<()> {
    let pool = Address::repeat_byte(0x41);
    let sqrt_price = U256::from(12_345_u64);
    let liquidity = U256::from(67_890_u64);
    let tick = U256::from(42_u64);
    let preserved_high_bits = U256::from(0xabcdef_u64) << 184;
    let log = rpc_log(
        pool,
        vec![
            v3_swap_topic(),
            topic_address(Address::repeat_byte(0x01)),
            topic_address(Address::repeat_byte(0x02)),
        ],
        abi_words([U256::ZERO, U256::ZERO, sqrt_price, liquidity, tick]),
        13,
        0,
        0,
    );

    let mut cache = setup_cache().await?;
    cache.apply_updates(&[StateUpdate::slot(pool, V3_SLOT0_SLOT, preserved_high_bits)]);

    let mut runtime = ReactiveRuntime::<Ethereum>::new(ReactiveConfig::default());
    runtime.register_handler(Arc::new(AmmReactiveHandler::new(v3_registry(pool))))?;

    let report = runtime.ingest_batch(
        &mut cache,
        batch(vec![(ReactiveInput::Log(log), included_context(13, 0))]),
    )?;

    let raw_slot0 = cache.cached_storage_value(pool, V3_SLOT0_SLOT).unwrap();
    assert_eq!(raw_slot0 & low_mask(160), sqrt_price);
    assert_eq!((raw_slot0 >> 160) & low_mask(24), tick);
    assert_eq!(raw_slot0 & !low_mask(184), preserved_high_bits);
    assert_eq!(
        cache.cached_storage_value(pool, V3_LIQUIDITY_SLOT),
        Some(liquidity)
    );
    assert_eq!(
        report.applied[0].quality,
        StateEffectQuality::ExactFromInput
    );
    Ok(())
}

#[tokio::test]
async fn v3_mint_emits_tick_range_repair_hook() -> Result<()> {
    let pool = Address::repeat_byte(0x42);
    let tick_lower = 60;
    let tick_upper = 120;
    let mut data = address_word(Address::repeat_byte(0x03));
    data.extend(abi_words([
        U256::from(1_u64),
        U256::from(2_u64),
        U256::from(3_u64),
    ]));
    let log = rpc_log(
        pool,
        vec![
            v3_mint_topic(),
            topic_address(Address::repeat_byte(0x04)),
            topic_i24(tick_lower),
            topic_i24(tick_upper),
        ],
        data,
        14,
        0,
        0,
    );

    let mut cache = setup_cache().await?;
    let mut runtime = ReactiveRuntime::<Ethereum>::new(ReactiveConfig::default());
    runtime.register_handler(Arc::new(AmmReactiveHandler::new(v3_registry(pool))))?;

    let report = runtime.ingest_batch(
        &mut cache,
        batch(vec![(ReactiveInput::Log(log), included_context(14, 0))]),
    )?;

    assert_eq!(report.applied.len(), 1);
    assert_eq!(
        report.applied[0].quality,
        StateEffectQuality::RequiresRepair
    );
    assert!(report.applied[0].state_updates.is_empty());
    let repair_signal = report.applied[0]
        .hook_signals
        .iter()
        .find(|signal| {
            signal.namespace.as_ref() == "evm-amm-state"
                && signal.kind.as_ref() == "amm.repair.v3_tick_range"
        })
        .expect("V3 liquidity changes should emit a tick-range repair hook");
    assert_eq!(tag_value(&repair_signal.labels, "tick_lower"), Some("60"));
    assert_eq!(tag_value(&repair_signal.labels, "tick_upper"), Some("120"));
    Ok(())
}

#[tokio::test]
async fn balancer_shared_emitter_routes_by_pool_id() -> Result<()> {
    let vault = Address::repeat_byte(0x51);
    let pool_a = B256::repeat_byte(0xa1);
    let pool_b = B256::repeat_byte(0xb2);
    let log = rpc_log(
        vault,
        vec![
            balancer_swap_topic(),
            pool_b,
            topic_address(Address::repeat_byte(0x01)),
            topic_address(Address::repeat_byte(0x02)),
        ],
        abi_words([U256::from(10_u64), U256::from(11_u64)]),
        15,
        0,
        0,
    );

    let mut cache = setup_cache().await?;
    let mut runtime = ReactiveRuntime::<Ethereum>::new(ReactiveConfig::default());
    runtime.register_handler(Arc::new(AmmReactiveHandler::new(balancer_registry(
        vault,
        [pool_a, pool_b],
    ))))?;

    let report = runtime.ingest_batch(
        &mut cache,
        batch(vec![(ReactiveInput::Log(log), included_context(15, 0))]),
    )?;

    assert_eq!(report.applied.len(), 1);
    assert_eq!(
        tag_value(&report.applied[0].tags, "pool"),
        Some(format!("{:?}", PoolKey::BalancerV2(pool_b)).as_str())
    );
    assert_eq!(
        tag_value(&report.applied[0].tags, "protocol"),
        Some("BalancerV2")
    );
    assert!(report.applied[0].state_updates.is_empty());
    assert!(
        report.applied[0]
            .hook_signals
            .iter()
            .any(|signal| signal.kind.as_ref() == "amm.event")
    );
    Ok(())
}

#[tokio::test]
async fn removed_log_rolls_back_previously_applied_update() -> Result<()> {
    let pool = Address::repeat_byte(0x61);
    let initial_slot = U256::from(0x1234_u64) << 224;
    let reserve0 = U256::from(7_u64);
    let reserve1 = U256::from(8_u64);
    let mut log = rpc_log(
        pool,
        vec![v2_sync_topic()],
        abi_words([reserve0, reserve1]),
        16,
        0,
        0,
    );

    let mut cache = setup_cache().await?;
    cache.apply_updates(&[StateUpdate::slot(pool, V2_RESERVES_SLOT, initial_slot)]);

    let mut runtime = ReactiveRuntime::<Ethereum>::new(ReactiveConfig::default());
    runtime.register_handler(Arc::new(AmmReactiveHandler::new(v2_registry(pool, true))))?;

    runtime.ingest_batch(
        &mut cache,
        batch(vec![(
            ReactiveInput::Log(log.clone()),
            included_context(16, 0),
        )]),
    )?;
    assert_ne!(
        cache.cached_storage_value(pool, V2_RESERVES_SLOT),
        Some(initial_slot)
    );

    log.removed = true;
    let report = runtime.ingest_batch(
        &mut cache,
        batch(vec![(ReactiveInput::Log(log), reorged_context(16, 0))]),
    )?;

    assert!(report.applied.is_empty());
    let reorg = report
        .reports
        .iter()
        .find_map(|report| match report.as_ref() {
            ReactiveReport::Reorg(report) => Some(report),
            _ => None,
        })
        .expect("removed log should produce a reorg recovery report");
    assert_eq!(reorg.dropped_blocks, vec![block_ref(16)]);
    assert!(!reorg.rollback_updates.is_empty());
    assert_eq!(
        cache.cached_storage_value(pool, V2_RESERVES_SLOT),
        Some(initial_slot)
    );
    Ok(())
}
