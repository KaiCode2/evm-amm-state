use std::collections::HashMap;
use std::sync::Arc;

use alloy_eips::BlockId;
use alloy_network::Ethereum;
use alloy_primitives::{Address, B256, Bytes, Log as PrimitiveLog, U256, hex, keccak256};
use alloy_provider::{RootProvider, network::AnyNetwork};
use alloy_rpc_client::RpcClient;
use alloy_rpc_types_eth::Log as RpcLog;
use alloy_transport::mock::Asserter;
use anyhow::Result;
use evm_amm_state::adapters::storage::{
    SolidlyStorageLayout, V2_RESERVES_SLOT, V3_LIQUIDITY_SLOT, V3_SLOT0_SLOT, V3StorageLayout,
    v3_tick_bitmap_storage_key_with_base, v3_tick_info_storage_keys_with_base,
};
use evm_amm_state::adapters::{
    AdapterRegistry, AmmAdapter, AmmReactiveHandler, BalancerV2Adapter, BalancerV2Metadata,
    ColdStartOutcome, ColdStartPolicy, ConcentratedLiquidityAdapter, CurveAdapter, CurveMetadata,
    CurveVariant, DeferredWork, PoolKey, PoolRegistration, PoolStatus, ProtocolMetadata,
    SolidlyV2Adapter, SolidlyV2Metadata, UniswapV2Adapter, UniswapV2Metadata, V3Metadata,
    uniswap_v2_pair_runtime_code_hash,
};
// The reactive-runtime seam these tests exercise (raw `EvmCache::apply_updates`
// and the upstream `ResyncedReport`/`InvalidationRequest`) speaks the upstream
// state vocabulary, so these are the `evm_fork_cache` types, not the crate mirrors.
use evm_fork_cache::AccountFieldsSample;
use evm_fork_cache::PurgeScope;
use evm_fork_cache::StateUpdate;
use evm_fork_cache::cache::{AccountFieldsFetchFn, EvmCache, StorageBatchFetchFn};
use evm_fork_cache::reactive::{
    BlockRef, ChainStatus, HandlerId, InputSource, ReactiveConfig, ReactiveInput,
    ReactiveInputBatch, ReactiveInputRecord, ReactiveInterest, ReactiveReport, ReactiveRuntime,
    ReportTag, ResyncBlock, ResyncReason, ResyncTarget, RouteKey, RouteKeySpec, StateEffectQuality,
};
use revm::state::{AccountInfo, Bytecode};

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
    Arc::new(move |requests: Vec<(Address, U256)>, _block: BlockId| {
        requests
            .into_iter()
            .map(|(address, slot)| {
                let value = values.get(&(address, slot)).copied().unwrap_or_default();
                (address, slot, Ok(value))
            })
            .collect()
    })
}

fn account_fields_fetcher(samples: HashMap<Address, (U256, B256)>) -> AccountFieldsFetchFn {
    Arc::new(move |addresses: Vec<Address>, _block: BlockId| {
        Ok(addresses
            .into_iter()
            .map(|address| {
                let (balance, code_hash) = samples.get(&address).copied().unwrap_or_default();
                (address, AccountFieldsSample { balance, code_hash })
            })
            .collect())
    })
}

fn v2_registry(pool: Address, store_sources: bool) -> AdapterRegistry {
    let adapter = Arc::new(UniswapV2Adapter::default());
    let mut registration = PoolRegistration::new(PoolKey::UniswapV2(pool)).with_state_address(pool);
    if store_sources {
        let sources = adapter.event_sources(&registration);
        registration = registration.with_event_sources(sources);
    }

    let mut registry = AdapterRegistry::new();
    registry.register_adapter(adapter).unwrap();
    registry.register_pool(registration).unwrap();
    registry
}

fn v3_registry(pool: Address) -> AdapterRegistry {
    let adapter = Arc::new(ConcentratedLiquidityAdapter::default());
    let mut registration = PoolRegistration::new(PoolKey::UniswapV3(pool))
        .with_state_address(pool)
        .with_metadata(ProtocolMetadata::UniswapV3(
            V3Metadata::default().with_storage_layout(V3StorageLayout::uniswap(60)),
        ));
    let sources = adapter.event_sources(&registration);
    registration = registration.with_event_sources(sources);

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
            .with_metadata(ProtocolMetadata::BalancerV2(
                BalancerV2Metadata::default().with_vault(vault),
            ));
        let sources = adapter.event_sources(&registration);
        registration = registration.with_event_sources(sources);
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

fn v3_burn_topic() -> B256 {
    keccak256("Burn(address,int24,int24,uint128,uint256,uint256)")
}

/// Bitmap word position for a tick, matching the V3 contract storage layout
/// (`floor(tick / tick_spacing)` then `div_euclid(256)`).
fn word_pos(tick: i32, tick_spacing: i32) -> i16 {
    tick.div_euclid(tick_spacing).div_euclid(256) as i16
}

/// Independent oracle for the slot set a V3 liquidity-event repair must resync:
/// the boundary `Tick.Info` slots {0, 3}, the (deduped) containing bitmap words,
/// and the global liquidity slot. Returned sorted and deduped.
fn expected_tick_repair_slots(
    layout: V3StorageLayout,
    tick_lower: i32,
    tick_upper: i32,
) -> Vec<U256> {
    let mut slots = Vec::new();
    for tick in [tick_lower, tick_upper] {
        let keys = v3_tick_info_storage_keys_with_base(tick, layout.ticks_base_slot);
        slots.push(keys[0]);
        slots.push(keys[3]);
    }
    let mut words = vec![
        word_pos(tick_lower, layout.tick_spacing),
        word_pos(tick_upper, layout.tick_spacing),
    ];
    words.sort_unstable();
    words.dedup();
    for word in words {
        slots.push(v3_tick_bitmap_storage_key_with_base(
            word,
            layout.tick_bitmap_base_slot,
        ));
    }
    slots.push(layout.liquidity_slot);
    slots.sort_unstable();
    slots.dedup();
    slots
}

fn v3_mint_log(pool: Address, tick_lower: i32, tick_upper: i32, block_number: u64) -> RpcLog {
    let mut data = address_word(Address::repeat_byte(0x03));
    data.extend(abi_words([
        U256::from(7_u64),
        U256::from(8_u64),
        U256::from(9_u64),
    ]));
    rpc_log(
        pool,
        vec![
            v3_mint_topic(),
            topic_address(Address::repeat_byte(0x04)),
            topic_i24(tick_lower),
            topic_i24(tick_upper),
        ],
        data,
        block_number,
        0,
        0,
    )
}

fn v3_burn_log(pool: Address, tick_lower: i32, tick_upper: i32, block_number: u64) -> RpcLog {
    rpc_log(
        pool,
        vec![
            v3_burn_topic(),
            topic_address(Address::repeat_byte(0x04)),
            topic_i24(tick_lower),
            topic_i24(tick_upper),
        ],
        abi_words([U256::from(7_u64), U256::from(8_u64), U256::from(9_u64)]),
        block_number,
        0,
        0,
    )
}

fn v3_registry_no_layout(pool: Address) -> AdapterRegistry {
    let adapter = Arc::new(ConcentratedLiquidityAdapter::default());
    let mut registration = PoolRegistration::new(PoolKey::UniswapV3(pool))
        .with_state_address(pool)
        .with_metadata(ProtocolMetadata::UniswapV3(V3Metadata::default()));
    let sources = adapter.event_sources(&registration);
    registration = registration.with_event_sources(sources);

    let mut registry = AdapterRegistry::new();
    registry.register_adapter(adapter).unwrap();
    registry.register_pool(registration).unwrap();
    registry
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

#[tokio::test]
async fn v3_mint_emits_targeted_tick_resync() -> Result<()> {
    let pool = Address::repeat_byte(0x43);
    let layout = V3StorageLayout::uniswap(60);
    let tick_lower = 60;
    let tick_upper = 15_360;
    let block = 20;

    let mut cache = setup_cache().await?;
    let mut runtime = ReactiveRuntime::<Ethereum>::new(ReactiveConfig::default());
    runtime.register_handler(Arc::new(AmmReactiveHandler::new(v3_registry(pool))))?;

    let report = runtime.ingest_batch(
        &mut cache,
        batch(vec![(
            ReactiveInput::Log(v3_mint_log(pool, tick_lower, tick_upper, block)),
            included_context(block, 0),
        )]),
    )?;

    assert_eq!(report.applied.len(), 1);
    assert_eq!(
        report.applied[0].quality,
        StateEffectQuality::RequiresRepair
    );
    assert!(report.applied[0].state_updates.is_empty());

    assert_eq!(report.applied[0].resyncs.len(), 1);
    let resync = &report.applied[0].resyncs[0];
    assert!(matches!(resync.reason, ResyncReason::HandlerRequested));
    assert!(matches!(
        resync.block,
        ResyncBlock::Hash {
            number,
            hash,
            require_canonical: true,
        } if number == block && hash == block_hash(block)
    ));

    let [ResyncTarget::StorageSlots { address, slots }] = resync.targets.as_slice() else {
        panic!("expected a single storage-slots resync target");
    };
    assert_eq!(*address, pool);
    let mut got = slots.clone();
    got.sort_unstable();
    got.dedup();
    assert_eq!(
        got,
        expected_tick_repair_slots(layout, tick_lower, tick_upper)
    );

    // Observability hook is preserved alongside the executable resync.
    assert!(
        report.applied[0]
            .hook_signals
            .iter()
            .any(|signal| signal.kind.as_ref() == "amm.repair.v3_tick_range")
    );
    Ok(())
}

#[tokio::test]
async fn v3_mint_resync_repairs_tick_and_liquidity_slots() -> Result<()> {
    let pool = Address::repeat_byte(0x44);
    let layout = V3StorageLayout::uniswap(60);
    let tick_lower = 60;
    let tick_upper = 180;
    let block = 21;

    let expected = expected_tick_repair_slots(layout, tick_lower, tick_upper);
    let mut fetched: HashMap<(Address, U256), U256> = HashMap::new();
    for (i, slot) in expected.iter().enumerate() {
        fetched.insert((pool, *slot), U256::from(1_000 + i as u64));
    }

    let mut cache = setup_cache().await?;
    cache.set_storage_batch_fetcher(stub_fetcher(fetched.clone()));

    let mut runtime = ReactiveRuntime::<Ethereum>::new(ReactiveConfig::default());
    runtime.register_handler(Arc::new(AmmReactiveHandler::new(v3_registry(pool))))?;

    let report = runtime.ingest_batch_with_resync(
        &mut cache,
        batch(vec![(
            ReactiveInput::Log(v3_mint_log(pool, tick_lower, tick_upper, block)),
            included_context(block, 0),
        )]),
    )?;

    let resynced = report
        .reports
        .iter()
        .find_map(|report| match report.as_ref() {
            ReactiveReport::Resynced(report) => Some(report),
            _ => None,
        })
        .expect("ingest_batch_with_resync should execute the tick repair");
    assert!(resynced.failed.is_empty());

    for slot in &expected {
        assert_eq!(
            cache.cached_storage_value(pool, *slot),
            Some(fetched[&(pool, *slot)]),
            "slot {slot:?} should hold its authoritatively resynced value"
        );
    }
    Ok(())
}

#[tokio::test]
async fn v3_burn_same_word_dedupes_bitmap_slot() -> Result<()> {
    let pool = Address::repeat_byte(0x45);
    let layout = V3StorageLayout::uniswap(60);
    let tick_lower = 60;
    let tick_upper = 180;
    let block = 22;

    // Precondition: both boundary ticks fall in the same bitmap word.
    assert_eq!(
        word_pos(tick_lower, layout.tick_spacing),
        word_pos(tick_upper, layout.tick_spacing)
    );

    let mut cache = setup_cache().await?;
    let mut runtime = ReactiveRuntime::<Ethereum>::new(ReactiveConfig::default());
    runtime.register_handler(Arc::new(AmmReactiveHandler::new(v3_registry(pool))))?;

    let report = runtime.ingest_batch(
        &mut cache,
        batch(vec![(
            ReactiveInput::Log(v3_burn_log(pool, tick_lower, tick_upper, block)),
            included_context(block, 0),
        )]),
    )?;

    assert_eq!(report.applied.len(), 1);
    assert_eq!(report.applied[0].resyncs.len(), 1);
    let [ResyncTarget::StorageSlots { address, slots }] =
        report.applied[0].resyncs[0].targets.as_slice()
    else {
        panic!("expected a single storage-slots resync target");
    };
    assert_eq!(*address, pool);
    let mut got = slots.clone();
    got.sort_unstable();
    got.dedup();
    // 2 ticks x {slot0, slot3} + 1 shared bitmap word + liquidity = 6 slots.
    assert_eq!(
        got,
        expected_tick_repair_slots(layout, tick_lower, tick_upper)
    );
    assert_eq!(got.len(), 6);
    Ok(())
}

#[tokio::test]
async fn v3_liquidity_event_missing_layout_falls_back_to_invalidation() -> Result<()> {
    let pool = Address::repeat_byte(0x46);
    let block = 23;

    let mut cache = setup_cache().await?;
    let mut runtime = ReactiveRuntime::<Ethereum>::new(ReactiveConfig::default());
    runtime.register_handler(Arc::new(AmmReactiveHandler::new(v3_registry_no_layout(
        pool,
    ))))?;

    let report = runtime.ingest_batch(
        &mut cache,
        batch(vec![(
            ReactiveInput::Log(v3_mint_log(pool, 60, 120, block)),
            included_context(block, 0),
        )]),
    )?;

    assert_eq!(report.applied.len(), 1);
    assert_eq!(
        report.applied[0].quality,
        StateEffectQuality::RequiresRepair
    );
    // No layout -> tick slots cannot be computed, so no targeted resync.
    assert!(report.applied[0].resyncs.is_empty());
    // Falls back to a conservative whole-storage invalidation.
    assert_eq!(report.applied[0].invalidations.len(), 1);
    let invalidation = &report.applied[0].invalidations[0];
    assert_eq!(invalidation.address, pool);
    assert!(matches!(invalidation.scope, PurgeScope::AllStorage));
    Ok(())
}

// --- Phase A3 (slice 1): Uniswap V2 adapter cold_start ---

const V2_TOKEN0_SLOT: U256 = U256::from_limbs([6, 0, 0, 0]);
const V2_TOKEN1_SLOT: U256 = U256::from_limbs([7, 0, 0, 0]);

/// Encode an address as the right-aligned 32-byte storage word a V2 pool stores
/// for token0/token1 (matches `U256::from_be_slice(addr.as_slice())`).
fn token_slot_word(addr: Address) -> U256 {
    U256::from_be_slice(addr.as_slice())
}

#[tokio::test]
async fn v2_cold_start_brings_pool_ready() -> Result<()> {
    let pool = Address::repeat_byte(0x71);
    let token0 = Address::repeat_byte(0xaa);
    let token1 = Address::repeat_byte(0xbb);
    let reserve0 = U256::from(1_000_u64);
    let reserve1 = U256::from(2_000_u64);
    let timestamp = U256::from(0x1234_u64);

    let mut cache = setup_cache().await?;
    cache.set_storage_batch_fetcher(stub_fetcher(HashMap::from([
        ((pool, V2_TOKEN0_SLOT), token_slot_word(token0)),
        ((pool, V2_TOKEN1_SLOT), token_slot_word(token1)),
        (
            (pool, V2_RESERVES_SLOT),
            reserves_slot(reserve0, reserve1, timestamp),
        ),
    ])));
    let expected_hash = uniswap_v2_pair_runtime_code_hash();
    cache.set_account_fields_fetcher(account_fields_fetcher(HashMap::from([(
        pool,
        (U256::ZERO, expected_hash),
    )])));

    let mut registry = AdapterRegistry::new();
    registry
        .register_adapter(Arc::new(UniswapV2Adapter::default()))
        .unwrap();
    let mut registration = PoolRegistration::new(PoolKey::UniswapV2(pool))
        .with_state_address(pool)
        .with_metadata(ProtocolMetadata::UniswapV2(
            UniswapV2Metadata::default().with_fee_bps(30),
        ));

    let outcome = registry.cold_start(&mut registration, &mut cache, ColdStartPolicy::Eager)?;

    assert!(matches!(outcome, ColdStartOutcome::Ready(_)));
    assert_eq!(registration.status, PoolStatus::Ready);
    match &registration.metadata {
        ProtocolMetadata::UniswapV2(metadata) => {
            assert_eq!(metadata.token0, Some(token0));
            assert_eq!(metadata.token1, Some(token1));
            assert_eq!(
                metadata.fee_bps,
                Some(30),
                "config fee_bps must be preserved"
            );
        }
        other => panic!("expected UniswapV2 metadata, got {other:?}"),
    }
    assert_eq!(
        cache.cached_storage_value(pool, V2_TOKEN0_SLOT),
        Some(token_slot_word(token0))
    );
    assert_eq!(
        cache.cached_storage_value(pool, V2_TOKEN1_SLOT),
        Some(token_slot_word(token1))
    );
    assert_eq!(
        cache.cached_storage_value(pool, V2_RESERVES_SLOT),
        Some(reserves_slot(reserve0, reserve1, timestamp))
    );
    Ok(())
}

#[tokio::test]
async fn v2_cold_start_then_sync_applies_exact_no_resync() -> Result<()> {
    let pool = Address::repeat_byte(0x72);
    let token0 = Address::repeat_byte(0xaa);
    let token1 = Address::repeat_byte(0xbb);
    let timestamp = U256::from(0xbeef_u64);

    let mut cache = setup_cache().await?;
    cache.set_storage_batch_fetcher(stub_fetcher(HashMap::from([
        ((pool, V2_TOKEN0_SLOT), token_slot_word(token0)),
        ((pool, V2_TOKEN1_SLOT), token_slot_word(token1)),
        (
            (pool, V2_RESERVES_SLOT),
            reserves_slot(U256::from(10_u64), U256::from(20_u64), timestamp),
        ),
    ])));

    let adapter = UniswapV2Adapter::default();
    let mut registration = PoolRegistration::new(PoolKey::UniswapV2(pool)).with_state_address(pool);
    let sources = adapter.event_sources(&registration);
    registration = registration.with_event_sources(sources);

    let mut cold_registry = AdapterRegistry::new();
    cold_registry
        .register_adapter(Arc::new(UniswapV2Adapter::default()))
        .unwrap();
    cold_registry.cold_start(&mut registration, &mut cache, ColdStartPolicy::Eager)?;

    let mut registry = AdapterRegistry::new();
    registry
        .register_adapter(Arc::new(UniswapV2Adapter::default()))
        .unwrap();
    registry.register_pool(registration).unwrap();

    let mut runtime = ReactiveRuntime::<Ethereum>::new(ReactiveConfig::default());
    runtime.register_handler(Arc::new(AmmReactiveHandler::new(registry)))?;

    let new_reserve0 = U256::from(111_u64);
    let new_reserve1 = U256::from(222_u64);
    let log = rpc_log(
        pool,
        vec![v2_sync_topic()],
        abi_words([new_reserve0, new_reserve1]),
        30,
        0,
        0,
    );
    let report = runtime.ingest_batch(
        &mut cache,
        batch(vec![(ReactiveInput::Log(log), included_context(30, 0))]),
    )?;

    assert_eq!(report.applied.len(), 1);
    // Warmed slot 8 means the masked Sync write lands exactly, with no resync.
    assert_eq!(
        report.applied[0].quality,
        StateEffectQuality::ExactFromInput
    );
    assert!(report.applied[0].resyncs.is_empty());

    let raw = cache.cached_storage_value(pool, V2_RESERVES_SLOT).unwrap();
    assert_eq!(raw & low_mask(112), new_reserve0);
    assert_eq!((raw >> 112) & low_mask(112), new_reserve1);
    assert_eq!(
        raw >> 224,
        timestamp,
        "blockTimestampLast must be preserved"
    );
    Ok(())
}

#[tokio::test]
async fn v2_cold_start_lazy_defers_token_slots() -> Result<()> {
    let pool = Address::repeat_byte(0x73);
    let reserve0 = U256::from(5_u64);
    let reserve1 = U256::from(6_u64);

    let mut cache = setup_cache().await?;
    cache.set_storage_batch_fetcher(stub_fetcher(HashMap::from([
        (
            (pool, V2_TOKEN0_SLOT),
            token_slot_word(Address::repeat_byte(0xaa)),
        ),
        (
            (pool, V2_TOKEN1_SLOT),
            token_slot_word(Address::repeat_byte(0xbb)),
        ),
        (
            (pool, V2_RESERVES_SLOT),
            reserves_slot(reserve0, reserve1, U256::ZERO),
        ),
    ])));

    let mut registry = AdapterRegistry::new();
    registry
        .register_adapter(Arc::new(UniswapV2Adapter::default()))
        .unwrap();
    let mut registration = PoolRegistration::new(PoolKey::UniswapV2(pool)).with_state_address(pool);

    let outcome = registry.cold_start(&mut registration, &mut cache, ColdStartPolicy::Lazy)?;

    match outcome {
        ColdStartOutcome::ReadyWithDeferred(_, deferred) => {
            assert!(
                deferred.iter().any(|work| matches!(
                    work,
                    DeferredWork::VerifySlots(slots)
                        if slots.contains(&(pool, V2_TOKEN0_SLOT))
                            && slots.contains(&(pool, V2_TOKEN1_SLOT))
                )),
                "Lazy cold-start must defer the token slots"
            );
        }
        other => panic!("expected ReadyWithDeferred under Lazy policy, got {other:?}"),
    }

    // Reserves are warmed now; token slots are deferred (not yet fetched).
    assert_eq!(
        cache.cached_storage_value(pool, V2_RESERVES_SLOT),
        Some(reserves_slot(reserve0, reserve1, U256::ZERO))
    );
    assert_eq!(cache.cached_storage_value(pool, V2_TOKEN0_SLOT), None);
    Ok(())
}

#[tokio::test]
async fn v2_cold_start_missing_reserves_needs_repair() -> Result<()> {
    let pool = Address::repeat_byte(0x74);

    // Fetcher serves the token slots but not reserves (returns ZERO -> not
    // injected -> reserves slot stays cold).
    let mut cache = setup_cache().await?;
    cache.set_storage_batch_fetcher(stub_fetcher(HashMap::from([
        (
            (pool, V2_TOKEN0_SLOT),
            token_slot_word(Address::repeat_byte(0xaa)),
        ),
        (
            (pool, V2_TOKEN1_SLOT),
            token_slot_word(Address::repeat_byte(0xbb)),
        ),
    ])));

    let mut registry = AdapterRegistry::new();
    registry
        .register_adapter(Arc::new(UniswapV2Adapter::default()))
        .unwrap();
    let mut registration = PoolRegistration::new(PoolKey::UniswapV2(pool)).with_state_address(pool);

    let outcome = registry.cold_start(&mut registration, &mut cache, ColdStartPolicy::Eager)?;

    assert!(
        matches!(outcome, ColdStartOutcome::NeedsRepair(_, _)),
        "an unfetchable reserves slot must surface as NeedsRepair, not a silent partial"
    );
    assert_eq!(cache.cached_storage_value(pool, V2_RESERVES_SLOT), None);
    Ok(())
}

// --- Phase A3 (slice 2): Uniswap V3 adapter cold_start ---

/// Pack a V3 `slot0` storage word: sqrtPriceX96[0:160] | tick[160:184] (24-bit
/// signed) | observation fields[184:256].
fn v3_slot0_word(sqrt_price: U256, tick: i32, obs_high: U256) -> U256 {
    let tick24 = U256::from((tick as u32) & 0x00FF_FFFF);
    sqrt_price | (tick24 << 160) | (obs_high << 184)
}

#[tokio::test]
async fn v3_cold_start_brings_pool_ready_with_tick_word() -> Result<()> {
    let pool = Address::repeat_byte(0x81);
    let token0 = Address::repeat_byte(0xaa);
    let token1 = Address::repeat_byte(0xbb);
    let layout = V3StorageLayout::uniswap(60);
    let sqrt_price = U256::from(12_345_u64);
    let liquidity = U256::from(67_890_u64);
    let current_tick = 0_i32;
    let obs_high = U256::from(0xabcdef_u64);

    // Bitmap word 0 has ticks 60 (bit 1) and 120 (bit 2) initialized.
    let bitmap_word = (U256::from(1_u64) << 1) | (U256::from(1_u64) << 2);
    let init_ticks = [60_i32, 120_i32];

    let mut seed: HashMap<(Address, U256), U256> = HashMap::from([
        (
            (pool, layout.slot0_slot),
            v3_slot0_word(sqrt_price, current_tick, obs_high),
        ),
        ((pool, layout.liquidity_slot), liquidity),
        (
            (
                pool,
                v3_tick_bitmap_storage_key_with_base(0_i16, layout.tick_bitmap_base_slot),
            ),
            bitmap_word,
        ),
    ]);
    for (i, &tick) in init_ticks.iter().enumerate() {
        let keys = v3_tick_info_storage_keys_with_base(tick, layout.ticks_base_slot);
        seed.insert((pool, keys[0]), U256::from(1_000 + i as u64));
        seed.insert((pool, keys[3]), U256::from(1_u64) << 248);
    }

    let metadata = V3Metadata::default()
        .with_token0(token0)
        .with_token1(token1)
        .with_fee(500)
        .with_tick_spacing(60)
        .with_storage_layout(layout);

    let mut cache = setup_cache().await?;
    cache.set_storage_batch_fetcher(stub_fetcher(seed));

    let mut registry = AdapterRegistry::new();
    registry
        .register_adapter(Arc::new(ConcentratedLiquidityAdapter::default()))
        .unwrap();
    let mut registration = PoolRegistration::new(PoolKey::UniswapV3(pool))
        .with_state_address(pool)
        .with_metadata(ProtocolMetadata::UniswapV3(metadata));

    let outcome = registry.cold_start(&mut registration, &mut cache, ColdStartPolicy::Eager)?;

    assert!(matches!(outcome, ColdStartOutcome::Ready(_)));
    assert_eq!(registration.status, PoolStatus::Ready);
    match &registration.metadata {
        ProtocolMetadata::UniswapV3(metadata) => {
            assert_eq!(metadata.token0, Some(token0));
            assert_eq!(metadata.token1, Some(token1));
            assert_eq!(metadata.fee, Some(500));
            assert_eq!(metadata.tick_spacing, Some(60));
        }
        other => panic!("expected UniswapV3 metadata, got {other:?}"),
    }

    assert!(
        cache
            .cached_storage_value(pool, layout.slot0_slot)
            .is_some()
    );
    assert_eq!(
        cache.cached_storage_value(pool, layout.liquidity_slot),
        Some(liquidity)
    );
    assert!(
        cache
            .cached_storage_value(
                pool,
                v3_tick_bitmap_storage_key_with_base(0_i16, layout.tick_bitmap_base_slot)
            )
            .is_some()
    );
    for &tick in &init_ticks {
        let keys = v3_tick_info_storage_keys_with_base(tick, layout.ticks_base_slot);
        assert!(
            cache.cached_storage_value(pool, keys[0]).is_some(),
            "tick {tick} info slot 0 should be warm"
        );
        assert!(
            cache.cached_storage_value(pool, keys[3]).is_some(),
            "tick {tick} info slot 3 should be warm"
        );
    }
    Ok(())
}

#[tokio::test]
async fn v3_cold_start_then_swap_applies_exact_no_resync() -> Result<()> {
    let pool = Address::repeat_byte(0x82);
    let layout = V3StorageLayout::uniswap(60);
    let obs_high = U256::from(0xfeed_u64);

    let mut cache = setup_cache().await?;
    cache.set_storage_batch_fetcher(stub_fetcher(HashMap::from([
        (
            (pool, layout.slot0_slot),
            v3_slot0_word(U256::from(500_u64), 0, obs_high),
        ),
        ((pool, layout.liquidity_slot), U256::from(600_u64)),
        (
            (
                pool,
                v3_tick_bitmap_storage_key_with_base(0_i16, layout.tick_bitmap_base_slot),
            ),
            U256::ZERO,
        ),
    ])));

    let adapter = ConcentratedLiquidityAdapter::default();
    let mut registration = PoolRegistration::new(PoolKey::UniswapV3(pool))
        .with_state_address(pool)
        .with_metadata(ProtocolMetadata::UniswapV3(
            V3Metadata::default()
                .with_storage_layout(layout)
                .with_tick_spacing(60),
        ));
    let sources = adapter.event_sources(&registration);
    registration = registration.with_event_sources(sources);

    let mut cold_registry = AdapterRegistry::new();
    cold_registry
        .register_adapter(Arc::new(ConcentratedLiquidityAdapter::default()))
        .unwrap();
    cold_registry.cold_start(&mut registration, &mut cache, ColdStartPolicy::Eager)?;

    let mut registry = AdapterRegistry::new();
    registry
        .register_adapter(Arc::new(ConcentratedLiquidityAdapter::default()))
        .unwrap();
    registry.register_pool(registration).unwrap();
    let mut runtime = ReactiveRuntime::<Ethereum>::new(ReactiveConfig::default());
    runtime.register_handler(Arc::new(AmmReactiveHandler::new(registry)))?;

    let new_sqrt = U256::from(777_u64);
    let log = rpc_log(
        pool,
        vec![
            v3_swap_topic(),
            topic_address(Address::repeat_byte(0x01)),
            topic_address(Address::repeat_byte(0x02)),
        ],
        abi_words([
            U256::ZERO,
            U256::ZERO,
            new_sqrt,
            U256::from(888_u64),
            U256::ZERO,
        ]),
        40,
        0,
        0,
    );
    let report = runtime.ingest_batch(
        &mut cache,
        batch(vec![(ReactiveInput::Log(log), included_context(40, 0))]),
    )?;

    assert_eq!(report.applied.len(), 1);
    assert_eq!(
        report.applied[0].quality,
        StateEffectQuality::ExactFromInput
    );
    assert!(report.applied[0].resyncs.is_empty());

    let raw = cache.cached_storage_value(pool, layout.slot0_slot).unwrap();
    assert_eq!(raw & low_mask(160), new_sqrt);
    assert_eq!(
        raw & !low_mask(184),
        obs_high << 184,
        "observation high bits must be preserved"
    );
    Ok(())
}

#[tokio::test]
async fn v3_cold_start_missing_layout_unsupported() -> Result<()> {
    let pool = Address::repeat_byte(0x83);
    let mut cache = setup_cache().await?;

    let mut registry = AdapterRegistry::new();
    registry
        .register_adapter(Arc::new(ConcentratedLiquidityAdapter::default()))
        .unwrap();
    // No resolvable layout: default metadata has neither storage_layout nor
    // tick_spacing.
    let mut registration = PoolRegistration::new(PoolKey::UniswapV3(pool))
        .with_state_address(pool)
        .with_metadata(ProtocolMetadata::UniswapV3(V3Metadata::default()));

    let outcome = registry.cold_start(&mut registration, &mut cache, ColdStartPolicy::Eager)?;
    assert!(matches!(outcome, ColdStartOutcome::Unsupported(_)));
    Ok(())
}

#[tokio::test]
async fn v3_cold_start_missing_slot0_needs_repair() -> Result<()> {
    let pool = Address::repeat_byte(0x84);
    let layout = V3StorageLayout::uniswap(60);

    // Fetcher serves liquidity but not slot0 (returns ZERO -> not injected ->
    // slot0 stays cold).
    let mut cache = setup_cache().await?;
    cache.set_storage_batch_fetcher(stub_fetcher(HashMap::from([(
        (pool, layout.liquidity_slot),
        U256::from(123_u64),
    )])));

    let mut registry = AdapterRegistry::new();
    registry
        .register_adapter(Arc::new(ConcentratedLiquidityAdapter::default()))
        .unwrap();
    let mut registration = PoolRegistration::new(PoolKey::UniswapV3(pool))
        .with_state_address(pool)
        .with_metadata(ProtocolMetadata::UniswapV3(
            V3Metadata::default()
                .with_storage_layout(layout)
                .with_tick_spacing(60),
        ));

    let outcome = registry.cold_start(&mut registration, &mut cache, ColdStartPolicy::Eager)?;
    assert!(matches!(outcome, ColdStartOutcome::NeedsRepair(_, _)));
    assert_eq!(cache.cached_storage_value(pool, layout.slot0_slot), None);
    Ok(())
}

// --- Module-shape hardening: V3-family consolidation + cold-start policies ---

#[tokio::test]
async fn pancake_v3_routes_and_applies_through_family_adapter() -> Result<()> {
    let pool = Address::repeat_byte(0x91);
    let layout = V3StorageLayout::pancake(60);
    let sqrt_price = U256::from(123_u64);
    let liquidity = U256::from(456_u64);

    let adapter = Arc::new(ConcentratedLiquidityAdapter::default());
    let mut registration = PoolRegistration::new(PoolKey::PancakeV3(pool))
        .with_state_address(pool)
        .with_metadata(ProtocolMetadata::PancakeV3(
            V3Metadata::default()
                .with_storage_layout(layout)
                .with_tick_spacing(60),
        ));
    let sources = adapter.event_sources(&registration);
    registration = registration.with_event_sources(sources);

    let mut registry = AdapterRegistry::new();
    registry.register_adapter(adapter).unwrap();
    registry.register_pool(registration).unwrap();

    let mut cache = setup_cache().await?;
    let mut runtime = ReactiveRuntime::<Ethereum>::new(ReactiveConfig::default());
    runtime.register_handler(Arc::new(AmmReactiveHandler::new(registry)))?;

    let log = rpc_log(
        pool,
        vec![
            v3_swap_topic(),
            topic_address(Address::repeat_byte(0x01)),
            topic_address(Address::repeat_byte(0x02)),
        ],
        abi_words([U256::ZERO, U256::ZERO, sqrt_price, liquidity, U256::ZERO]),
        50,
        0,
        0,
    );
    let report = runtime.ingest_batch(
        &mut cache,
        batch(vec![(ReactiveInput::Log(log), included_context(50, 0))]),
    )?;

    assert_eq!(report.applied.len(), 1);
    assert_eq!(
        tag_value(&report.applied[0].tags, "protocol"),
        Some("PancakeV3")
    );
    // The Swap's absolute liquidity write lands at the PANCAKE liquidity slot,
    // proving the family adapter decoded against the Pancake layout.
    assert_eq!(
        cache.cached_storage_value(pool, layout.liquidity_slot),
        Some(liquidity)
    );
    Ok(())
}

#[tokio::test]
async fn v2_cold_start_hot_slots_only_warms_reserves_only() -> Result<()> {
    let pool = Address::repeat_byte(0x92);
    let mut cache = setup_cache().await?;
    cache.set_storage_batch_fetcher(stub_fetcher(HashMap::from([
        (
            (pool, V2_TOKEN0_SLOT),
            token_slot_word(Address::repeat_byte(0xaa)),
        ),
        (
            (pool, V2_TOKEN1_SLOT),
            token_slot_word(Address::repeat_byte(0xbb)),
        ),
        (
            (pool, V2_RESERVES_SLOT),
            reserves_slot(U256::from(1_u64), U256::from(2_u64), U256::ZERO),
        ),
    ])));

    let mut registry = AdapterRegistry::new();
    registry
        .register_adapter(Arc::new(UniswapV2Adapter::default()))
        .unwrap();
    let mut registration = PoolRegistration::new(PoolKey::UniswapV2(pool)).with_state_address(pool);

    let outcome =
        registry.cold_start(&mut registration, &mut cache, ColdStartPolicy::HotSlotsOnly)?;
    assert!(matches!(outcome, ColdStartOutcome::Ready(_)));
    assert!(cache.cached_storage_value(pool, V2_RESERVES_SLOT).is_some());
    assert_eq!(
        cache.cached_storage_value(pool, V2_TOKEN0_SLOT),
        None,
        "HotSlotsOnly must not warm token slots"
    );
    Ok(())
}

#[tokio::test]
async fn v3_cold_start_lazy_defers_tick_word() -> Result<()> {
    let pool = Address::repeat_byte(0x93);
    let layout = V3StorageLayout::uniswap(60);
    let mut cache = setup_cache().await?;
    cache.set_storage_batch_fetcher(stub_fetcher(HashMap::from([
        (
            (pool, layout.slot0_slot),
            v3_slot0_word(U256::from(7_u64), 0, U256::ZERO),
        ),
        ((pool, layout.liquidity_slot), U256::from(8_u64)),
    ])));

    let mut registry = AdapterRegistry::new();
    registry
        .register_adapter(Arc::new(ConcentratedLiquidityAdapter::default()))
        .unwrap();
    let mut registration = PoolRegistration::new(PoolKey::UniswapV3(pool))
        .with_state_address(pool)
        .with_metadata(ProtocolMetadata::UniswapV3(
            V3Metadata::default()
                .with_storage_layout(layout)
                .with_tick_spacing(60),
        ));

    let outcome = registry.cold_start(&mut registration, &mut cache, ColdStartPolicy::Lazy)?;
    let bitmap_key = v3_tick_bitmap_storage_key_with_base(0_i16, layout.tick_bitmap_base_slot);
    match outcome {
        ColdStartOutcome::ReadyWithDeferred(_, deferred) => {
            assert!(
                deferred.iter().any(|work| matches!(
                    work,
                    DeferredWork::VerifySlots(slots) if slots.contains(&(pool, bitmap_key))
                )),
                "Lazy must defer the tick-bitmap word"
            );
        }
        other => panic!("expected ReadyWithDeferred under Lazy, got {other:?}"),
    }
    assert!(
        cache
            .cached_storage_value(pool, layout.slot0_slot)
            .is_some()
    );
    assert_eq!(cache.cached_storage_value(pool, bitmap_key), None);
    Ok(())
}

#[tokio::test]
async fn v3_cold_start_hot_slots_only_skips_tick_word() -> Result<()> {
    let pool = Address::repeat_byte(0x94);
    let layout = V3StorageLayout::uniswap(60);
    let mut cache = setup_cache().await?;
    cache.set_storage_batch_fetcher(stub_fetcher(HashMap::from([
        (
            (pool, layout.slot0_slot),
            v3_slot0_word(U256::from(7_u64), 0, U256::ZERO),
        ),
        ((pool, layout.liquidity_slot), U256::from(8_u64)),
    ])));

    let mut registry = AdapterRegistry::new();
    registry
        .register_adapter(Arc::new(ConcentratedLiquidityAdapter::default()))
        .unwrap();
    let mut registration = PoolRegistration::new(PoolKey::UniswapV3(pool))
        .with_state_address(pool)
        .with_metadata(ProtocolMetadata::UniswapV3(
            V3Metadata::default()
                .with_storage_layout(layout)
                .with_tick_spacing(60),
        ));

    let outcome =
        registry.cold_start(&mut registration, &mut cache, ColdStartPolicy::HotSlotsOnly)?;
    assert!(matches!(outcome, ColdStartOutcome::Ready(_)));
    assert!(
        cache
            .cached_storage_value(pool, layout.slot0_slot)
            .is_some()
    );
    assert!(
        cache
            .cached_storage_value(pool, layout.liquidity_slot)
            .is_some()
    );
    let bitmap_key = v3_tick_bitmap_storage_key_with_base(0_i16, layout.tick_bitmap_base_slot);
    assert_eq!(
        cache.cached_storage_value(pool, bitmap_key),
        None,
        "HotSlotsOnly must not warm the tick word"
    );
    Ok(())
}

// --- Phase A8: V3-family shifted-layout repair + Strict policy coverage ---

/// Build a single-pool registry served by the V3-family adapter for any V3-family
/// key + metadata (so Pancake/Slipstream shifted layouts can be exercised).
fn v3_family_registry(key: PoolKey, metadata: ProtocolMetadata) -> AdapterRegistry {
    let address = key.address().expect("v3-family pools are address-keyed");
    let adapter = Arc::new(ConcentratedLiquidityAdapter::default());
    let mut registration = PoolRegistration::new(key)
        .with_state_address(address)
        .with_metadata(metadata);
    let sources = adapter.event_sources(&registration);
    registration = registration.with_event_sources(sources);

    let mut registry = AdapterRegistry::new();
    registry.register_adapter(adapter).unwrap();
    registry.register_pool(registration).unwrap();
    registry
}

#[tokio::test]
async fn pancake_v3_mint_repair_targets_pancake_layout_slots() -> Result<()> {
    let pool = Address::repeat_byte(0x95);
    let layout = V3StorageLayout::pancake(60);
    let tick_lower = 60;
    let tick_upper = 15_360;
    let block = 30;

    let registry = v3_family_registry(
        PoolKey::PancakeV3(pool),
        ProtocolMetadata::PancakeV3(
            V3Metadata::default()
                .with_storage_layout(layout)
                .with_tick_spacing(60),
        ),
    );

    let mut cache = setup_cache().await?;
    let mut runtime = ReactiveRuntime::<Ethereum>::new(ReactiveConfig::default());
    runtime.register_handler(Arc::new(AmmReactiveHandler::new(registry)))?;

    let report = runtime.ingest_batch(
        &mut cache,
        batch(vec![(
            ReactiveInput::Log(v3_mint_log(pool, tick_lower, tick_upper, block)),
            included_context(block, 0),
        )]),
    )?;

    assert_eq!(report.applied.len(), 1);
    assert_eq!(report.applied[0].resyncs.len(), 1);
    let [ResyncTarget::StorageSlots { address, slots }] =
        report.applied[0].resyncs[0].targets.as_slice()
    else {
        panic!("expected a single storage-slots resync target");
    };
    assert_eq!(*address, pool);
    let mut got = slots.clone();
    got.sort_unstable();
    got.dedup();
    assert_eq!(
        got,
        expected_tick_repair_slots(layout, tick_lower, tick_upper),
        "repair must target the Pancake layout slots"
    );
    // The Pancake slots are genuinely distinct from the Uniswap layout's,
    // proving the family adapter lowered the repair against the Pancake layout.
    assert_ne!(
        got,
        expected_tick_repair_slots(V3StorageLayout::uniswap(60), tick_lower, tick_upper)
    );
    Ok(())
}

#[tokio::test]
async fn slipstream_mint_repair_targets_slipstream_layout_slots() -> Result<()> {
    let pool = Address::repeat_byte(0x96);
    let layout = V3StorageLayout::slipstream(60);
    let tick_lower = 60;
    let tick_upper = 15_360;
    let block = 31;

    let registry = v3_family_registry(
        PoolKey::Slipstream(pool),
        ProtocolMetadata::Slipstream(
            V3Metadata::default()
                .with_storage_layout(layout)
                .with_tick_spacing(60),
        ),
    );

    let mut cache = setup_cache().await?;
    let mut runtime = ReactiveRuntime::<Ethereum>::new(ReactiveConfig::default());
    runtime.register_handler(Arc::new(AmmReactiveHandler::new(registry)))?;

    let report = runtime.ingest_batch(
        &mut cache,
        batch(vec![(
            ReactiveInput::Log(v3_mint_log(pool, tick_lower, tick_upper, block)),
            included_context(block, 0),
        )]),
    )?;

    assert_eq!(report.applied.len(), 1);
    assert_eq!(report.applied[0].resyncs.len(), 1);
    let [ResyncTarget::StorageSlots { address, slots }] =
        report.applied[0].resyncs[0].targets.as_slice()
    else {
        panic!("expected a single storage-slots resync target");
    };
    assert_eq!(*address, pool);
    let mut got = slots.clone();
    got.sort_unstable();
    got.dedup();
    assert_eq!(
        got,
        expected_tick_repair_slots(layout, tick_lower, tick_upper),
        "repair must target the Slipstream layout slots"
    );
    assert_ne!(
        got,
        expected_tick_repair_slots(V3StorageLayout::uniswap(60), tick_lower, tick_upper)
    );
    Ok(())
}

#[tokio::test]
async fn v2_cold_start_strict_warms_all_slots_like_eager() -> Result<()> {
    let pool = Address::repeat_byte(0x97);
    let token0 = Address::repeat_byte(0xa0);
    let token1 = Address::repeat_byte(0xa1);
    let mut cache = setup_cache().await?;
    cache.set_storage_batch_fetcher(stub_fetcher(HashMap::from([
        ((pool, V2_TOKEN0_SLOT), token_slot_word(token0)),
        ((pool, V2_TOKEN1_SLOT), token_slot_word(token1)),
        (
            (pool, V2_RESERVES_SLOT),
            reserves_slot(U256::from(1_u64), U256::from(2_u64), U256::ZERO),
        ),
    ])));

    let mut registry = AdapterRegistry::new();
    registry
        .register_adapter(Arc::new(UniswapV2Adapter::default()))
        .unwrap();
    let mut registration = PoolRegistration::new(PoolKey::UniswapV2(pool)).with_state_address(pool);

    let outcome = registry.cold_start(&mut registration, &mut cache, ColdStartPolicy::Strict)?;
    assert!(matches!(outcome, ColdStartOutcome::Ready(_)));
    // Strict currently warms the same slot set as Eager (this locks that
    // behavior so a future Strict-specific divergence is caught).
    assert!(cache.cached_storage_value(pool, V2_TOKEN0_SLOT).is_some());
    assert!(cache.cached_storage_value(pool, V2_TOKEN1_SLOT).is_some());
    assert!(cache.cached_storage_value(pool, V2_RESERVES_SLOT).is_some());
    assert_eq!(registration.status, PoolStatus::Ready);
    match registration.metadata {
        ProtocolMetadata::UniswapV2(ref metadata) => {
            assert_eq!(metadata.token0, Some(token0));
            assert_eq!(metadata.token1, Some(token1));
        }
        ref other => panic!("expected merged UniswapV2 metadata, got {other:?}"),
    }
    Ok(())
}

#[tokio::test]
async fn v3_cold_start_strict_warms_hot_slots_like_eager() -> Result<()> {
    let pool = Address::repeat_byte(0x98);
    let layout = V3StorageLayout::uniswap(60);
    let bitmap_key = v3_tick_bitmap_storage_key_with_base(0_i16, layout.tick_bitmap_base_slot);
    let mut cache = setup_cache().await?;
    // slot0 carries tick 0 (-> bitmap word 0); the word reads as empty (no
    // initialized ticks), so the tick-info round is a no-op.
    cache.set_storage_batch_fetcher(stub_fetcher(HashMap::from([
        (
            (pool, layout.slot0_slot),
            v3_slot0_word(U256::from(7_u64), 0, U256::ZERO),
        ),
        ((pool, layout.liquidity_slot), U256::from(8_u64)),
        ((pool, bitmap_key), U256::ZERO),
    ])));

    let mut registry = AdapterRegistry::new();
    registry
        .register_adapter(Arc::new(ConcentratedLiquidityAdapter::default()))
        .unwrap();
    let mut registration = PoolRegistration::new(PoolKey::UniswapV3(pool))
        .with_state_address(pool)
        .with_metadata(ProtocolMetadata::UniswapV3(
            V3Metadata::default()
                .with_storage_layout(layout)
                .with_tick_spacing(60),
        ));

    let outcome = registry.cold_start(&mut registration, &mut cache, ColdStartPolicy::Strict)?;
    assert!(matches!(outcome, ColdStartOutcome::Ready(_)));
    assert!(
        cache
            .cached_storage_value(pool, layout.slot0_slot)
            .is_some()
    );
    assert!(
        cache
            .cached_storage_value(pool, layout.liquidity_slot)
            .is_some()
    );
    assert_eq!(registration.status, PoolStatus::Ready);
    Ok(())
}

#[tokio::test]
async fn v3_cold_start_missing_liquidity_is_still_ready() -> Result<()> {
    let pool = Address::repeat_byte(0x99);
    let layout = V3StorageLayout::uniswap(60);
    let bitmap_key = v3_tick_bitmap_storage_key_with_base(0_i16, layout.tick_bitmap_base_slot);
    let mut cache = setup_cache().await?;
    // slot0 is fetchable; the liquidity slot is intentionally cold (reads as
    // zero). slot0 is the only mandatory slot — liquidity is an absolute write
    // the next reactive Swap always reapplies — so cold-start still reaches Ready.
    cache.set_storage_batch_fetcher(stub_fetcher(HashMap::from([
        (
            (pool, layout.slot0_slot),
            v3_slot0_word(U256::from(7_u64), 0, U256::ZERO),
        ),
        ((pool, bitmap_key), U256::ZERO),
    ])));

    let mut registry = AdapterRegistry::new();
    registry
        .register_adapter(Arc::new(ConcentratedLiquidityAdapter::default()))
        .unwrap();
    let mut registration = PoolRegistration::new(PoolKey::UniswapV3(pool))
        .with_state_address(pool)
        .with_metadata(ProtocolMetadata::UniswapV3(
            V3Metadata::default()
                .with_storage_layout(layout)
                .with_tick_spacing(60),
        ));

    let outcome = registry.cold_start(&mut registration, &mut cache, ColdStartPolicy::Eager)?;
    assert!(matches!(outcome, ColdStartOutcome::Ready(_)));
    assert!(
        cache
            .cached_storage_value(pool, layout.slot0_slot)
            .is_some()
    );
    assert_eq!(
        cache.cached_storage_value(pool, layout.liquidity_slot),
        None,
        "a cold liquidity slot is best-effort, not mandatory"
    );
    Ok(())
}

// --- Solidly V2 reactive ---

fn solidly_sync_topic() -> B256 {
    keccak256("Sync(uint256,uint256)")
}

fn solidly_registry(pool: Address, layout: SolidlyStorageLayout) -> AdapterRegistry {
    let adapter = Arc::new(SolidlyV2Adapter::default());
    let mut registration = PoolRegistration::new(PoolKey::SolidlyV2(pool))
        .with_state_address(pool)
        .with_metadata(ProtocolMetadata::SolidlyV2(
            SolidlyV2Metadata::default()
                .with_stable(false)
                .with_storage_layout(layout),
        ));
    let sources = adapter.event_sources(&registration);
    registration = registration.with_event_sources(sources);

    let mut registry = AdapterRegistry::new();
    registry.register_adapter(adapter).unwrap();
    registry.register_pool(registration).unwrap();
    registry
}

// A Solidly `Sync(uint256,uint256)` writes the two separate reserve slots exactly
// from the event payload (no fetch) — the V2-style exact event-sourcing, adapted
// to Solidly's unpacked reserve layout.
#[tokio::test]
async fn solidly_sync_writes_both_reserve_slots_through_runtime() -> Result<()> {
    let pool = Address::repeat_byte(0x51);
    let (r0_slot, r1_slot) = (U256::from(10_u64), U256::from(11_u64));
    let layout =
        SolidlyStorageLayout::new(r0_slot, r1_slot, U256::from(12_u64), U256::from(13_u64));
    let reserve0 = U256::from(123_u64);
    let reserve1 = U256::from(456_u64);
    let log = rpc_log(
        pool,
        vec![solidly_sync_topic()],
        abi_words([reserve0, reserve1]),
        11,
        0,
        0,
    );

    let mut cache = setup_cache().await?;
    let mut runtime = ReactiveRuntime::<Ethereum>::new(ReactiveConfig::default());
    runtime.register_handler(Arc::new(AmmReactiveHandler::new(solidly_registry(
        pool, layout,
    ))))?;

    let report = runtime.ingest_batch(
        &mut cache,
        batch(vec![(ReactiveInput::Log(log), included_context(11, 0))]),
    )?;
    assert_eq!(report.applied.len(), 1, "the Sync must apply exactly once");

    assert_eq!(
        cache.cached_storage_value(pool, r0_slot),
        Some(reserve0),
        "reserve0 written exactly from the Sync payload"
    );
    assert_eq!(
        cache.cached_storage_value(pool, r1_slot),
        Some(reserve1),
        "reserve1 written exactly from the Sync payload"
    );
    Ok(())
}

// A Solidly pool registered WITHOUT a storage layout (cold-start would be
// MissingMetadata) must not have a Sync silently mutate the cache — decode_event
// errors on the missing layout, so nothing is written.
#[tokio::test]
async fn solidly_sync_without_layout_does_not_mutate_cache() -> Result<()> {
    let pool = Address::repeat_byte(0x52);
    let adapter = Arc::new(SolidlyV2Adapter::default());
    let mut registration = PoolRegistration::new(PoolKey::SolidlyV2(pool))
        .with_state_address(pool)
        .with_metadata(ProtocolMetadata::SolidlyV2(
            SolidlyV2Metadata::default().with_stable(false),
        ));
    let sources = adapter.event_sources(&registration);
    registration = registration.with_event_sources(sources);
    let mut registry = AdapterRegistry::new();
    registry.register_adapter(adapter).unwrap();
    registry.register_pool(registration).unwrap();

    let log = rpc_log(
        pool,
        vec![solidly_sync_topic()],
        abi_words([U256::from(123_u64), U256::from(456_u64)]),
        11,
        0,
        0,
    );
    let mut cache = setup_cache().await?;
    let mut runtime = ReactiveRuntime::<Ethereum>::new(ReactiveConfig::default());
    runtime.register_handler(Arc::new(AmmReactiveHandler::new(registry)))?;

    // ingest must not panic, and a layout-less decode must write nothing.
    let _ = runtime.ingest_batch(
        &mut cache,
        batch(vec![(ReactiveInput::Log(log), included_context(11, 0))]),
    )?;
    assert_eq!(
        cache.cached_storage_value(pool, U256::from(10_u64)),
        None,
        "a layout-less Solidly Sync must not write any reserve slot"
    );
    Ok(())
}

// --- Curve StableSwap reactive (resync on TokenExchange) ---
//
// A Curve `TokenExchange` carries deltas, not absolute balances, and the
// `get_dy` read-set (balances + A + fee) lives behind a non-predictable Vyper
// layout, so the reactive path re-verifies (resyncs) exactly the cold-start
// `discovered_slots` rather than applying the event payload. These tests cover:
// (1) a cold-started pool emits a `VerifySlots` resync over its discovered slot,
// and (2) the batch-robustness guard: an empty-`discovered_slots` pool must not
// error or mutate (a decode error would fail the whole `ingest_batch`).

fn curve_token_exchange_topic() -> B256 {
    keccak256("TokenExchange(address,int128,uint256,int128,uint256)")
}

/// Install `runtime_hex` (a hand-assembled stub) as the deployed bytecode at
/// `address`, mirroring the cold-start harness so a `get_dy` discover call can
/// run offline against the reactive cache.
fn install_runtime(cache: &mut EvmCache, address: Address, runtime_hex: &str) {
    let code = Bytecode::new_raw(Bytes::from(
        hex::decode(runtime_hex.trim()).expect("valid mock-pool runtime hex"),
    ));
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
}

// Cold-start a Curve pool (discover -> verify), re-seed the discovered slot
// stale, then a `TokenExchange` must emit a hash-pinned resync over exactly the
// discovered slot (the post-swap re-read), refreshing it to the fresh value.
#[tokio::test(flavor = "multi_thread")]
async fn curve_token_exchange_resyncs_discovered_slot() -> Result<()> {
    let pool = Address::repeat_byte(0xc1);
    let dai = Address::repeat_byte(0x01);
    let usdc = Address::repeat_byte(0x02);
    let usdt = Address::repeat_byte(0x03);
    let fresh = U256::from(999_900_u64);

    let mut cache = setup_cache().await?;
    // The block beneficiary (Address::ZERO) is credited gas during the discover
    // call's transact; install it so the offline run does not fetch it.
    cache
        .db_mut()
        .insert_account_info(Address::ZERO, AccountInfo::default());
    install_runtime(
        &mut cache,
        pool,
        include_str!("fixtures/mock_curve_pool_runtime.hex"),
    );
    // Seed slot 0 so the discover `get_dy` SLOADs it; the fetcher serves the
    // fresh post-swap value the verify round and the reactive resync re-read.
    cache
        .db_mut()
        .insert_account_storage(pool, U256::ZERO, U256::from(1_u64))?;
    cache.set_storage_batch_fetcher(stub_fetcher(HashMap::from([((pool, U256::ZERO), fresh)])));

    // Cold-start to populate `discovered_slots`.
    let mut cold_registry = AdapterRegistry::new();
    cold_registry
        .register_adapter(Arc::new(CurveAdapter::default()))
        .unwrap();
    let mut registration = PoolRegistration::new(PoolKey::Curve(pool))
        .with_state_address(pool)
        .with_metadata(ProtocolMetadata::Curve(
            CurveMetadata::default()
                .with_coins(vec![dai, usdc, usdt])
                .with_discovered_slots(Vec::new())
                .with_variant(CurveVariant::StableSwap),
        ));
    let outcome =
        cold_registry.cold_start(&mut registration, &mut cache, ColdStartPolicy::Eager)?;
    assert!(matches!(outcome, ColdStartOutcome::Ready(_)));
    match &registration.metadata {
        ProtocolMetadata::Curve(m) => {
            assert!(
                m.discovered_slots.contains(&U256::ZERO),
                "cold-start must have discovered slot 0, got {:?}",
                m.discovered_slots
            );
        }
        other => panic!("expected Curve metadata, got {other:?}"),
    }

    let adapter = CurveAdapter::default();
    let sources = adapter.event_sources(&registration);
    registration = registration.with_event_sources(sources);

    let mut registry = AdapterRegistry::new();
    registry
        .register_adapter(Arc::new(CurveAdapter::default()))
        .unwrap();
    registry.register_pool(registration).unwrap();
    let mut runtime = ReactiveRuntime::<Ethereum>::new(ReactiveConfig::default());
    runtime.register_handler(Arc::new(AmmReactiveHandler::new(registry)))?;

    let log = rpc_log(
        pool,
        vec![curve_token_exchange_topic(), topic_address(dai)],
        abi_words([
            U256::ZERO,        // sold_id
            U256::from(1_u64), // tokens_sold
            U256::from(1_u64), // bought_id
            U256::from(1_u64), // tokens_bought
        ]),
        60,
        0,
        0,
    );
    let report = runtime.ingest_batch_with_resync(
        &mut cache,
        batch(vec![(ReactiveInput::Log(log), included_context(60, 0))]),
    )?;

    assert_eq!(report.applied.len(), 1);
    assert_eq!(
        report.applied[0].resyncs.len(),
        1,
        "a TokenExchange must resync"
    );
    let [ResyncTarget::StorageSlots { address, slots }] =
        report.applied[0].resyncs[0].targets.as_slice()
    else {
        panic!("expected a single storage-slots resync target");
    };
    assert_eq!(*address, pool);
    assert_eq!(
        slots,
        &vec![U256::ZERO],
        "resync must target the discovered slot"
    );
    Ok(())
}

// Batch-robustness guard: a Curve pool with EMPTY `discovered_slots` (cold-start
// has not run) must route a `TokenExchange` without erroring and without
// mutating the cache. An error here would fail the whole `ingest_batch` (the
// Solidly batch-robustness lesson).
#[tokio::test]
async fn curve_token_exchange_empty_slots_no_error_no_mutation() -> Result<()> {
    let pool = Address::repeat_byte(0xc2);
    let dai = Address::repeat_byte(0x01);
    let usdc = Address::repeat_byte(0x02);

    let mut cache = setup_cache().await?;

    let adapter = CurveAdapter::default();
    let mut registration = PoolRegistration::new(PoolKey::Curve(pool))
        .with_state_address(pool)
        .with_metadata(ProtocolMetadata::Curve(
            CurveMetadata::default()
                .with_coins(vec![dai, usdc])
                .with_variant(CurveVariant::StableSwap),
        ));
    let sources = adapter.event_sources(&registration);
    registration = registration.with_event_sources(sources);

    let mut registry = AdapterRegistry::new();
    registry
        .register_adapter(Arc::new(CurveAdapter::default()))
        .unwrap();
    registry.register_pool(registration).unwrap();
    let mut runtime = ReactiveRuntime::<Ethereum>::new(ReactiveConfig::default());
    runtime.register_handler(Arc::new(AmmReactiveHandler::new(registry)))?;

    let log = rpc_log(
        pool,
        vec![curve_token_exchange_topic(), topic_address(dai)],
        abi_words([
            U256::ZERO,
            U256::from(1_u64),
            U256::from(1_u64),
            U256::from(1_u64),
        ]),
        61,
        0,
        0,
    );
    // Must not panic / error, and must not mutate the cold cache.
    let report = runtime.ingest_batch(
        &mut cache,
        batch(vec![(ReactiveInput::Log(log), included_context(61, 0))]),
    )?;
    assert_eq!(report.applied.len(), 1, "the event still routes");
    assert!(
        report.applied[0].resyncs.is_empty(),
        "empty discovered_slots must produce no resync"
    );
    assert_eq!(
        cache.cached_storage_value(pool, U256::ZERO),
        None,
        "no discovered slots -> no mutation"
    );
    Ok(())
}

/// Cold-start a Curve pool of `variant` (generic mock; discovers slot 0) and
/// return a reactive runtime wired so a resync re-reads slot 0 as `fresh`. Shared
/// by the liquidity-event reactive tests below.
async fn curve_reactive_runtime(
    pool: Address,
    coins: Vec<Address>,
    variant: CurveVariant,
    fresh: U256,
) -> Result<(EvmCache, ReactiveRuntime<Ethereum>)> {
    let mut cache = setup_cache().await?;
    cache
        .db_mut()
        .insert_account_info(Address::ZERO, AccountInfo::default());
    install_runtime(
        &mut cache,
        pool,
        include_str!("fixtures/mock_curve_pool_runtime.hex"),
    );
    cache
        .db_mut()
        .insert_account_storage(pool, U256::ZERO, U256::from(1_u64))?;
    cache.set_storage_batch_fetcher(stub_fetcher(HashMap::from([((pool, U256::ZERO), fresh)])));

    let mut cold = AdapterRegistry::new();
    cold.register_adapter(Arc::new(CurveAdapter::default()))
        .unwrap();
    let mut reg = PoolRegistration::new(PoolKey::Curve(pool))
        .with_state_address(pool)
        .with_metadata(ProtocolMetadata::Curve(
            CurveMetadata::default()
                .with_coins(coins)
                .with_variant(variant),
        ));
    let outcome = cold.cold_start(&mut reg, &mut cache, ColdStartPolicy::Eager)?;
    assert!(
        matches!(outcome, ColdStartOutcome::Ready(_)),
        "cold-start Ready"
    );

    let sources = CurveAdapter::default().event_sources(&reg);
    reg = reg.with_event_sources(sources);
    let mut registry = AdapterRegistry::new();
    registry
        .register_adapter(Arc::new(CurveAdapter::default()))
        .unwrap();
    registry.register_pool(reg).unwrap();
    let mut runtime = ReactiveRuntime::<Ethereum>::new(ReactiveConfig::default());
    runtime.register_handler(Arc::new(AmmReactiveHandler::new(registry)))?;
    Ok((cache, runtime))
}

// CryptoSwap liquidity events (single-fee AddLiquidity, fees-array-less
// RemoveLiquidity, and the 3-arg RemoveLiquidityOne — signatures verified on-chain
// against tricrypto2) must each route -> resync the discovered slot, keeping
// cached state fresh after liquidity changes (not just swaps).
#[tokio::test(flavor = "multi_thread")]
async fn curve_cryptoswap_liquidity_events_resync() -> Result<()> {
    let pool = Address::repeat_byte(0xc3);
    let (usdt, wbtc, weth) = (
        Address::repeat_byte(0x01),
        Address::repeat_byte(0x02),
        Address::repeat_byte(0x03),
    );
    let (mut cache, mut runtime) = curve_reactive_runtime(
        pool,
        vec![usdt, wbtc, weth],
        CurveVariant::CryptoSwap,
        U256::from(147_348_u64),
    )
    .await?;

    let topics = [
        keccak256("AddLiquidity(address,uint256[3],uint256,uint256)"),
        keccak256("RemoveLiquidity(address,uint256[3],uint256)"),
        keccak256("RemoveLiquidityOne(address,uint256,uint256,uint256)"),
    ];
    let logs: Vec<_> = topics
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let b = 62 + i as u64;
            (
                ReactiveInput::Log(rpc_log(
                    pool,
                    vec![*t, topic_address(usdt)],
                    abi_words([U256::ZERO]),
                    b,
                    0,
                    0,
                )),
                included_context(b, 0),
            )
        })
        .collect();
    let report = runtime.ingest_batch_with_resync(&mut cache, batch(logs))?;

    assert_eq!(
        report.applied.len(),
        3,
        "all 3 CryptoSwap liquidity events must route"
    );
    for applied in &report.applied {
        assert_eq!(applied.resyncs.len(), 1, "each liquidity event must resync");
        let [ResyncTarget::StorageSlots { address, slots }] = applied.resyncs[0].targets.as_slice()
        else {
            panic!("expected a single storage-slots resync target");
        };
        assert_eq!(*address, pool);
        assert_eq!(
            slots,
            &vec![U256::ZERO],
            "resync must target the discovered slot"
        );
    }
    Ok(())
}

// StableSwap-NG emits the 3-arg RemoveLiquidityOne (classic StableSwap uses the
// 2-arg form); routing it via the StableSwap variant closes the NG liquidity gap.
#[tokio::test(flavor = "multi_thread")]
async fn curve_ng_remove_liquidity_one_3arg_resyncs() -> Result<()> {
    let pool = Address::repeat_byte(0xc4);
    let (a, b) = (Address::repeat_byte(0x01), Address::repeat_byte(0x02));
    let (mut cache, mut runtime) = curve_reactive_runtime(
        pool,
        vec![a, b],
        CurveVariant::StableSwap,
        U256::from(42_u64),
    )
    .await?;

    let log = rpc_log(
        pool,
        vec![
            keccak256("RemoveLiquidityOne(address,uint256,uint256,uint256)"),
            topic_address(a),
        ],
        abi_words([U256::ZERO]),
        70,
        0,
        0,
    );
    let report = runtime.ingest_batch_with_resync(
        &mut cache,
        batch(vec![(ReactiveInput::Log(log), included_context(70, 0))]),
    )?;
    assert_eq!(
        report.applied.len(),
        1,
        "the NG 3-arg RemoveLiquidityOne must route"
    );
    assert_eq!(report.applied[0].resyncs.len(), 1, "it must resync");
    let [ResyncTarget::StorageSlots { address, slots }] =
        report.applied[0].resyncs[0].targets.as_slice()
    else {
        panic!("expected a single storage-slots resync target");
    };
    assert_eq!(*address, pool);
    assert_eq!(
        slots,
        &vec![U256::ZERO],
        "resync must target the discovered slot"
    );
    Ok(())
}

// Tricrypto-NG extended events — 7-arg TokenExchange, 5-arg AddLiquidity, 6-arg
// RemoveLiquidityOne, and ClaimAdminFee (signatures verified on-chain against
// tricryptoUSDC/USDT) — each route -> resync the discovered slot for a
// CryptoSwapNG pool.
#[tokio::test(flavor = "multi_thread")]
async fn curve_tricrypto_ng_events_resync() -> Result<()> {
    let pool = Address::repeat_byte(0xc5);
    let (usdc, wbtc, weth) = (
        Address::repeat_byte(0x01),
        Address::repeat_byte(0x02),
        Address::repeat_byte(0x03),
    );
    let (mut cache, mut runtime) = curve_reactive_runtime(
        pool,
        vec![usdc, wbtc, weth],
        CurveVariant::CryptoSwapNG,
        U256::from(1_476_u64),
    )
    .await?;

    // The 7-arg NG TokenExchange IS decode-validated (6 non-indexed uint256
    // words); the others route on topic only. Six zero words satisfy all.
    let topics = [
        keccak256("TokenExchange(address,uint256,uint256,uint256,uint256,uint256,uint256)"),
        keccak256("AddLiquidity(address,uint256[3],uint256,uint256,uint256)"),
        keccak256("RemoveLiquidityOne(address,uint256,uint256,uint256,uint256,uint256)"),
        keccak256("ClaimAdminFee(address,uint256)"),
    ];
    let logs: Vec<_> = topics
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let b = 80 + i as u64;
            (
                ReactiveInput::Log(rpc_log(
                    pool,
                    vec![*t, topic_address(usdc)],
                    abi_words([U256::ZERO; 6]),
                    b,
                    0,
                    0,
                )),
                included_context(b, 0),
            )
        })
        .collect();
    let report = runtime.ingest_batch_with_resync(&mut cache, batch(logs))?;

    assert_eq!(
        report.applied.len(),
        4,
        "all 4 Tricrypto-NG events must route"
    );
    for applied in &report.applied {
        assert_eq!(applied.resyncs.len(), 1, "each NG event must resync");
        let [ResyncTarget::StorageSlots { address, slots }] = applied.resyncs[0].targets.as_slice()
        else {
            panic!("expected a single storage-slots resync target");
        };
        assert_eq!(*address, pool);
        assert_eq!(
            slots,
            &vec![U256::ZERO],
            "resync must target the discovered slot"
        );
    }
    Ok(())
}
