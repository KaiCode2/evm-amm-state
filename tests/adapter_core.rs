use alloy_primitives::{Address, B256, Bytes, Log, U256};
use evm_amm_state::adapters::storage::{
    PANCAKE_V3_TICK_BITMAP_BASE_SLOT, V2_RESERVES_SLOT, V3_TICK_BITMAP_BASE_SLOT,
    V3_TICKS_BASE_SLOT, V3StorageLayout, v3_tick_bitmap_storage_key_with_base,
    v3_tick_info_storage_keys_with_base,
};
use evm_amm_state::adapters::{
    AdapterRegistry, EventRoute, EventSource, PoolKey, PoolRegistration, ProtocolId,
};

fn log(address: Address, topics: Vec<B256>) -> Log {
    Log::new(address, topics, Bytes::new()).expect("valid test log")
}

#[test]
fn pool_key_preserves_protocol_specific_identity() {
    let address_key = Address::repeat_byte(0x11);
    let bytes32_key = B256::repeat_byte(0x22);

    let v3 = PoolKey::UniswapV3(address_key);
    assert_eq!(v3.protocol(), ProtocolId::UniswapV3);
    assert_eq!(v3.address(), Some(address_key));
    assert_eq!(v3.bytes32(), None);

    let balancer = PoolKey::BalancerV2(bytes32_key);
    assert_eq!(balancer.protocol(), ProtocolId::BalancerV2);
    assert_eq!(balancer.address(), None);
    assert_eq!(balancer.bytes32(), Some(bytes32_key));
}

#[test]
fn registry_routes_direct_pool_events_by_emitter_and_topic() {
    let pool = Address::repeat_byte(0x33);
    let sync_topic = B256::repeat_byte(0xaa);
    let wrong_topic = B256::repeat_byte(0xbb);
    let key = PoolKey::UniswapV2(pool);

    let registration = PoolRegistration::new(key.clone())
        .with_state_address(pool)
        .with_event_source(EventSource::direct(pool, vec![sync_topic]));

    let mut registry = AdapterRegistry::new();
    registry.register_pool(registration).unwrap();

    assert_eq!(registry.subscription_topics(), vec![sync_topic]);
    assert_eq!(
        registry
            .route_log(&log(pool, vec![sync_topic]))
            .map(|pool| &pool.key),
        Some(&key)
    );
    assert!(registry.route_log(&log(pool, vec![wrong_topic])).is_none());
    assert!(
        registry
            .route_log(&log(Address::repeat_byte(0x34), vec![sync_topic]))
            .is_none()
    );
}

#[test]
fn registry_routes_bytes32_indexed_vault_events() {
    let vault = Address::repeat_byte(0x44);
    let swap_topic = B256::repeat_byte(0xcc);
    let pool_id = B256::repeat_byte(0xdd);
    let other_pool_id = B256::repeat_byte(0xee);
    let key = PoolKey::BalancerV2(pool_id);

    let registration = PoolRegistration::new(key.clone())
        .with_state_address(vault)
        .with_event_source(EventSource::indexed_bytes32(vault, vec![swap_topic], 1));

    let mut registry = AdapterRegistry::new();
    registry.register_pool(registration).unwrap();

    assert_eq!(
        registry
            .route_log(&log(vault, vec![swap_topic, pool_id]))
            .map(|pool| &pool.key),
        Some(&key)
    );
    assert!(
        registry
            .route_log(&log(vault, vec![swap_topic, other_pool_id]))
            .is_none()
    );

    let source = &registry.pool(&key).unwrap().event_sources[0];
    assert_eq!(source.route, EventRoute::IndexedBytes32 { topic_index: 1 });
}

#[test]
fn storage_layout_helpers_are_available_from_this_crate() {
    assert_eq!(V2_RESERVES_SLOT, U256::from(8));

    let uniswap = V3StorageLayout::uniswap(60);
    assert_eq!(uniswap.slot0_slot, U256::ZERO);
    assert_eq!(uniswap.liquidity_slot, U256::from(4));
    assert_eq!(uniswap.ticks_base_slot, V3_TICKS_BASE_SLOT);
    assert_eq!(uniswap.tick_bitmap_base_slot, V3_TICK_BITMAP_BASE_SLOT);
    assert_eq!(uniswap.tick_spacing, 60);

    let uni_bitmap = v3_tick_bitmap_storage_key_with_base(10, V3_TICK_BITMAP_BASE_SLOT);
    let pancake_bitmap = v3_tick_bitmap_storage_key_with_base(10, PANCAKE_V3_TICK_BITMAP_BASE_SLOT);
    assert_ne!(uni_bitmap, pancake_bitmap);

    let keys = v3_tick_info_storage_keys_with_base(-100, V3_TICKS_BASE_SLOT);
    assert_eq!(keys[1], keys[0] + U256::from(1));
    assert_eq!(keys[2], keys[0] + U256::from(2));
    assert_eq!(keys[3], keys[0] + U256::from(3));
}
