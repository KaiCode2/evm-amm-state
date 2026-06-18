use std::collections::HashMap;
use std::sync::Arc;

use alloy_primitives::{Address, B256, Bytes, Log, U256, keccak256};
use anyhow::Result;
use evm_amm_state::adapters::storage::{
    V2_RESERVES_SLOT, V3_LIQUIDITY_SLOT, V3_SLOT0_SLOT, V3StorageLayout,
};
use evm_amm_state::adapters::{
    AdapterCache, AdapterDriver, AdapterEvent, AdapterEventKind, AdapterEventResult,
    AdapterRegistry, AmmAdapter, BalancerV2Adapter, BalancerV2Metadata, EventSource, PoolKey,
    PoolRegistration, ProtocolId, ProtocolMetadata, RegistryError, RepairAction, SkippedDelta,
    SkippedMask, SlotChange, SlotDelta, StateDiff, StateUpdate, StateView, SubscriptionSpec,
    UniswapV2Adapter, UniswapV3Adapter, UpdateQuality, V3Metadata,
};
use revm::context::result::ExecutionResult;

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
                        diff.slots.push(SlotChange {
                            address: *address,
                            slot: *slot,
                            old,
                            new: *value,
                        });
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
                            diff.slots.push(SlotChange {
                                address: *address,
                                slot: *slot,
                                old,
                                new,
                            });
                        }
                    } else {
                        diff.skipped_masks.push(SkippedMask {
                            address: *address,
                            slot: *slot,
                            mask: *mask,
                            value: *value,
                        });
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
                            diff.slots.push(SlotChange {
                                address: *address,
                                slot: *slot,
                                old,
                                new,
                            });
                        }
                    } else {
                        diff.skipped.push(SkippedDelta {
                            address: *address,
                            slot: *slot,
                            delta: *delta,
                        });
                    }
                }
                StateUpdate::Purge { address, .. } => {
                    self.storage.retain(|(stored, _), _| stored != address);
                }
                StateUpdate::BalanceDelta { .. }
                | StateUpdate::Account { .. }
                | StateUpdate::AccountUpsert { .. } => {}
                _ => panic!("unexpected StateUpdate variant in adapter A1 mock cache"),
            }
        }

        diff
    }

    fn verify_slots(&mut self, _slots: &[(Address, U256)]) -> Result<Vec<SlotChange>> {
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

    fn read_storage_slot(&mut self, address: Address, slot: U256) -> Result<U256> {
        self.value(address, slot)
            .ok_or_else(|| anyhow::anyhow!("slot is cold"))
    }

    fn call_raw(
        &mut self,
        _from: Address,
        _to: Address,
        _calldata: Bytes,
        _commit: bool,
    ) -> Result<ExecutionResult> {
        anyhow::bail!("mock cache does not execute calls")
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
        AdapterEventResult::event(AdapterEvent {
            pool: pool.key.clone(),
            emitter: log.address,
            topic0: self.topic,
            kind: AdapterEventKind::Swap,
            updates: vec![
                StateUpdate::slot(address, self.slot, next),
                StateUpdate::slot_masked(address, self.cold_slot, U256::MAX, U256::from(9)),
            ],
            quality: UpdateQuality::ExactIfApplied,
            repair: RepairAction::None,
        })
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

    let SubscriptionSpec { sources } = registry.subscription_spec();
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

#[test]
fn uniswap_v2_sync_updates_reserves_without_clobbering_timestamp() {
    let pool = Address::repeat_byte(0xbb);
    let reserve0 = U256::from(123_u64);
    let reserve1 = U256::from(456_u64);
    let timestamp = U256::from(0x1234_u64);
    let initial_slot = timestamp << 224;

    let adapter = Arc::new(UniswapV2Adapter::default());
    let mut registration = PoolRegistration::new(PoolKey::UniswapV2(pool)).with_state_address(pool);
    registration = registration.with_event_sources(adapter.event_sources(&registration));

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
fn uniswap_v3_swap_emits_masked_slot0_and_liquidity_update() {
    let pool = Address::repeat_byte(0xcc);
    let sender = Address::repeat_byte(0x01);
    let recipient = Address::repeat_byte(0x02);
    let sqrt_price = U256::from(12_345_u64);
    let liquidity = U256::from(67_890_u64);
    let tick = U256::from(42_u64);
    let preserved_high_bits = U256::from(0xabcdef_u64) << 184;

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

    let mut cache = MockCache::default();
    cache.seed(pool, V3_SLOT0_SLOT, preserved_high_bits);

    let driver = AdapterDriver::new(registry);
    let report = driver
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
        .unwrap()
        .expect("swap should decode");

    let raw_slot0 = cache.value(pool, V3_SLOT0_SLOT).unwrap();
    assert_eq!(raw_slot0 & low_mask(160), sqrt_price);
    assert_eq!((raw_slot0 >> 160) & low_mask(24), tick);
    assert_eq!(raw_slot0 & !low_mask(184), preserved_high_bits);
    assert_eq!(cache.value(pool, V3_LIQUIDITY_SLOT), Some(liquidity));
    assert_eq!(report.event.kind, AdapterEventKind::Swap);
    assert_eq!(report.event.quality, UpdateQuality::ExactIfApplied);
    assert!(!report.applied.has_skipped());
}

#[test]
fn balancer_v2_adapter_routes_vault_swap_by_pool_id() {
    let vault = Address::repeat_byte(0xdd);
    let pool_id = B256::repeat_byte(0xee);

    let adapter = Arc::new(BalancerV2Adapter::default());
    let mut registration = PoolRegistration::new(PoolKey::BalancerV2(pool_id))
        .with_state_address(vault)
        .with_metadata(ProtocolMetadata::BalancerV2(BalancerV2Metadata {
            vault: Some(vault),
            ..Default::default()
        }));
    registration = registration.with_event_sources(adapter.event_sources(&registration));
    let swap_topic = registration.event_sources[0].topics[0];

    let mut registry = AdapterRegistry::new();
    registry.register_adapter(adapter).unwrap();
    registry.register_pool(registration).unwrap();

    let routed = registry
        .route_log(&log(vault, vec![swap_topic, pool_id], Vec::new()))
        .expect("vault swap should route by pool id");
    assert_eq!(routed.key, PoolKey::BalancerV2(pool_id));
}
