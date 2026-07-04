//! MANAGER-AUTHORED acceptance tests for factory-discovery cleanup (Cycle 2):
//! multiple factories per protocol, and a custom-factory open channel. The
//! implementation agent must make these pass WITHOUT modifying them.
//!
//! Runs fully offline over an in-memory `AdapterCache`.

use std::collections::HashMap;
use std::sync::Arc;

use alloy_primitives::{Address, Bytes, U256};
use anyhow::Result;
use evm_amm_state::adapters::factory::derive;
use evm_amm_state::adapters::{
    AdapterCache, AdapterRegistry, CallOutcome, CreationLogContext, DiscoveredPool, DiscoverySource,
    FactoryConfig, PoolDiscovery, PoolFactory, PoolKey, PoolQuery, PoolRegistration, ProtocolId,
    ProtocolMetadata, SlotChange, StateDiff, StateUpdate, StateView, UniswapV2Adapter,
    UniswapV2FactoryConfig, UniswapV2Metadata,
};
use alloy_primitives::Log;

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
    fn apply_updates(&mut self, _updates: &[StateUpdate]) -> StateDiff {
        StateDiff::default()
    }
    fn verify_slots(
        &mut self,
        slots: &[(Address, U256)],
    ) -> std::result::Result<Vec<SlotChange>, evm_amm_state::adapters::CacheError> {
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
    fn purge_storage(&mut self, _address: Address) -> StateDiff {
        StateDiff::default()
    }
    fn purge_slots(&mut self, _address: Address, _slots: &[U256]) -> StateDiff {
        StateDiff::default()
    }
    fn read_storage_slot(
        &mut self,
        address: Address,
        slot: U256,
    ) -> std::result::Result<U256, evm_amm_state::adapters::CacheError> {
        Ok(self.storage(address, slot).unwrap_or_default())
    }
    fn call_raw(
        &mut self,
        _from: Address,
        _to: Address,
        _calldata: Bytes,
        _commit: bool,
    ) -> std::result::Result<CallOutcome, evm_amm_state::adapters::CacheError> {
        Ok(CallOutcome::Halt {
            reason: "test cache does not execute calls".into(),
        })
    }
}

fn address_word(address: Address) -> U256 {
    U256::from_be_slice(address.as_slice())
}

/// Two Uniswap-V2-style factories (e.g. Uniswap + a Sushi fork) share
/// `ProtocolId::UniswapV2`. Both must be retained and both must resolve — the
/// old `HashMap<ProtocolId, _>` keying dropped all but the first.
#[test]
fn two_v2_style_factories_both_resolve() -> Result<()> {
    let uni_factory = Address::repeat_byte(0xf1);
    let sushi_factory = Address::repeat_byte(0xf2);
    let token_a = Address::repeat_byte(0xaa);
    let token_b = Address::repeat_byte(0xbb);
    let uni_pair = Address::repeat_byte(0x11);
    let sushi_pair = Address::repeat_byte(0x22);

    let query = PoolQuery::pair(token_a, token_b);
    let (token0, token1) = query.tokens;

    let uni_cfg = UniswapV2FactoryConfig::uniswap_v2(uni_factory).with_fee_bps(30);
    let sushi_cfg = UniswapV2FactoryConfig::uniswap_v2(sushi_factory).with_fee_bps(30);
    let uni_slot = derive::v2_get_pair_slot(uni_cfg.get_pair_base_slot, token0, token1);
    let sushi_slot = derive::v2_get_pair_slot(sushi_cfg.get_pair_base_slot, token0, token1);

    let mut cache = TestCache::default()
        .with_storage(uni_factory, uni_slot, address_word(uni_pair))
        .with_storage(sushi_factory, sushi_slot, address_word(sushi_pair));

    let mut registry = AdapterRegistry::new();
    registry.register_adapter(Arc::new(UniswapV2Adapter::default()))?;
    let discovery = PoolDiscovery::for_registry(
        &registry,
        FactoryConfig::default()
            .with_uniswap_v2(uni_cfg)
            .with_uniswap_v2(sushi_cfg),
    );

    let found = discovery.find(&mut cache, ProtocolId::UniswapV2, query)?;

    let mut pairs: Vec<Address> = found
        .iter()
        .filter_map(|p| p.key.address())
        .collect();
    pairs.sort();
    assert_eq!(
        pairs,
        {
            let mut want = vec![uni_pair, sushi_pair];
            want.sort();
            want
        },
        "both same-protocol factories must resolve; got {found:?}"
    );
    Ok(())
}

/// A caller can extend a registry-built discovery with a custom `PoolFactory`
/// through a public open channel (`with_factory`), and that factory is used.
#[test]
fn custom_factory_open_channel_is_used() -> Result<()> {
    let custom_pool = Address::repeat_byte(0x99);
    let mut registry = AdapterRegistry::new();
    registry.register_adapter(Arc::new(UniswapV2Adapter::default()))?;

    let discovery = PoolDiscovery::for_registry(&registry, FactoryConfig::default())
        .with_factory(Box::new(StubFactory {
            factory: Address::repeat_byte(0xcf),
            pool: custom_pool,
        }));

    let mut cache = TestCache::default();
    let found = discovery.find(
        &mut cache,
        ProtocolId::UniswapV2,
        PoolQuery::pair(Address::repeat_byte(0x01), Address::repeat_byte(0x02)),
    )?;

    assert_eq!(found.len(), 1, "custom factory must be consulted");
    assert_eq!(found[0].key.address(), Some(custom_pool));
    Ok(())
}

/// A minimal external `PoolFactory` a downstream crate could write.
struct StubFactory {
    factory: Address,
    pool: Address,
}

impl PoolFactory for StubFactory {
    fn protocol(&self) -> ProtocolId {
        ProtocolId::UniswapV2
    }

    fn factory_address(&self) -> Address {
        self.factory
    }

    fn find_pools(
        &self,
        _cache: &mut dyn AdapterCache,
        _query: &evm_amm_state::adapters::FactoryQuery,
    ) -> std::result::Result<Vec<DiscoveredPool>, evm_amm_state::adapters::DiscoveryError> {
        let registration = PoolRegistration::new(PoolKey::UniswapV2(self.pool))
            .with_state_address(self.pool)
            .with_metadata(ProtocolMetadata::UniswapV2(UniswapV2Metadata::default()));
        Ok(vec![DiscoveredPool::new(
            registration.key.clone(),
            registration,
            DiscoverySource::Query,
        )])
    }

    fn creation_sources(&self) -> Vec<evm_amm_state::adapters::EventSource> {
        Vec::new()
    }

    fn decode_creation(
        &self,
        _log: &Log,
        _context: CreationLogContext,
    ) -> std::result::Result<Option<DiscoveredPool>, evm_amm_state::adapters::DiscoveryError> {
        Ok(None)
    }
}
