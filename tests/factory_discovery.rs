use std::collections::HashMap;
use std::sync::Arc;

use alloy_primitives::{Address, B256, Bytes, Log, U256, keccak256};
use anyhow::{Result, anyhow};
use evm_amm_state::adapters::factory::derive;
use evm_amm_state::adapters::{
    AdapterCache, AdapterRegistry, CallOutcome, ConcentratedLiquidityAdapter, CreationLogContext,
    DiscoverySource, FactoryConfig, PoolDiscovery, PoolKey, PoolQuery, ProtocolId,
    ProtocolMetadata, SlotChange, StateDiff, StateUpdate, StateView, UniswapV2Adapter,
    UniswapV2FactoryConfig, UniswapV3FactoryConfig, UniswapV3PoolQuery, V3Metadata,
};

#[derive(Default)]
struct TestCache {
    storage: HashMap<(Address, U256), U256>,
}

impl TestCache {
    fn with_storage(mut self, address: Address, slot: U256, value: U256) -> Self {
        self.storage.insert((address, slot), value);
        self
    }
}

impl StateView for TestCache {
    fn storage(&self, address: Address, slot: U256) -> Option<U256> {
        self.storage.get(&(address, slot)).copied()
    }
}

impl AdapterCache for TestCache {
    fn cached_storage(&self, address: Address, slot: U256) -> Option<U256> {
        self.storage(address, slot)
    }

    fn apply_updates(&mut self, updates: &[StateUpdate]) -> StateDiff {
        let mut diff = StateDiff::default();
        for update in updates {
            if let StateUpdate::Slot {
                address,
                slot,
                value,
            } = update
            {
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
        }
        diff
    }

    fn verify_slots(
        &mut self,
        slots: &[(Address, U256)],
    ) -> Result<Vec<SlotChange>, evm_amm_state::adapters::CacheError> {
        Ok(slots
            .iter()
            .map(|(address, slot)| SlotChange {
                address: *address,
                slot: *slot,
                old: U256::ZERO,
                new: self.storage(*address, *slot).unwrap_or_default(),
            })
            .collect())
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

    fn read_storage_slot(
        &mut self,
        address: Address,
        slot: U256,
    ) -> Result<U256, evm_amm_state::adapters::CacheError> {
        Ok(self.storage(address, slot).unwrap_or_default())
    }

    fn call_raw(
        &mut self,
        _from: Address,
        _to: Address,
        _calldata: Bytes,
        _commit: bool,
    ) -> Result<CallOutcome, evm_amm_state::adapters::CacheError> {
        Ok(CallOutcome::Halt {
            reason: "test cache does not execute calls".into(),
        })
    }
}

#[test]
fn v2_factory_query_returns_cold_start_ready_registration() -> Result<()> {
    let factory = Address::repeat_byte(0xf2);
    let token_a = Address::repeat_byte(0xbb);
    let token_b = Address::repeat_byte(0xaa);
    let pair = Address::repeat_byte(0x22);
    let query = PoolQuery::pair(token_a, token_b);
    let (token0, token1) = query.tokens;
    let v2_config = UniswapV2FactoryConfig::uniswap_v2(factory).with_fee_bps(30);
    let slot = derive::v2_get_pair_slot(v2_config.get_pair_base_slot, token0, token1);
    let mut cache = TestCache::default().with_storage(factory, slot, address_word(pair));
    let discovery = discovery_with_v2(v2_config);

    let found = discovery.find(&mut cache, ProtocolId::UniswapV2, query)?;

    assert_eq!(found.len(), 1);
    assert_eq!(found[0].key, PoolKey::UniswapV2(pair));
    assert_eq!(found[0].registration.state_addresses, vec![pair]);
    assert!(!found[0].registration.event_sources.is_empty());
    assert_eq!(found[0].source, DiscoverySource::Query);
    match &found[0].registration.metadata {
        ProtocolMetadata::UniswapV2(metadata) => {
            assert_eq!(metadata.token0, Some(token0));
            assert_eq!(metadata.token1, Some(token1));
            assert_eq!(metadata.fee_bps, Some(30));
        }
        other => panic!("expected UniswapV2 metadata, got {other:?}"),
    }
    Ok(())
}

#[test]
fn v2_factory_query_zero_mapping_answer_is_empty() -> Result<()> {
    let factory = Address::repeat_byte(0xf2);
    let v2_config = UniswapV2FactoryConfig::uniswap_v2(factory);
    let mut cache = TestCache::default();
    let discovery = discovery_with_v2(v2_config);

    let found = discovery.find(
        &mut cache,
        ProtocolId::UniswapV2,
        PoolQuery::pair(Address::repeat_byte(0x01), Address::repeat_byte(0x02)),
    )?;

    assert!(found.is_empty());
    Ok(())
}

#[test]
fn v3_uniswap_preset_uses_canonical_factory_mapping_slots() {
    let config = UniswapV3FactoryConfig::uniswap_v3(Address::repeat_byte(0xf3));

    assert_eq!(config.fee_amount_tick_spacing_base_slot, U256::from(4));
    assert_eq!(config.get_pool_base_slot, U256::from(5));
}

#[test]
fn v2_factory_creation_log_decodes_to_same_registration_shape() -> Result<()> {
    let factory = Address::repeat_byte(0xf2);
    let token0 = Address::repeat_byte(0x01);
    let token1 = Address::repeat_byte(0x02);
    let pair = Address::repeat_byte(0x55);
    let v2_config = UniswapV2FactoryConfig::uniswap_v2(factory).with_fee_bps(30);
    let discovery = discovery_with_v2(v2_config);
    let log = Log::new(
        factory,
        vec![
            keccak256("PairCreated(address,address,address,uint256)"),
            topic_address(token0),
            topic_address(token1),
        ],
        abi_words([address_word(pair), U256::from(12_u64)]).into(),
    )
    .expect("valid log");

    let found = discovery
        .decode_creation(&log, CreationLogContext::new(Some(456), Some(9)))?
        .expect("decoded creation");

    assert_eq!(found.key, PoolKey::UniswapV2(pair));
    assert_eq!(
        found.source,
        DiscoverySource::CreationEvent {
            block_number: Some(456),
            log_index: Some(9)
        }
    );
    match &found.registration.metadata {
        ProtocolMetadata::UniswapV2(metadata) => {
            assert_eq!(metadata.token0, Some(token0));
            assert_eq!(metadata.token1, Some(token1));
            assert_eq!(metadata.fee_bps, Some(30));
        }
        other => panic!("expected UniswapV2 metadata, got {other:?}"),
    }
    Ok(())
}

#[test]
fn v3_factory_query_fills_factory_metadata_and_enables_code_seed() -> Result<()> {
    let factory = Address::repeat_byte(0xf3);
    let token_a = Address::repeat_byte(0xcc);
    let token_b = Address::repeat_byte(0xaa);
    let pool = Address::repeat_byte(0x33);
    let fee = 500;
    let tick_spacing = 10;
    let query = UniswapV3PoolQuery::pair(token_a, token_b).with_fee_tier(fee);
    let (token0, token1) = query.tokens;
    let v3_config = UniswapV3FactoryConfig::uniswap_v3(factory).with_fee_tiers([fee]);
    let pool_slot = derive::v3_get_pool_slot(v3_config.get_pool_base_slot, token0, token1, fee);
    let tick_spacing_slot =
        derive::v3_fee_amount_tick_spacing_slot(v3_config.fee_amount_tick_spacing_base_slot, fee);
    let mut cache = TestCache::default()
        .with_storage(factory, pool_slot, address_word(pool))
        .with_storage(factory, tick_spacing_slot, U256::from(tick_spacing));
    let (registry, discovery) = discovery_with_v3(v3_config);

    let found = discovery.find_uniswap_v3(&mut cache, query)?;

    assert_eq!(found.len(), 1);
    assert_eq!(found[0].key, PoolKey::UniswapV3(pool));
    assert!(!found[0].registration.event_sources.is_empty());
    let ProtocolMetadata::UniswapV3(metadata) = &found[0].registration.metadata else {
        panic!("expected UniswapV3 metadata");
    };
    assert_eq!(metadata.token0, Some(token0));
    assert_eq!(metadata.token1, Some(token1));
    assert_eq!(metadata.fee, Some(fee));
    assert_eq!(metadata.tick_spacing, Some(tick_spacing));
    assert_eq!(metadata.factory, Some(factory));

    let adapter = registry
        .adapter(ProtocolId::UniswapV3)
        .ok_or_else(|| anyhow!("missing V3 adapter"))?;
    let seeds = adapter
        .code_seeds(&found[0].registration)
        .map_err(|reason| anyhow!("{reason:?}"))?;
    assert_eq!(seeds.len(), 1);
    assert_eq!(seeds[0].address, pool);
    assert_eq!(seeds[0].runtime_bytecode.len(), 22_142);
    Ok(())
}

#[test]
fn v3_factory_creation_log_decodes_to_same_registration_shape() -> Result<()> {
    let factory = Address::repeat_byte(0xf3);
    let token0 = Address::repeat_byte(0x01);
    let token1 = Address::repeat_byte(0x02);
    let pool = Address::repeat_byte(0x44);
    let fee = 3_000;
    let tick_spacing = 60;
    let v3_config = UniswapV3FactoryConfig::uniswap_v3(factory);
    let (_registry, discovery) = discovery_with_v3(v3_config);
    let log = Log::new(
        factory,
        vec![
            keccak256("PoolCreated(address,address,uint24,int24,address)"),
            topic_address(token0),
            topic_address(token1),
            topic_u24(fee),
        ],
        abi_words([U256::from(tick_spacing), address_word(pool)]).into(),
    )
    .expect("valid log");

    let found = discovery
        .decode_creation(&log, CreationLogContext::new(Some(123), Some(7)))?
        .expect("decoded creation");

    assert_eq!(found.key, PoolKey::UniswapV3(pool));
    assert_eq!(
        found.source,
        DiscoverySource::CreationEvent {
            block_number: Some(123),
            log_index: Some(7)
        }
    );
    match &found.registration.metadata {
        ProtocolMetadata::UniswapV3(V3Metadata {
            token0: actual_token0,
            token1: actual_token1,
            fee: actual_fee,
            tick_spacing: actual_tick_spacing,
            factory: actual_factory,
            ..
        }) => {
            assert_eq!(*actual_token0, Some(token0));
            assert_eq!(*actual_token1, Some(token1));
            assert_eq!(*actual_fee, Some(fee));
            assert_eq!(*actual_tick_spacing, Some(tick_spacing));
            assert_eq!(*actual_factory, Some(factory));
        }
        other => panic!("expected UniswapV3 metadata, got {other:?}"),
    }
    Ok(())
}

fn discovery_with_v2(config: UniswapV2FactoryConfig) -> PoolDiscovery {
    let mut registry = AdapterRegistry::new();
    registry
        .register_adapter(Arc::new(UniswapV2Adapter::default()))
        .unwrap();
    PoolDiscovery::for_registry(&registry, FactoryConfig::default().with_uniswap_v2(config))
}

fn discovery_with_v3(config: UniswapV3FactoryConfig) -> (AdapterRegistry, PoolDiscovery) {
    let mut registry = AdapterRegistry::new();
    registry
        .register_adapter(Arc::new(ConcentratedLiquidityAdapter::default()))
        .unwrap();
    let discovery =
        PoolDiscovery::for_registry(&registry, FactoryConfig::default().with_uniswap_v3(config));
    (registry, discovery)
}

fn address_word(address: Address) -> U256 {
    U256::from_be_slice(address.as_slice())
}

fn topic_address(address: Address) -> B256 {
    let mut bytes = [0u8; 32];
    bytes[12..].copy_from_slice(address.as_slice());
    B256::from(bytes)
}

fn topic_u24(value: u32) -> B256 {
    let mut bytes = [0u8; 32];
    bytes[29..32].copy_from_slice(&value.to_be_bytes()[1..4]);
    B256::from(bytes)
}

fn abi_words(values: impl IntoIterator<Item = U256>) -> Vec<u8> {
    values
        .into_iter()
        .flat_map(|value| value.to_be_bytes::<32>().to_vec())
        .collect()
}
