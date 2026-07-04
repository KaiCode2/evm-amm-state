//! Token-basket discovery: `PoolDiscovery::find_pairs_among` resolves every
//! pool joining any pair of a token set, across all factories, in ONE batched
//! storage read.

use std::collections::HashMap;
use std::sync::Arc;

use alloy_primitives::{Address, Bytes, U256};
use anyhow::Result;
use evm_amm_state::adapters::factory::derive;
use evm_amm_state::adapters::{
    AdapterCache, AdapterRegistry, CacheError, CallOutcome, ConcentratedLiquidityAdapter,
    FactoryConfig, PoolDiscovery, PoolKey, ProtocolMetadata, SlotChange, StateDiff, StateUpdate,
    StateView, UniswapV2Adapter, UniswapV2FactoryConfig, UniswapV3FactoryConfig,
};

#[derive(Default)]
struct CountingCache {
    storage: HashMap<(Address, U256), U256>,
    single_reads: usize,
    batch_reads: usize,
    slots_read: usize,
}

impl CountingCache {
    fn set(&mut self, address: Address, slot: U256, value: U256) {
        self.storage.insert((address, slot), value);
    }
}

impl StateView for CountingCache {
    fn storage(&self, address: Address, slot: U256) -> Option<U256> {
        self.storage.get(&(address, slot)).copied()
    }
}

impl AdapterCache for CountingCache {
    fn cached_storage(&self, address: Address, slot: U256) -> Option<U256> {
        self.storage(address, slot)
    }
    fn apply_updates(&mut self, _updates: &[StateUpdate]) -> StateDiff {
        StateDiff::default()
    }
    fn verify_slots(&mut self, slots: &[(Address, U256)]) -> Result<Vec<SlotChange>, CacheError> {
        Ok(slots
            .iter()
            .map(|(a, s)| SlotChange {
                address: *a,
                slot: *s,
                old: U256::ZERO,
                new: self.storage(*a, *s).unwrap_or_default(),
            })
            .collect())
    }
    fn purge_storage(&mut self, _address: Address) -> StateDiff {
        StateDiff::default()
    }
    fn purge_slots(&mut self, _address: Address, _slots: &[U256]) -> StateDiff {
        StateDiff::default()
    }
    fn read_storage_slot(&mut self, address: Address, slot: U256) -> Result<U256, CacheError> {
        self.single_reads += 1;
        Ok(self.storage(address, slot).unwrap_or_default())
    }
    fn read_storage_slots(&mut self, slots: &[(Address, U256)]) -> Result<Vec<U256>, CacheError> {
        self.batch_reads += 1;
        self.slots_read += slots.len();
        Ok(slots
            .iter()
            .map(|(a, s)| self.storage(*a, *s).unwrap_or_default())
            .collect())
    }
    fn call_raw(
        &mut self,
        _from: Address,
        _to: Address,
        _calldata: Bytes,
        _commit: bool,
    ) -> Result<CallOutcome, CacheError> {
        Ok(CallOutcome::Halt {
            reason: "test cache does not execute calls".into(),
        })
    }
}

fn word(address: Address) -> U256 {
    U256::from_be_slice(address.as_slice())
}

#[test]
fn find_pairs_among_resolves_whole_basket_in_one_batched_read() -> Result<()> {
    let v2_factory = Address::repeat_byte(0xf2);
    let v3_factory = Address::repeat_byte(0xf3);
    // A < B < C by byte order, so pairs_among yields (A,B), (A,C), (B,C).
    let a = Address::repeat_byte(0x0a);
    let b = Address::repeat_byte(0x0b);
    let c = Address::repeat_byte(0x0c);
    let v2_pair_ab = Address::repeat_byte(0x21);
    let v3_pool_ac_500 = Address::repeat_byte(0x35);

    let v2_config = UniswapV2FactoryConfig::uniswap_v2(v2_factory).with_fee_bps(30);
    let v3_config = UniswapV3FactoryConfig::uniswap_v3(v3_factory); // default 4 fee tiers

    let mut cache = CountingCache::default();
    // One V2 pool: A/B.
    cache.set(
        v2_factory,
        derive::v2_get_pair_slot(v2_config.get_pair_base_slot, a, b),
        word(v2_pair_ab),
    );
    // One V3 pool: A/C at the 0.05% tier (tickSpacing 10).
    cache.set(
        v3_factory,
        derive::v3_get_pool_slot(v3_config.get_pool_base_slot, a, c, 500),
        word(v3_pool_ac_500),
    );
    cache.set(
        v3_factory,
        derive::v3_fee_amount_tick_spacing_slot(v3_config.fee_amount_tick_spacing_base_slot, 500),
        U256::from(10),
    );

    let mut registry = AdapterRegistry::new();
    registry.register_adapter(Arc::new(UniswapV2Adapter::default()))?;
    registry.register_adapter(Arc::new(ConcentratedLiquidityAdapter::default()))?;
    let discovery = PoolDiscovery::for_registry(
        &registry,
        FactoryConfig::default()
            .with_uniswap_v2(v2_config)
            .with_uniswap_v3(v3_config),
    );

    let found = discovery.find_pairs_among(&mut cache, &[a, b, c])?;

    let addrs: std::collections::HashSet<Option<Address>> =
        found.iter().map(|p| p.key.address()).collect();
    assert!(addrs.contains(&Some(v2_pair_ab)), "V2 A/B pool must be found");
    assert!(addrs.contains(&Some(v3_pool_ac_500)), "V3 A/C 0.05% pool must be found");
    assert_eq!(found.len(), 2, "exactly the two existing pools; got {found:?}");

    // The whole basket — both factories, all 3 pairs, all 4 V3 fee tiers — must
    // resolve in exactly ONE batched read and zero single reads.
    assert_eq!(cache.single_reads, 0, "no one-at-a-time reads");
    assert_eq!(
        cache.batch_reads, 1,
        "the entire basket resolves in a single batched read; got {} batched reads",
        cache.batch_reads
    );
    // 3 V2 getPair + 4 V3 tickSpacing + 3 pairs * 4 fees getPool = 19 slots.
    assert_eq!(cache.slots_read, 19, "3 + 4 + 12 candidate slots in the one batch");

    // Sanity: the returned V2 pool carries V2 metadata; V3 carries a fee/layout.
    let v2 = found
        .iter()
        .find(|p| p.key == PoolKey::UniswapV2(v2_pair_ab))
        .unwrap();
    assert!(matches!(v2.registration.metadata, ProtocolMetadata::UniswapV2(_)));
    let v3 = found
        .iter()
        .find(|p| p.key == PoolKey::UniswapV3(v3_pool_ac_500))
        .unwrap();
    match &v3.registration.metadata {
        ProtocolMetadata::UniswapV3(m) => {
            assert_eq!(m.fee, Some(500));
            assert_eq!(m.tick_spacing, Some(10));
            assert_eq!(m.factory, Some(v3_factory));
        }
        other => panic!("expected V3 metadata, got {other:?}"),
    }
    Ok(())
}

#[test]
fn find_pairs_among_empty_or_single_token_is_empty() -> Result<()> {
    let mut registry = AdapterRegistry::new();
    registry.register_adapter(Arc::new(UniswapV2Adapter::default()))?;
    let discovery = PoolDiscovery::for_registry(
        &registry,
        FactoryConfig::default()
            .with_uniswap_v2(UniswapV2FactoryConfig::uniswap_v2(Address::repeat_byte(0xf2))),
    );
    let mut cache = CountingCache::default();
    assert!(discovery.find_pairs_among(&mut cache, &[])?.is_empty());
    assert!(
        discovery
            .find_pairs_among(&mut cache, &[Address::repeat_byte(0x01)])?
            .is_empty()
    );
    assert_eq!(cache.batch_reads, 0, "no pairs → no reads");
    Ok(())
}
