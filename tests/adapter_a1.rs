use std::collections::HashMap;
use std::sync::Arc;

use alloy_primitives::{Address, B256, Bytes, Log, U256, keccak256};
use evm_amm_state::adapters::storage::{
    V2_RESERVES_SLOT, V3_LIQUIDITY_SLOT, V3_SLOT0_SLOT, V3StorageLayout,
    v3_tick_bitmap_storage_key_with_base, v3_tick_info_storage_keys_with_base,
};
use evm_amm_state::adapters::{
    AdapterCache, AdapterDriver, AdapterEvent, AdapterEventContext, AdapterEventError,
    AdapterEventKind, AdapterEventResult, AdapterRegistry, AmmAdapter, BalancerV2Adapter,
    BalancerV2Metadata, CacheError, CallOutcome, ColdStartPolicy, ConcentratedLiquidityAdapter,
    CustomPoolKey, DriverError, EventSource, PoolKey, PoolRegistration, ProtocolId,
    ProtocolMetadata, RegistryError, RepairAction, SkippedDelta, SkippedMask, SlotChange,
    SlotDelta, StateDiff, StateUpdate, StateView, UniswapV2Adapter, UnsupportedReason,
    UpdateQuality, V3Metadata, V3SwapTransitionCapability, V3TransitionError,
};

const CUSTOM_PROTOCOL: &str = "custom-adapter-defined";

#[derive(Default)]
struct MockCache {
    storage: HashMap<(Address, U256), U256>,
    batches: Vec<Vec<StateUpdate>>,
}

impl MockCache {
    fn seed(&mut self, address: Address, slot: U256, value: U256) {
        self.storage.insert((address, slot), value);
    }

    fn value(&self, address: Address, slot: U256) -> Option<U256> {
        self.storage.get(&(address, slot)).copied()
    }
}

impl StateView for MockCache {
    fn storage(&self, address: Address, slot: U256) -> Option<U256> {
        self.value(address, slot)
    }
}

impl AdapterCache for MockCache {
    fn cached_storage(&self, address: Address, slot: U256) -> Option<U256> {
        self.value(address, slot)
    }

    fn apply_updates(&mut self, updates: &[StateUpdate]) -> StateDiff {
        self.batches.push(updates.to_vec());

        let mut diff = StateDiff::default();
        for update in updates {
            match update {
                StateUpdate::Slot {
                    address,
                    slot,
                    value,
                } => {
                    let old = self
                        .storage
                        .insert((*address, *slot), *value)
                        .unwrap_or_default();
                    if old != *value {
                        diff.slots
                            .push(SlotChange::new(*address, *slot, old, *value));
                    }
                }
                StateUpdate::SlotMasked {
                    address,
                    slot,
                    mask,
                    value,
                } => {
                    if let Some(old) = self.value(*address, *slot) {
                        let new = (old & !*mask) | (*value & *mask);
                        self.storage.insert((*address, *slot), new);
                        if old != new {
                            diff.slots.push(SlotChange::new(*address, *slot, old, new));
                        }
                    } else {
                        diff.skipped_masks
                            .push(SkippedMask::new(*address, *slot, *mask, *value));
                    }
                }
                StateUpdate::SlotDelta {
                    address,
                    slot,
                    delta,
                } => {
                    if let Some(old) = self.value(*address, *slot) {
                        let new = delta.apply(old);
                        self.storage.insert((*address, *slot), new);
                        if old != new {
                            diff.slots.push(SlotChange::new(*address, *slot, old, new));
                        }
                    } else {
                        diff.skipped
                            .push(SkippedDelta::new(*address, *slot, *delta));
                    }
                }
                StateUpdate::Purge { address, .. } => {
                    self.storage.retain(|(stored, _), _| stored != address);
                }
                _ => panic!("unexpected StateUpdate variant in adapter A1 mock cache"),
            }
        }

        diff
    }

    fn verify_slots(&mut self, _slots: &[(Address, U256)]) -> Result<Vec<SlotChange>, CacheError> {
        Ok(Vec::new())
    }

    fn purge_storage(&mut self, address: Address) -> StateDiff {
        self.storage.retain(|(stored, _), _| *stored != address);
        StateDiff::default()
    }

    fn purge_slots(&mut self, address: Address, slots: &[U256]) -> StateDiff {
        for slot in slots {
            self.storage.remove(&(address, *slot));
        }
        StateDiff::default()
    }

    fn read_storage_slot(&mut self, address: Address, slot: U256) -> Result<U256, CacheError> {
        self.value(address, slot)
            .ok_or_else(|| CacheError::Backend("slot is cold".into()))
    }

    fn call_raw(
        &mut self,
        _from: Address,
        _to: Address,
        _calldata: Bytes,
        _commit: bool,
    ) -> Result<CallOutcome, CacheError> {
        Err(CacheError::Backend(
            "mock cache does not execute calls".into(),
        ))
    }
}

struct AdapterDefinedRouter {
    key: PoolKey,
    emitter: Address,
    topic: B256,
}

impl AmmAdapter for AdapterDefinedRouter {
    fn protocol(&self) -> ProtocolId {
        ProtocolId::Custom(CUSTOM_PROTOCOL)
    }

    fn route_log(&self, log: &Log, _registry: &AdapterRegistry) -> Option<PoolKey> {
        (log.address == self.emitter && log.topics().first() == Some(&self.topic))
            .then(|| self.key.clone())
    }
}

struct SequencingAdapter {
    topic: B256,
    slot: U256,
    cold_slot: U256,
}

struct ContextOnlyAdapter {
    topic: B256,
    slot: U256,
}

impl AmmAdapter for ContextOnlyAdapter {
    fn protocol(&self) -> ProtocolId {
        ProtocolId::UniswapV2
    }

    fn event_sources(&self, pool: &PoolRegistration) -> Vec<EventSource> {
        pool.key
            .address()
            .map(|address| EventSource::direct(address, vec![self.topic]))
            .into_iter()
            .collect()
    }

    fn decode_event_with_context(
        &self,
        pool: &PoolRegistration,
        log: &Log,
        _view: &dyn StateView,
        context: &AdapterEventContext,
    ) -> AdapterEventResult {
        let Some(timestamp) = context.block_timestamp else {
            return AdapterEventResult::error(AdapterEventError::V3Transition(
                evm_amm_state::adapters::V3TransitionError::MissingContext("block_timestamp"),
            ));
        };
        let address = pool.key.address().expect("address pool");
        AdapterEventResult::event(
            AdapterEvent::new(
                pool.key.clone(),
                log.address,
                self.topic,
                AdapterEventKind::Swap,
                UpdateQuality::Exact,
            )
            .with_updates(vec![StateUpdate::slot(
                address,
                self.slot,
                U256::from(timestamp),
            )]),
        )
    }
}

#[test]
fn driver_routes_context_aware_event_application() {
    let pool = Address::repeat_byte(0x19);
    let topic = B256::repeat_byte(0x29);
    let slot = U256::from(99);
    let adapter = Arc::new(ContextOnlyAdapter { topic, slot });
    let mut registration = PoolRegistration::new(PoolKey::UniswapV2(pool));
    let sources = adapter.event_sources(&registration);
    registration = registration.with_event_sources(sources);
    let mut registry = AdapterRegistry::new();
    registry.register_adapter(adapter).unwrap();
    registry.register_pool(registration).unwrap();
    let driver = AdapterDriver::new(registry);
    let mut cache = MockCache::default();
    let event_context = AdapterEventContext::for_block(7, B256::repeat_byte(7), 1_700_000_007)
        .with_chain_id(1)
        .with_parent_hash(B256::repeat_byte(6))
        .with_transaction_hash(B256::repeat_byte(5))
        .with_event_order(1, 2);

    driver
        .apply_log_with_context(
            &mut cache,
            &log(pool, vec![topic], Vec::new()),
            &event_context,
        )
        .unwrap()
        .expect("context-aware event");
    assert_eq!(cache.value(pool, slot), Some(U256::from(1_700_000_007_u64)));
}

impl AmmAdapter for SequencingAdapter {
    fn protocol(&self) -> ProtocolId {
        ProtocolId::UniswapV2
    }

    fn decode_event(
        &self,
        pool: &PoolRegistration,
        log: &Log,
        view: &dyn StateView,
    ) -> AdapterEventResult {
        if log.topics().first() != Some(&self.topic) {
            return AdapterEventResult::ignored();
        }

        let address = pool.key.address().expect("test pool is address-keyed");
        let current = view.storage(address, self.slot).unwrap_or_default();
        let next = current + U256::from(1);
        AdapterEventResult::event(
            AdapterEvent::new(
                pool.key.clone(),
                log.address,
                self.topic,
                AdapterEventKind::Swap,
                UpdateQuality::ExactIfApplied,
            )
            .with_updates(vec![
                StateUpdate::slot(address, self.slot, next),
                StateUpdate::slot_masked(address, self.cold_slot, U256::MAX, U256::from(9)),
            ]),
        )
    }

    fn after_apply(
        &self,
        pool: &PoolRegistration,
        _event: &AdapterEvent,
        diff: &StateDiff,
    ) -> RepairAction {
        if diff.has_skipped() {
            let address = pool.key.address().expect("test pool is address-keyed");
            RepairAction::VerifySlots(vec![(address, self.cold_slot)])
        } else {
            RepairAction::None
        }
    }
}

fn log(address: Address, topics: Vec<B256>, data: Vec<u8>) -> Log {
    Log::new(address, topics, Bytes::from(data)).expect("valid test log")
}

fn topic_address(address: Address) -> B256 {
    let mut bytes = [0u8; 32];
    bytes[12..].copy_from_slice(address.as_slice());
    B256::from(bytes)
}

fn word(value: U256) -> Vec<u8> {
    value.to_be_bytes::<32>().to_vec()
}

fn abi_words(values: impl IntoIterator<Item = U256>) -> Vec<u8> {
    values.into_iter().flat_map(word).collect()
}

fn low_mask(bits: usize) -> U256 {
    (U256::from(1) << bits) - U256::from(1)
}

fn v2_sync_topic() -> B256 {
    keccak256("Sync(uint112,uint112)")
}

fn v3_swap_topic() -> B256 {
    keccak256("Swap(address,address,int256,int256,uint160,uint128,int24)")
}

#[test]
fn registry_registers_protocol_adapters_and_rejects_duplicates() {
    assert_eq!(
        SlotDelta::Add(U256::from(1)).apply(U256::from(2)),
        U256::from(3)
    );

    let key = PoolKey::Custom(evm_amm_state::adapters::CustomPoolKey::Address {
        protocol: CUSTOM_PROTOCOL,
        address: Address::repeat_byte(0x11),
    });
    let emitter = Address::repeat_byte(0x22);
    let topic = B256::repeat_byte(0x33);

    let mut registry = AdapterRegistry::new();
    registry
        .register_pool(
            PoolRegistration::new(key.clone())
                .with_state_address(key.address().unwrap())
                .with_event_source(EventSource::adapter_defined(emitter, vec![topic])),
        )
        .unwrap();

    registry
        .register_adapter(Arc::new(AdapterDefinedRouter {
            key: key.clone(),
            emitter,
            topic,
        }))
        .unwrap();

    assert!(
        registry
            .adapter(ProtocolId::Custom(CUSTOM_PROTOCOL))
            .is_some()
    );
    assert!(matches!(
        registry.register_adapter(Arc::new(AdapterDefinedRouter {
            key: key.clone(),
            emitter,
            topic,
        })),
        Err(RegistryError::DuplicateAdapter(ProtocolId::Custom(
            CUSTOM_PROTOCOL
        )))
    ));

    let routed = registry
        .route_log(&log(emitter, vec![topic], Vec::new()))
        .expect("adapter-defined route should resolve");
    assert_eq!(routed.key, key);
}

#[test]
fn subscription_spec_preserves_emitters_topics_and_routes() {
    let direct_pool = Address::repeat_byte(0x44);
    let vault = Address::repeat_byte(0x55);
    let direct_topic = B256::repeat_byte(0x66);
    let vault_topic = B256::repeat_byte(0x77);
    let pool_id = B256::repeat_byte(0x88);

    let mut registry = AdapterRegistry::new();
    registry
        .register_pool(
            PoolRegistration::new(PoolKey::UniswapV2(direct_pool))
                .with_state_address(direct_pool)
                .with_event_source(EventSource::direct(direct_pool, vec![direct_topic])),
        )
        .unwrap();
    registry
        .register_pool(
            PoolRegistration::new(PoolKey::BalancerV2(pool_id))
                .with_state_address(vault)
                .with_event_source(EventSource::indexed_bytes32(vault, vec![vault_topic], 1)),
        )
        .unwrap();

    let sources = registry.subscription_spec().sources;
    assert_eq!(sources.len(), 2);
    assert!(sources.contains(&EventSource::direct(direct_pool, vec![direct_topic])));
    assert!(sources.contains(&EventSource::indexed_bytes32(vault, vec![vault_topic], 1)));
    assert_eq!(
        registry.subscription_topics(),
        vec![direct_topic, vault_topic]
    );
}

#[test]
fn driver_processes_logs_in_order_and_reports_post_apply_repairs() {
    let pool = Address::repeat_byte(0x99);
    let topic = B256::repeat_byte(0xaa);
    let slot = U256::from(3);
    let cold_slot = U256::from(4);

    let mut registry = AdapterRegistry::new();
    registry
        .register_adapter(Arc::new(SequencingAdapter {
            topic,
            slot,
            cold_slot,
        }))
        .unwrap();
    registry
        .register_pool(
            PoolRegistration::new(PoolKey::UniswapV2(pool))
                .with_state_address(pool)
                .with_event_source(EventSource::direct(pool, vec![topic])),
        )
        .unwrap();

    let mut cache = MockCache::default();
    cache.seed(pool, slot, U256::ZERO);

    let driver = AdapterDriver::new(registry);
    let reports = driver
        .apply_logs(
            &mut cache,
            &[
                log(pool, vec![topic], Vec::new()),
                log(pool, vec![topic], Vec::new()),
            ],
        )
        .unwrap();

    assert_eq!(reports.len(), 2);
    assert_eq!(cache.value(pool, slot), Some(U256::from(2)));
    assert!(reports.iter().all(|report| report.applied.has_skipped()));
    assert!(matches!(
        reports[0].repair,
        RepairAction::VerifySlots(ref slots) if slots == &vec![(pool, cold_slot)]
    ));
}

/// `apply_logs` is batch-robust: a single malformed log (a `DriverError::Decode`)
/// is skipped so the rest of the batch still applies — the same contract the
/// reactive runtime path follows. A malformed Sync ahead of a valid one must not
/// drop the valid event or abort the batch.
#[test]
fn apply_logs_isolates_a_malformed_log_from_the_batch() {
    let pool = Address::repeat_byte(0x9a);
    let mask112 = (U256::from(1) << 112) - U256::from(1);

    let mut registry = AdapterRegistry::new();
    registry
        .register_adapter(Arc::new(UniswapV2Adapter::default()))
        .unwrap();
    registry
        .register_pool(
            PoolRegistration::new(PoolKey::UniswapV2(pool))
                .with_state_address(pool)
                .with_event_source(EventSource::direct(pool, vec![v2_sync_topic()])),
        )
        .unwrap();

    let mut cache = MockCache::default();
    // Warm the reserves slot so the valid masked Sync write lands exactly.
    cache.seed(pool, V2_RESERVES_SLOT, U256::ZERO);

    let driver = AdapterDriver::new(registry);
    let reports = driver
        .apply_logs(
            &mut cache,
            &[
                // Truncated one-word Sync body → DriverError::Decode, isolated.
                log(pool, vec![v2_sync_topic()], word(U256::from(1_u64))),
                // Well-formed Sync → applied.
                log(
                    pool,
                    vec![v2_sync_topic()],
                    abi_words([U256::from(111_u64), U256::from(222_u64)]),
                ),
            ],
        )
        .expect("a malformed log must not abort the batch");

    assert_eq!(reports.len(), 1, "only the valid Sync yields a report");
    let raw = cache.value(pool, V2_RESERVES_SLOT).unwrap();
    assert_eq!(raw & mask112, U256::from(111_u64));
    assert_eq!((raw >> 112) & mask112, U256::from(222_u64));
}

#[test]
fn uniswap_v2_sync_updates_reserves_without_clobbering_timestamp() {
    let pool = Address::repeat_byte(0xbb);
    let reserve0 = U256::from(123_u64);
    let reserve1 = U256::from(456_u64);
    let timestamp = U256::from(0x1234_u64);
    let initial_slot = timestamp << 224;

    let adapter = Arc::new(UniswapV2Adapter::default());
    let mut registration = PoolRegistration::new(PoolKey::UniswapV2(pool)).with_state_address(pool);
    let sources = adapter.event_sources(&registration);
    registration = registration.with_event_sources(sources);

    let mut registry = AdapterRegistry::new();
    registry.register_adapter(adapter).unwrap();
    registry.register_pool(registration).unwrap();

    let mut cache = MockCache::default();
    cache.seed(pool, V2_RESERVES_SLOT, initial_slot);

    let driver = AdapterDriver::new(registry);
    let report = driver
        .apply_log(
            &mut cache,
            &log(pool, vec![v2_sync_topic()], abi_words([reserve0, reserve1])),
        )
        .unwrap()
        .expect("sync should decode");

    let raw = cache.value(pool, V2_RESERVES_SLOT).unwrap();
    assert_eq!(raw & low_mask(112), reserve0);
    assert_eq!((raw >> 112) & low_mask(112), reserve1);
    assert_eq!(raw >> 224, timestamp);
    assert_eq!(report.event.kind, AdapterEventKind::Sync);
    assert_eq!(report.event.quality, UpdateQuality::ExactIfApplied);
    assert_eq!(report.repair, RepairAction::None);
}

#[test]
fn contextless_uniswap_v3_swap_fails_closed_and_requests_repair() {
    let pool = Address::repeat_byte(0xcc);
    let sender = Address::repeat_byte(0x01);
    let recipient = Address::repeat_byte(0x02);
    let sqrt_price = U256::from(12_345_u64);
    let liquidity = U256::from(67_890_u64);
    let tick = U256::from(42_u64);
    let preserved_high_bits = U256::from(0xabcdef_u64) << 184;

    let adapter = Arc::new(ConcentratedLiquidityAdapter::default());
    let mut registration = PoolRegistration::new(PoolKey::UniswapV3(pool))
        .with_state_address(pool)
        .with_metadata(ProtocolMetadata::UniswapV3(
            V3Metadata::default()
                .with_fee(3_000)
                .with_tick_spacing(60)
                .with_storage_layout(V3StorageLayout::uniswap(60)),
        ));
    let sources = adapter.event_sources(&registration);
    registration = registration.with_event_sources(sources);

    let mut registry = AdapterRegistry::new();
    registry.register_adapter(adapter).unwrap();
    registry.register_pool(registration).unwrap();

    let mut cache = MockCache::default();
    cache.seed(pool, V3_SLOT0_SLOT, preserved_high_bits);

    let driver = AdapterDriver::new(registry);
    let error = driver
        .apply_log(
            &mut cache,
            &log(
                pool,
                vec![
                    v3_swap_topic(),
                    topic_address(sender),
                    topic_address(recipient),
                ],
                abi_words([U256::ZERO, U256::ZERO, sqrt_price, liquidity, tick]),
            ),
        )
        .expect_err("contextless exact swap must surface its typed failure");

    assert_eq!(cache.value(pool, V3_SLOT0_SLOT), None);
    assert_eq!(cache.value(pool, V3_LIQUIDITY_SLOT), None);
    assert!(matches!(
        error,
        evm_amm_state::adapters::DriverError::Decode {
            error: AdapterEventError::V3Transition(V3TransitionError::MissingContext(_)),
            ..
        }
    ));
}

#[test]
fn exact_v3_driver_failure_purges_stale_parent_before_returning_typed_error() {
    let pool = Address::repeat_byte(0xcd);
    let adapter = Arc::new(ConcentratedLiquidityAdapter::default());
    let mut registration = PoolRegistration::new(PoolKey::UniswapV3(pool))
        .with_state_address(pool)
        .with_metadata(ProtocolMetadata::UniswapV3(
            V3Metadata::default()
                .with_fee(3_000)
                .with_tick_spacing(60)
                .with_storage_layout(V3StorageLayout::uniswap(60)),
        ));
    let sources = adapter.event_sources(&registration);
    registration = registration.with_event_sources(sources);

    let mut registry = AdapterRegistry::new();
    registry.register_adapter(adapter).unwrap();
    registry.register_pool(registration).unwrap();

    let stale_slot = U256::from(99);
    let mut cache = MockCache::default();
    cache.seed(pool, stale_slot, U256::from(123));
    let context = AdapterEventContext::for_block(7, B256::repeat_byte(7), 1_700_000_007)
        .with_chain_id(1)
        .with_parent_hash(B256::repeat_byte(6))
        .with_transaction_hash(B256::repeat_byte(5))
        .with_event_order(1, 2);
    let result = AdapterDriver::new(registry).apply_log_with_context(
        &mut cache,
        &log(
            pool,
            vec![
                v3_swap_topic(),
                topic_address(Address::repeat_byte(0x01)),
                topic_address(Address::repeat_byte(0x02)),
            ],
            abi_words([
                U256::from(1),
                U256::MAX,
                U256::from(12_345),
                U256::from(67_890),
                U256::from(42),
            ]),
        ),
        &context,
    );

    assert!(matches!(
        result,
        Err(DriverError::Decode {
            protocol: ProtocolId::UniswapV3,
            error: AdapterEventError::MissingState { .. },
        })
    ));
    assert_eq!(
        cache.value(pool, stale_slot),
        None,
        "exact-transition failure must invalidate every stale pool storage word"
    );
}

#[test]
fn balancer_v2_adapter_routes_vault_swap_by_pool_id() {
    let vault = Address::repeat_byte(0xdd);
    let pool_id = B256::repeat_byte(0xee);

    let adapter = Arc::new(BalancerV2Adapter::default());
    let mut registration = PoolRegistration::new(PoolKey::BalancerV2(pool_id))
        .with_state_address(vault)
        .with_metadata(ProtocolMetadata::BalancerV2(
            BalancerV2Metadata::default().with_vault(vault),
        ));
    let sources = adapter.event_sources(&registration);
    registration = registration.with_event_sources(sources);
    let swap_topic = registration.event_sources[0].topics[0];

    let mut registry = AdapterRegistry::new();
    registry.register_adapter(adapter).unwrap();
    registry.register_pool(registration).unwrap();

    let routed = registry
        .route_log(&log(vault, vec![swap_topic, pool_id], Vec::new()))
        .expect("vault swap should route by pool id");
    assert_eq!(routed.key, PoolKey::BalancerV2(pool_id));
}

#[test]
fn v3_family_adapter_claims_pancake_and_slipstream() {
    // The V3 adapter is registered once but must serve the whole V3 storage-layout
    // family (Uniswap V3, Pancake V3, Slipstream) so those pools can route to it.
    let mut registry = AdapterRegistry::new();
    registry
        .register_adapter(Arc::new(ConcentratedLiquidityAdapter::default()))
        .unwrap();

    assert!(registry.adapter(ProtocolId::UniswapV3).is_some());
    assert!(
        registry.adapter(ProtocolId::PancakeV3).is_some(),
        "V3-family adapter must claim PancakeV3"
    );
    assert!(
        registry.adapter(ProtocolId::Slipstream).is_some(),
        "V3-family adapter must claim Slipstream"
    );
}

#[test]
fn v3_static_exact_swap_capability_is_canonical_uniswap_only_and_fee_bounded() {
    let adapter = ConcentratedLiquidityAdapter::default();
    let canonical = PoolRegistration::new(PoolKey::UniswapV3(Address::repeat_byte(0x31)))
        .with_metadata(ProtocolMetadata::UniswapV3(
            V3Metadata::default()
                .with_fee(3_000)
                .with_storage_layout(V3StorageLayout::uniswap(60)),
        ));
    let pancake = PoolRegistration::new(PoolKey::PancakeV3(Address::repeat_byte(0x32)))
        .with_metadata(ProtocolMetadata::PancakeV3(
            V3Metadata::default()
                .with_fee(2_500)
                .with_storage_layout(V3StorageLayout::pancake(50)),
        ));
    let invalid_fee = PoolRegistration::new(PoolKey::UniswapV3(Address::repeat_byte(0x34)))
        .with_metadata(ProtocolMetadata::UniswapV3(
            V3Metadata::default()
                .with_fee(1_000_000)
                .with_storage_layout(V3StorageLayout::uniswap(60)),
        ));
    let slipstream = PoolRegistration::new(PoolKey::Slipstream(Address::repeat_byte(0x33)))
        .with_metadata(ProtocolMetadata::Slipstream(
            V3Metadata::default()
                .with_fee(10_000)
                // Deployed mooBIFI pools use slot0=6, liquidity=16,
                // ticks=17 and bitmap=18. Layout similarity is deliberately
                // insufficient to claim semantic parity.
                .with_storage_layout(V3StorageLayout::new(
                    U256::from(6),
                    U256::from(16),
                    U256::from(17),
                    U256::from(18),
                    200,
                )),
        ));

    assert_eq!(
        ConcentratedLiquidityAdapter::swap_transition_capability(&canonical),
        V3SwapTransitionCapability::Exact
    );
    assert_eq!(
        ConcentratedLiquidityAdapter::swap_transition_capability(&pancake),
        V3SwapTransitionCapability::Unsupported
    );
    assert_eq!(
        ConcentratedLiquidityAdapter::swap_transition_capability(&invalid_fee),
        V3SwapTransitionCapability::Unsupported
    );
    assert_eq!(
        ConcentratedLiquidityAdapter::swap_transition_capability(&slipstream),
        V3SwapTransitionCapability::Unsupported
    );

    let context = AdapterEventContext::for_block(7, B256::repeat_byte(7), 1_700_000_007)
        .with_chain_id(8453)
        .with_parent_hash(B256::repeat_byte(6))
        .with_transaction_hash(B256::repeat_byte(5))
        .with_event_order(1, 2);
    let decoded = adapter.decode_event_with_context(
        &slipstream,
        &log(
            Address::repeat_byte(0x33),
            vec![
                v3_swap_topic(),
                topic_address(Address::repeat_byte(0x01)),
                topic_address(Address::repeat_byte(0x02)),
            ],
            abi_words([
                U256::from(1),
                U256::MAX,
                U256::from(12_345),
                U256::from(67_890),
                U256::from(42),
            ]),
        ),
        &MockCache::default(),
        &context,
    );
    assert!(matches!(
        decoded.error,
        Some(AdapterEventError::Unsupported(
            evm_amm_state::adapters::UnsupportedReason::Protocol(ProtocolId::Slipstream)
        ))
    ));
    let event = decoded.event.expect("unsupported family remains routed");
    assert_eq!(event.quality, UpdateQuality::RequiresRepair);
    assert!(matches!(
        event.updates.as_slice(),
        [StateUpdate::Purge { address, .. }] if *address == Address::repeat_byte(0x33)
    ));
    assert_eq!(
        event.repair,
        RepairAction::PurgeStorage(Address::repeat_byte(0x33))
    );
}

#[test]
fn canonical_v3_subscribes_every_standard_pool_mutation() {
    let pool = Address::repeat_byte(0x35);
    let adapter = ConcentratedLiquidityAdapter::default();
    let sources = adapter.event_sources(&v3_pool_registration(pool));
    assert_eq!(sources.len(), 1);
    let topics = &sources[0].topics;
    let expected = [
        v3_swap_topic(),
        v3_initialize_topic(),
        v3_mint_topic(),
        v3_collect_topic(),
        v3_burn_topic(),
        v3_flash_topic(),
        v3_increase_observation_cardinality_next_topic(),
        v3_set_fee_protocol_topic(),
        v3_collect_protocol_topic(),
    ];
    assert_eq!(topics.len(), expected.len());
    for topic in expected {
        assert!(
            topics.contains(&topic),
            "missing canonical V3 topic {topic}"
        );
    }
}

#[test]
fn mint_new_tick_below_current_purges_before_a_crossing_swap() {
    let pool = Address::repeat_byte(0x36);
    let layout = V3StorageLayout::uniswap(60);
    let adapter = Arc::new(ConcentratedLiquidityAdapter::default());
    let mut registration = v3_pool_registration(pool);
    let event_sources = adapter.event_sources(&registration);
    registration = registration.with_event_sources(event_sources);
    let mut registry = AdapterRegistry::new();
    registry.register_adapter(adapter).unwrap();
    registry.register_pool(registration).unwrap();
    let driver = AdapterDriver::new(registry);

    let mut cache = MockCache::default();
    cache.seed(pool, layout.slot0_slot, U256::from(1));
    cache.seed(pool, layout.liquidity_slot, U256::from(1_000));
    for tick in [-60, 120] {
        for slot in v3_tick_info_storage_keys_with_base(tick, layout.ticks_base_slot) {
            cache.seed(pool, slot, U256::ZERO);
        }
        cache.seed(
            pool,
            v3_tick_bitmap_storage_key_with_base(0, layout.tick_bitmap_base_slot),
            U256::ZERO,
        );
    }

    let mint = log(
        pool,
        vec![
            v3_mint_topic(),
            topic_address(Address::repeat_byte(0x37)),
            topic_i24(-60),
            topic_i24(120),
        ],
        abi_words([U256::from(1), U256::from(7), U256::from(2), U256::from(3)]),
    );
    assert!(matches!(
        driver.apply_log(&mut cache, &mint),
        Err(DriverError::Decode {
            error: AdapterEventError::Unsupported(UnsupportedReason::Custom(_)),
            ..
        })
    ));
    assert_eq!(cache.value(pool, layout.slot0_slot), None);
    assert_eq!(cache.value(pool, layout.liquidity_slot), None);

    let context = AdapterEventContext::for_block(8, B256::repeat_byte(8), 1_700_000_008)
        .with_chain_id(1)
        .with_parent_hash(B256::repeat_byte(7))
        .with_transaction_hash(B256::repeat_byte(6))
        .with_event_order(1, 2);
    let crossing_swap = log(
        pool,
        vec![
            v3_swap_topic(),
            topic_address(Address::repeat_byte(0x01)),
            topic_address(Address::repeat_byte(0x02)),
        ],
        abi_words([
            U256::from(1),
            U256::MAX,
            U256::from(2),
            U256::from(900),
            U256::MAX,
        ]),
    );
    let crossing_result = driver.apply_log_with_context(&mut cache, &crossing_swap, &context);
    assert!(
        matches!(
            crossing_result,
            Err(DriverError::Decode {
                error: AdapterEventError::MissingState { .. },
                ..
            })
        ),
        "purged parent must reject the crossing swap: {crossing_result:?}"
    );
    assert_eq!(cache.value(pool, layout.slot0_slot), None);
}

#[test]
fn burn_clear_and_admin_mutations_never_leave_a_swap_parent_quoteable() {
    let pool = Address::repeat_byte(0x38);
    let layout = V3StorageLayout::uniswap(60);
    let adapter = Arc::new(ConcentratedLiquidityAdapter::default());
    let mut registration = v3_pool_registration(pool);
    let event_sources = adapter.event_sources(&registration);
    registration = registration.with_event_sources(event_sources);
    let mut registry = AdapterRegistry::new();
    registry.register_adapter(adapter).unwrap();
    registry.register_pool(registration).unwrap();
    let driver = AdapterDriver::new(registry);
    let mut cache = MockCache::default();

    let burn = log(
        pool,
        vec![
            v3_burn_topic(),
            topic_address(Address::repeat_byte(0x39)),
            topic_i24(-60),
            topic_i24(120),
        ],
        abi_words([U256::from(7), U256::from(2), U256::from(3)]),
    );
    cache.seed(pool, layout.slot0_slot, U256::from(1));
    cache.seed(pool, layout.liquidity_slot, U256::from(7));
    let lower_word = U256::from(7) | (U256::from(7) << 128);
    cache.seed(
        pool,
        v3_tick_info_storage_keys_with_base(-60, layout.ticks_base_slot)[0],
        lower_word,
    );
    assert!(matches!(
        driver.apply_log(&mut cache, &burn),
        Err(DriverError::Decode {
            error: AdapterEventError::Unsupported(UnsupportedReason::Custom(_)),
            ..
        })
    ));
    assert_eq!(cache.value(pool, layout.slot0_slot), None);

    let context = AdapterEventContext::for_block(9, B256::repeat_byte(9), 1_700_000_009)
        .with_chain_id(1)
        .with_parent_hash(B256::repeat_byte(8))
        .with_transaction_hash(B256::repeat_byte(7))
        .with_event_order(1, 1);
    let post_burn_swap = log(
        pool,
        vec![
            v3_swap_topic(),
            topic_address(Address::repeat_byte(0x01)),
            topic_address(Address::repeat_byte(0x02)),
        ],
        abi_words([
            U256::from(1),
            U256::MAX,
            U256::from(2),
            U256::from(3),
            U256::ZERO,
        ]),
    );
    assert!(matches!(
        driver.apply_log_with_context(&mut cache, &post_burn_swap, &context),
        Err(DriverError::Decode {
            error: AdapterEventError::MissingState { .. },
            ..
        })
    ));

    for topic in [
        v3_initialize_topic(),
        v3_collect_topic(),
        v3_flash_topic(),
        v3_increase_observation_cardinality_next_topic(),
        v3_set_fee_protocol_topic(),
        v3_collect_protocol_topic(),
    ] {
        cache.seed(pool, layout.slot0_slot, U256::from(9));
        cache.seed(pool, layout.liquidity_slot, U256::from(10));
        assert!(matches!(
            driver.apply_log(&mut cache, &log(pool, vec![topic], Vec::new())),
            Err(DriverError::Decode {
                error: AdapterEventError::Unsupported(UnsupportedReason::Custom(_)),
                ..
            })
        ));
        assert_eq!(cache.value(pool, layout.slot0_slot), None);
        assert_eq!(cache.value(pool, layout.liquidity_slot), None);
    }
}

// --- Phase A8: negative / malformed event-decode coverage ---
//
// The adapters carry many MalformedLog / Unsupported / ignored branches in
// `decode_event` that had zero test coverage. These call `decode_event`
// directly so the exact `AdapterEventResult` is asserted. A `MockCache` stands
// in for the `&dyn StateView` (no fetch is performed during decode).

fn v3_mint_topic() -> B256 {
    keccak256("Mint(address,address,int24,int24,uint128,uint256,uint256)")
}

fn v3_burn_topic() -> B256 {
    keccak256("Burn(address,int24,int24,uint128,uint256,uint256)")
}

fn v3_initialize_topic() -> B256 {
    keccak256("Initialize(uint160,int24)")
}

fn v3_collect_topic() -> B256 {
    keccak256("Collect(address,address,int24,int24,uint128,uint128)")
}

fn v3_flash_topic() -> B256 {
    keccak256("Flash(address,address,uint256,uint256,uint256,uint256)")
}

fn v3_increase_observation_cardinality_next_topic() -> B256 {
    keccak256("IncreaseObservationCardinalityNext(uint16,uint16)")
}

fn v3_set_fee_protocol_topic() -> B256 {
    keccak256("SetFeeProtocol(uint8,uint8,uint8,uint8)")
}

fn v3_collect_protocol_topic() -> B256 {
    keccak256("CollectProtocol(address,address,uint128,uint128)")
}

fn balancer_swap_topic() -> B256 {
    keccak256("Swap(bytes32,address,address,uint256,uint256)")
}

fn topic_i24(value: i32) -> B256 {
    let mut bytes = if value < 0 { [0xff; 32] } else { [0u8; 32] };
    let raw = value.to_be_bytes();
    bytes[29..32].copy_from_slice(&raw[1..4]);
    B256::from(bytes)
}

/// A V3 registration with a resolvable Uniswap layout, so decode reaches the
/// branches after the layout guard.
fn v3_pool_registration(pool: Address) -> PoolRegistration {
    PoolRegistration::new(PoolKey::UniswapV3(pool))
        .with_state_address(pool)
        .with_metadata(ProtocolMetadata::UniswapV3(
            V3Metadata::default()
                .with_fee(3_000)
                .with_tick_spacing(60)
                .with_storage_layout(V3StorageLayout::uniswap(60)),
        ))
}

/// A non-address-keyed pool key, used to exercise the "pool key is not
/// address-keyed" guards of adapters that require an address.
fn custom_bytes32_key() -> PoolKey {
    PoolKey::Custom(CustomPoolKey::Bytes32 {
        protocol: CUSTOM_PROTOCOL,
        id: B256::repeat_byte(0x5a),
    })
}

#[test]
fn v2_sync_wrong_topic_is_ignored() {
    let pool = Address::repeat_byte(0x21);
    let adapter = UniswapV2Adapter::default();
    let registration = PoolRegistration::new(PoolKey::UniswapV2(pool)).with_state_address(pool);
    let view = MockCache::default();

    let result = adapter.decode_event(
        &registration,
        &log(
            pool,
            vec![B256::repeat_byte(0xee)],
            abi_words([U256::from(1_u64), U256::from(2_u64)]),
        ),
        &view,
    );
    assert_eq!(result, AdapterEventResult::ignored());
}

#[test]
fn v2_sync_malformed_data_is_rejected() {
    let pool = Address::repeat_byte(0x22);
    let adapter = UniswapV2Adapter::default();
    let registration = PoolRegistration::new(PoolKey::UniswapV2(pool)).with_state_address(pool);
    let view = MockCache::default();

    // Sync carries two uint112 words (64 bytes of data); 32 bytes is truncated.
    let result = adapter.decode_event(
        &registration,
        &log(pool, vec![v2_sync_topic()], word(U256::from(1_u64))),
        &view,
    );
    assert_eq!(
        result.error,
        Some(AdapterEventError::MalformedLog(
            "malformed Uniswap V2 Sync log"
        ))
    );
    assert!(result.event.is_none());
}

#[test]
fn v2_sync_non_address_keyed_pool_is_rejected() {
    let adapter = UniswapV2Adapter::default();
    let registration = PoolRegistration::new(custom_bytes32_key());
    let view = MockCache::default();

    let result = adapter.decode_event(
        &registration,
        &log(
            Address::repeat_byte(0x23),
            vec![v2_sync_topic()],
            abi_words([U256::from(1_u64), U256::from(2_u64)]),
        ),
        &view,
    );
    assert_eq!(
        result.error,
        Some(AdapterEventError::MalformedLog(
            "Uniswap V2 pool key is not address-keyed"
        ))
    );
}

#[test]
fn v3_swap_malformed_data_is_rejected() {
    let pool = Address::repeat_byte(0x24);
    let adapter = ConcentratedLiquidityAdapter::default();
    let registration = v3_pool_registration(pool);
    let view = MockCache::default();

    // Swap carries five non-indexed words (160 bytes); 96 bytes is truncated.
    let result = adapter.decode_event(
        &registration,
        &log(
            pool,
            vec![
                v3_swap_topic(),
                topic_address(Address::repeat_byte(0x01)),
                topic_address(Address::repeat_byte(0x02)),
            ],
            abi_words([U256::ZERO, U256::ZERO, U256::from(1_u64)]),
        ),
        &view,
    );
    assert_eq!(
        result.error,
        Some(AdapterEventError::MalformedLog("malformed V3 Swap log"))
    );
}

#[test]
fn v3_swap_missing_layout_is_unsupported() {
    let pool = Address::repeat_byte(0x25);
    let adapter = ConcentratedLiquidityAdapter::default();
    // No storage_layout and no tick_spacing -> `layout_for` cannot resolve.
    let registration = PoolRegistration::new(PoolKey::UniswapV3(pool))
        .with_state_address(pool)
        .with_metadata(ProtocolMetadata::UniswapV3(V3Metadata::default()));
    let view = MockCache::default();

    let result = adapter.decode_event(
        &registration,
        &log(
            pool,
            vec![
                v3_swap_topic(),
                topic_address(Address::repeat_byte(0x01)),
                topic_address(Address::repeat_byte(0x02)),
            ],
            abi_words([
                U256::ZERO,
                U256::ZERO,
                U256::from(1_u64),
                U256::from(2_u64),
                U256::from(3_u64),
            ]),
        ),
        &view,
    );
    assert_eq!(
        result.error,
        Some(AdapterEventError::Unsupported(
            UnsupportedReason::MissingMetadata("V3 storage layout")
        ))
    );
}

#[test]
fn v3_swap_non_address_keyed_pool_is_rejected() {
    let adapter = ConcentratedLiquidityAdapter::default();
    let registration =
        PoolRegistration::new(custom_bytes32_key()).with_metadata(ProtocolMetadata::UniswapV3(
            V3Metadata::default().with_storage_layout(V3StorageLayout::uniswap(60)),
        ));
    let view = MockCache::default();

    let result = adapter.decode_event(
        &registration,
        &log(
            Address::repeat_byte(0x26),
            vec![
                v3_swap_topic(),
                topic_address(Address::repeat_byte(0x01)),
                topic_address(Address::repeat_byte(0x02)),
            ],
            abi_words([
                U256::ZERO,
                U256::ZERO,
                U256::from(1_u64),
                U256::from(2_u64),
                U256::from(3_u64),
            ]),
        ),
        &view,
    );
    assert_eq!(
        result.error,
        Some(AdapterEventError::MalformedLog(
            "V3 pool key is not address-keyed"
        ))
    );
}

#[test]
fn v3_mint_malformed_data_is_rejected() {
    let pool = Address::repeat_byte(0x27);
    let adapter = ConcentratedLiquidityAdapter::default();
    let registration = v3_pool_registration(pool);
    let view = MockCache::default();

    // Mint carries four non-indexed words (128 bytes); 64 bytes is truncated.
    let result = adapter.decode_event(
        &registration,
        &log(
            pool,
            vec![
                v3_mint_topic(),
                topic_address(Address::repeat_byte(0x04)),
                topic_i24(60),
                topic_i24(120),
            ],
            abi_words([U256::from(1_u64), U256::from(2_u64)]),
        ),
        &view,
    );
    assert_eq!(
        result.error,
        Some(AdapterEventError::MalformedLog(
            "malformed V3 liquidity log"
        ))
    );
}

#[test]
fn v3_burn_malformed_data_is_rejected() {
    let pool = Address::repeat_byte(0x28);
    let adapter = ConcentratedLiquidityAdapter::default();
    let registration = v3_pool_registration(pool);
    let view = MockCache::default();

    let result = adapter.decode_event(
        &registration,
        &log(
            pool,
            vec![
                v3_burn_topic(),
                topic_address(Address::repeat_byte(0x04)),
                topic_i24(60),
                topic_i24(120),
            ],
            abi_words([U256::from(1_u64)]),
        ),
        &view,
    );
    assert_eq!(
        result.error,
        Some(AdapterEventError::MalformedLog(
            "malformed V3 liquidity log"
        ))
    );
}

#[test]
fn v3_mint_missing_tick_topics_is_rejected() {
    let pool = Address::repeat_byte(0x29);
    let adapter = ConcentratedLiquidityAdapter::default();
    let registration = v3_pool_registration(pool);
    let view = MockCache::default();

    // tickLower/tickUpper indexed topics are absent. Decoding the indexed
    // params fails, so the log is rejected as malformed (the explicit per-topic
    // guards are defensive-in-depth behind this validation).
    let result = adapter.decode_event(
        &registration,
        &log(
            pool,
            vec![v3_mint_topic(), topic_address(Address::repeat_byte(0x04))],
            abi_words([
                U256::ZERO,
                U256::from(7_u64),
                U256::from(8_u64),
                U256::from(9_u64),
            ]),
        ),
        &view,
    );
    assert_eq!(
        result.error,
        Some(AdapterEventError::MalformedLog(
            "malformed V3 liquidity log"
        ))
    );
}

#[test]
fn balancer_swap_wrong_topic_is_ignored() {
    let vault = Address::repeat_byte(0x2a);
    let adapter = BalancerV2Adapter::default();
    let registration = PoolRegistration::new(PoolKey::BalancerV2(B256::repeat_byte(0x2b)))
        .with_state_address(vault);
    let view = MockCache::default();

    let result = adapter.decode_event(
        &registration,
        &log(
            vault,
            vec![B256::repeat_byte(0xee), B256::repeat_byte(0x2b)],
            Vec::new(),
        ),
        &view,
    );
    assert_eq!(result, AdapterEventResult::ignored());
}

#[test]
fn balancer_swap_malformed_data_is_rejected() {
    let vault = Address::repeat_byte(0x2c);
    let pool_id = B256::repeat_byte(0x2d);
    let adapter = BalancerV2Adapter::default();
    let registration =
        PoolRegistration::new(PoolKey::BalancerV2(pool_id)).with_state_address(vault);
    let view = MockCache::default();

    // The indexed topics are present (poolId, tokenIn, tokenOut) but the
    // non-indexed body (amountIn, amountOut) is empty -> malformed.
    let result = adapter.decode_event(
        &registration,
        &log(
            vault,
            vec![
                balancer_swap_topic(),
                pool_id,
                topic_address(Address::repeat_byte(0x01)),
                topic_address(Address::repeat_byte(0x02)),
            ],
            Vec::new(),
        ),
        &view,
    );
    assert_eq!(
        result.error,
        Some(AdapterEventError::MalformedLog(
            "malformed Balancer V2 Swap log"
        ))
    );
}

#[test]
fn v2_cold_start_non_address_keyed_is_unsupported() {
    let adapter = UniswapV2Adapter::default();
    let registration = PoolRegistration::new(custom_bytes32_key());

    // A non-address-keyed pool cannot build a planner: the factory rejects it with
    // a `Custom` reason, which `AdapterRegistry::cold_start` maps to
    // `ColdStartOutcome::Unsupported(Custom(_))`.
    let reason = adapter
        .cold_start_planner(&registration, ColdStartPolicy::Eager)
        .err()
        .expect("a non-address-keyed V2 pool must be unsupported");
    assert!(matches!(reason, UnsupportedReason::Custom(_)));
}

#[test]
fn v3_no_topics_is_ignored() {
    let pool = Address::repeat_byte(0x2e);
    let adapter = ConcentratedLiquidityAdapter::default();
    let registration = v3_pool_registration(pool);
    let view = MockCache::default();

    let result = adapter.decode_event(&registration, &log(pool, Vec::new(), Vec::new()), &view);
    assert_eq!(result, AdapterEventResult::ignored());
}

#[test]
fn v3_unknown_topic_is_ignored() {
    let pool = Address::repeat_byte(0x2f);
    let adapter = ConcentratedLiquidityAdapter::default();
    let registration = v3_pool_registration(pool);
    let view = MockCache::default();

    // A topic that is neither Swap, Mint, nor Burn falls through to ignored.
    let result = adapter.decode_event(
        &registration,
        &log(pool, vec![B256::repeat_byte(0xab)], Vec::new()),
        &view,
    );
    assert_eq!(result, AdapterEventResult::ignored());
}

#[test]
fn v3_cold_start_non_address_keyed_is_unsupported() {
    let adapter = ConcentratedLiquidityAdapter::default();
    let registration =
        PoolRegistration::new(custom_bytes32_key()).with_metadata(ProtocolMetadata::UniswapV3(
            V3Metadata::default().with_storage_layout(V3StorageLayout::uniswap(60)),
        ));

    // The non-address-keyed check runs ahead of layout resolution, so even with a
    // resolvable layout the factory rejects the key with a `Custom` reason that
    // `AdapterRegistry::cold_start` maps to `Unsupported(Custom(_))`.
    let reason = adapter
        .cold_start_planner(&registration, ColdStartPolicy::Eager)
        .err()
        .expect("a non-address-keyed V3 pool must be unsupported");
    assert!(matches!(reason, UnsupportedReason::Custom(_)));
}

#[test]
fn registry_unregister_pool_and_adapter_lifecycle() {
    let mut registry = AdapterRegistry::new();
    registry
        .register_adapter(Arc::new(UniswapV2Adapter::default()))
        .expect("adapter registers");
    let key = PoolKey::UniswapV2(Address::repeat_byte(0x51));
    registry
        .register_pool(PoolRegistration::new(key.clone()))
        .expect("pool registers");

    // An adapter with a live pool cannot be unregistered.
    assert!(matches!(
        registry.unregister_adapter(ProtocolId::UniswapV2),
        Err(RegistryError::AdapterInUse { .. })
    ));

    // Unregistering the pool returns its registration and stops tracking it.
    let removed = registry
        .unregister_pool(&key)
        .expect("registration returned");
    assert_eq!(removed.key, key);
    assert!(registry.pool(&key).is_none());
    assert!(registry.unregister_pool(&key).is_none());

    // With no pools left the adapter can go; a second call is a clean no-op.
    let adapter = registry
        .unregister_adapter(ProtocolId::UniswapV2)
        .expect("no pools reference it")
        .expect("adapter was registered");
    assert_eq!(adapter.protocol(), ProtocolId::UniswapV2);
    assert!(registry.adapter(ProtocolId::UniswapV2).is_none());
    assert!(
        registry
            .unregister_adapter(ProtocolId::UniswapV2)
            .expect("no-op")
            .is_none()
    );
}

#[test]
fn registry_unregister_family_adapter_removes_every_served_id() {
    let mut registry = AdapterRegistry::new();
    registry
        .register_adapter(Arc::new(ConcentratedLiquidityAdapter::default()))
        .expect("family adapter registers");

    // A pool on ANY served id blocks unregistration through any other id.
    let key = PoolKey::PancakeV3(Address::repeat_byte(0x52));
    registry
        .register_pool(PoolRegistration::new(key.clone()))
        .expect("pool registers");
    assert!(matches!(
        registry.unregister_adapter(ProtocolId::UniswapV3),
        Err(RegistryError::AdapterInUse { .. })
    ));

    registry.unregister_pool(&key).expect("pool removed");
    registry
        .unregister_adapter(ProtocolId::UniswapV3)
        .expect("removable once pools are gone")
        .expect("adapter was registered");
    assert!(registry.adapter(ProtocolId::UniswapV3).is_none());
    assert!(registry.adapter(ProtocolId::PancakeV3).is_none());
    assert!(registry.adapter(ProtocolId::Slipstream).is_none());
}
