//! MANAGER-AUTHORED acceptance tests for the unified pool-discovery surface:
//! ONE `PoolDiscovery::find(cache, PoolQuery)` method, with a fluent `PoolQuery`
//! (`pair` / `basket` / `pairs`, optionally `.on(protocol)`). The
//! implementation agent must make these pass WITHOUT modifying them.
//!
//! Every query resolves in a single batched `read_storage_slots` (one bulk
//! `eth_call` on a real cache), across all matching factories, with a per-query
//! fallback for factories that only implement `find_pools`.

use std::collections::HashMap;
use std::sync::Arc;

use alloy_primitives::{Address, B256, Bytes, Log, U256, keccak256};
use anyhow::{Result, anyhow};
use evm_amm_state::adapters::factory::derive;
use evm_amm_state::adapters::{
    AdapterCache, AdapterRegistry, CacheError, CallOutcome, ConcentratedLiquidityAdapter,
    CreationLogContext, DiscoveredPool, DiscoveryError, DiscoverySource, EventSource,
    FactoryConfig, PoolDiscovery, PoolFactory, PoolKey, PoolQuery, PoolRegistration, ProtocolId,
    ProtocolMetadata, SlotChange, StateDiff, StateUpdate, StateView, UniswapV2Adapter,
    UniswapV2FactoryConfig, UniswapV2Metadata, UniswapV3FactoryConfig,
};

// --- counting cache ---

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

fn registry_v2_v3() -> AdapterRegistry {
    let mut registry = AdapterRegistry::new();
    registry
        .register_adapter(Arc::new(UniswapV2Adapter::default()))
        .unwrap();
    registry
        .register_adapter(Arc::new(ConcentratedLiquidityAdapter::default()))
        .unwrap();
    registry
}

// --- one query, one batched request ---

/// `find(PoolQuery::pair(..))` with no protocol filter resolves every matching
/// factory (V2 + V3) in ONE batched read.
#[test]
fn find_pair_resolves_all_protocols_in_one_read() -> Result<()> {
    let v2_factory = Address::repeat_byte(0xf2);
    let v3_factory = Address::repeat_byte(0xf3);
    let a = Address::repeat_byte(0x0a);
    let b = Address::repeat_byte(0x0b);
    let v2_pair = Address::repeat_byte(0x21);
    let v3_pool = Address::repeat_byte(0x35);

    let v2_cfg = UniswapV2FactoryConfig::uniswap_v2(v2_factory).with_fee_bps(30);
    let v3_cfg = UniswapV3FactoryConfig::uniswap_v3(v3_factory);
    let (t0, t1) = derive::sort_tokens(a, b);

    let mut cache = CountingCache::default();
    cache.set(
        v2_factory,
        derive::v2_get_pair_slot(v2_cfg.get_pair_base_slot, t0, t1),
        word(v2_pair),
    );
    cache.set(
        v3_factory,
        derive::v3_get_pool_slot(v3_cfg.get_pool_base_slot, t0, t1, 500),
        word(v3_pool),
    );
    cache.set(
        v3_factory,
        derive::v3_fee_amount_tick_spacing_slot(v3_cfg.fee_amount_tick_spacing_base_slot, 500),
        U256::from(10),
    );

    let discovery = PoolDiscovery::for_registry(
        &registry_v2_v3(),
        FactoryConfig::default()
            .with_uniswap_v2(v2_cfg)
            .with_uniswap_v3(v3_cfg),
    );

    let found = discovery.find(&mut cache, PoolQuery::pair(a, b))?;

    let addrs: std::collections::HashSet<Option<Address>> =
        found.iter().map(|p| p.key.address()).collect();
    assert!(addrs.contains(&Some(v2_pair)) && addrs.contains(&Some(v3_pool)));
    assert_eq!(cache.single_reads, 0);
    assert_eq!(
        cache.batch_reads, 1,
        "one batched read across both protocols"
    );
    Ok(())
}

/// `.on(protocol)` filters discovery to a single protocol.
#[test]
fn find_pair_on_protocol_filters() -> Result<()> {
    let v2_factory = Address::repeat_byte(0xf2);
    let v3_factory = Address::repeat_byte(0xf3);
    let a = Address::repeat_byte(0x0a);
    let b = Address::repeat_byte(0x0b);
    let v2_pair = Address::repeat_byte(0x21);
    let v3_pool = Address::repeat_byte(0x35);

    let v2_cfg = UniswapV2FactoryConfig::uniswap_v2(v2_factory).with_fee_bps(30);
    let v3_cfg = UniswapV3FactoryConfig::uniswap_v3(v3_factory);
    let (t0, t1) = derive::sort_tokens(a, b);

    let mut cache = CountingCache::default();
    cache.set(
        v2_factory,
        derive::v2_get_pair_slot(v2_cfg.get_pair_base_slot, t0, t1),
        word(v2_pair),
    );
    cache.set(
        v3_factory,
        derive::v3_get_pool_slot(v3_cfg.get_pool_base_slot, t0, t1, 500),
        word(v3_pool),
    );
    cache.set(
        v3_factory,
        derive::v3_fee_amount_tick_spacing_slot(v3_cfg.fee_amount_tick_spacing_base_slot, 500),
        U256::from(10),
    );

    let discovery = PoolDiscovery::for_registry(
        &registry_v2_v3(),
        FactoryConfig::default()
            .with_uniswap_v2(v2_cfg)
            .with_uniswap_v3(v3_cfg),
    );

    let v2_only = discovery.find(&mut cache, PoolQuery::pair(a, b).on(ProtocolId::UniswapV2))?;
    assert_eq!(v2_only.len(), 1);
    assert_eq!(v2_only[0].key, PoolKey::UniswapV2(v2_pair));

    let v3_only = discovery.find(&mut cache, PoolQuery::pair(a, b).on(ProtocolId::UniswapV3))?;
    assert_eq!(v3_only.len(), 1);
    assert_eq!(v3_only[0].key, PoolKey::UniswapV3(v3_pool));
    Ok(())
}

/// `.on(protocol)` for a protocol with no registered factory errors
/// `MissingFactory`; an unfiltered query never does.
#[test]
fn find_on_absent_protocol_errors_but_unfiltered_does_not() -> Result<()> {
    let v2_factory = Address::repeat_byte(0xf2);
    let mut registry = AdapterRegistry::new();
    registry.register_adapter(Arc::new(UniswapV2Adapter::default()))?;
    let discovery = PoolDiscovery::for_registry(
        &registry,
        FactoryConfig::default().with_uniswap_v2(UniswapV2FactoryConfig::uniswap_v2(v2_factory)),
    );
    let mut cache = CountingCache::default();

    let err = discovery
        .find(
            &mut cache,
            PoolQuery::pair(Address::repeat_byte(1), Address::repeat_byte(2))
                .on(ProtocolId::UniswapV3),
        )
        .unwrap_err();
    assert!(matches!(
        err,
        DiscoveryError::MissingFactory(ProtocolId::UniswapV3)
    ));

    // No filter → best-effort across registered factories, no error.
    let found = discovery.find(
        &mut cache,
        PoolQuery::pair(Address::repeat_byte(1), Address::repeat_byte(2)),
    )?;
    assert!(found.is_empty());
    Ok(())
}

/// `find(PoolQuery::basket(..))` expands to all `C(n,2)` pairs and resolves the
/// whole basket — both factories, all fee tiers — in ONE batched read.
#[test]
fn find_basket_resolves_all_pairs_in_one_read() -> Result<()> {
    let v2_factory = Address::repeat_byte(0xf2);
    let v3_factory = Address::repeat_byte(0xf3);
    let a = Address::repeat_byte(0x0a);
    let b = Address::repeat_byte(0x0b);
    let c = Address::repeat_byte(0x0c);
    let v2_pair_ab = Address::repeat_byte(0x21);
    let v3_pool_ac = Address::repeat_byte(0x35);

    let v2_cfg = UniswapV2FactoryConfig::uniswap_v2(v2_factory).with_fee_bps(30);
    let v3_cfg = UniswapV3FactoryConfig::uniswap_v3(v3_factory); // 4 fee tiers

    let mut cache = CountingCache::default();
    cache.set(
        v2_factory,
        derive::v2_get_pair_slot(v2_cfg.get_pair_base_slot, a, b),
        word(v2_pair_ab),
    );
    cache.set(
        v3_factory,
        derive::v3_get_pool_slot(v3_cfg.get_pool_base_slot, a, c, 500),
        word(v3_pool_ac),
    );
    cache.set(
        v3_factory,
        derive::v3_fee_amount_tick_spacing_slot(v3_cfg.fee_amount_tick_spacing_base_slot, 500),
        U256::from(10),
    );

    let discovery = PoolDiscovery::for_registry(
        &registry_v2_v3(),
        FactoryConfig::default()
            .with_uniswap_v2(v2_cfg)
            .with_uniswap_v3(v3_cfg),
    );

    let found = discovery.find(&mut cache, PoolQuery::basket([a, b, c]))?;

    let addrs: std::collections::HashSet<Option<Address>> =
        found.iter().map(|p| p.key.address()).collect();
    assert!(addrs.contains(&Some(v2_pair_ab)) && addrs.contains(&Some(v3_pool_ac)));
    assert_eq!(cache.single_reads, 0);
    assert_eq!(cache.batch_reads, 1, "whole basket in one batched read");
    // 3 pairs V2 getPair + 4 V3 tickSpacing + 3 pairs * 4 fees V3 getPool = 19.
    assert_eq!(cache.slots_read, 19);
    Ok(())
}

/// `find` over two factories of the same protocol resolves both in one read.
#[test]
fn find_across_same_protocol_factories_one_read() -> Result<()> {
    let uni = Address::repeat_byte(0xf1);
    let sushi = Address::repeat_byte(0xf2);
    let a = Address::repeat_byte(0x0a);
    let b = Address::repeat_byte(0x0b);
    let uni_pair = Address::repeat_byte(0x11);
    let sushi_pair = Address::repeat_byte(0x22);

    let uni_cfg = UniswapV2FactoryConfig::uniswap_v2(uni).with_fee_bps(30);
    let sushi_cfg = UniswapV2FactoryConfig::uniswap_v2(sushi).with_fee_bps(30);
    let (t0, t1) = derive::sort_tokens(a, b);

    let mut cache = CountingCache::default();
    cache.set(
        uni,
        derive::v2_get_pair_slot(uni_cfg.get_pair_base_slot, t0, t1),
        word(uni_pair),
    );
    cache.set(
        sushi,
        derive::v2_get_pair_slot(sushi_cfg.get_pair_base_slot, t0, t1),
        word(sushi_pair),
    );

    let mut registry = AdapterRegistry::new();
    registry.register_adapter(Arc::new(UniswapV2Adapter::default()))?;
    let discovery = PoolDiscovery::for_registry(
        &registry,
        FactoryConfig::default()
            .with_uniswap_v2(uni_cfg)
            .with_uniswap_v2(sushi_cfg),
    );

    let found = discovery.find(&mut cache, PoolQuery::pair(a, b).on(ProtocolId::UniswapV2))?;
    let mut pairs: Vec<Address> = found.iter().filter_map(|p| p.key.address()).collect();
    pairs.sort();
    let mut want = vec![uni_pair, sushi_pair];
    want.sort();
    assert_eq!(pairs, want);
    assert_eq!(cache.single_reads, 0);
    assert_eq!(cache.batch_reads, 1);
    Ok(())
}

/// A discovered V3 registration carries full metadata (used by bytecode seeding).
#[test]
fn find_v3_registration_has_full_metadata() -> Result<()> {
    let v3_factory = Address::repeat_byte(0xf3);
    let a = Address::repeat_byte(0xcc);
    let b = Address::repeat_byte(0xaa);
    let pool = Address::repeat_byte(0x33);
    let v3_cfg = UniswapV3FactoryConfig::uniswap_v3(v3_factory).with_fee_tiers([500]);
    let (t0, t1) = derive::sort_tokens(a, b);

    let mut cache = CountingCache::default();
    cache.set(
        v3_factory,
        derive::v3_get_pool_slot(v3_cfg.get_pool_base_slot, t0, t1, 500),
        word(pool),
    );
    cache.set(
        v3_factory,
        derive::v3_fee_amount_tick_spacing_slot(v3_cfg.fee_amount_tick_spacing_base_slot, 500),
        U256::from(10),
    );

    let registry = registry_v2_v3();
    let discovery =
        PoolDiscovery::for_registry(&registry, FactoryConfig::default().with_uniswap_v3(v3_cfg));

    let found = discovery.find(&mut cache, PoolQuery::pair(a, b).on(ProtocolId::UniswapV3))?;
    assert_eq!(found.len(), 1);
    let ProtocolMetadata::UniswapV3(m) = &found[0].registration.metadata else {
        panic!("expected V3 metadata");
    };
    assert_eq!(m.token0, Some(t0));
    assert_eq!(m.token1, Some(t1));
    assert_eq!(m.fee, Some(500));
    assert_eq!(m.tick_spacing, Some(10));
    assert_eq!(m.factory, Some(v3_factory));

    // The discovered registration enables bytecode seeding.
    let adapter = registry
        .adapter(ProtocolId::UniswapV3)
        .ok_or_else(|| anyhow!("missing V3 adapter"))?;
    let seeds = adapter
        .code_seeds(&found[0].registration)
        .map_err(|e| anyhow!("{e}"))?;
    assert_eq!(seeds.len(), 1);
    assert_eq!(seeds[0].address, pool);
    Ok(())
}

/// A factory that only implements `find_pools` (no batched `candidate_reads`)
/// still participates through `find`, via the per-query fallback, and can be
/// attached through the open channel.
#[test]
fn external_find_pools_only_factory_participates() -> Result<()> {
    let custom_pool = Address::repeat_byte(0x99);
    let mut registry = AdapterRegistry::new();
    registry.register_adapter(Arc::new(UniswapV2Adapter::default()))?;

    let discovery = PoolDiscovery::for_registry(&registry, FactoryConfig::default()).with_factory(
        Box::new(StubFactory {
            factory: Address::repeat_byte(0xcf),
            pool: custom_pool,
        }),
    );

    let mut cache = CountingCache::default();
    let found = discovery.find(
        &mut cache,
        PoolQuery::pair(Address::repeat_byte(0x01), Address::repeat_byte(0x02)),
    )?;
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].key.address(), Some(custom_pool));
    Ok(())
}

/// An empty query is a no-op.
#[test]
fn find_empty_query_is_empty() -> Result<()> {
    let mut registry = AdapterRegistry::new();
    registry.register_adapter(Arc::new(UniswapV2Adapter::default()))?;
    let discovery = PoolDiscovery::for_registry(
        &registry,
        FactoryConfig::default().with_uniswap_v2(UniswapV2FactoryConfig::uniswap_v2(
            Address::repeat_byte(0xf2),
        )),
    );
    let mut cache = CountingCache::default();
    assert!(
        discovery
            .find(&mut cache, PoolQuery::basket([]))?
            .is_empty()
    );
    assert!(discovery.find(&mut cache, PoolQuery::pairs([]))?.is_empty());
    assert_eq!(cache.batch_reads, 0);
    Ok(())
}

/// Creation-log decoding still works (unchanged by the query reshape).
#[test]
fn creation_log_decodes() -> Result<()> {
    let factory = Address::repeat_byte(0xf2);
    let token0 = Address::repeat_byte(0x01);
    let token1 = Address::repeat_byte(0x02);
    let pair = Address::repeat_byte(0x55);
    let mut registry = AdapterRegistry::new();
    registry.register_adapter(Arc::new(UniswapV2Adapter::default()))?;
    let discovery = PoolDiscovery::for_registry(
        &registry,
        FactoryConfig::default()
            .with_uniswap_v2(UniswapV2FactoryConfig::uniswap_v2(factory).with_fee_bps(30)),
    );

    let mut pair_word = [0u8; 32];
    pair_word[12..].copy_from_slice(pair.as_slice());
    let mut len_word = [0u8; 32];
    len_word[31] = 12;
    let mut data = Vec::new();
    data.extend_from_slice(&pair_word);
    data.extend_from_slice(&len_word);

    let log = Log::new(
        factory,
        vec![
            keccak256("PairCreated(address,address,address,uint256)"),
            topic(token0),
            topic(token1),
        ],
        data.into(),
    )
    .expect("valid log");

    let found = discovery
        .decode_creation(&log, CreationLogContext::new(Some(456), Some(9)))?
        .expect("decoded");
    assert_eq!(found.key, PoolKey::UniswapV2(pair));
    assert_eq!(
        found.source,
        DiscoverySource::CreationEvent {
            block_number: Some(456),
            log_index: Some(9)
        }
    );
    Ok(())
}

/// `find_many` resolves several heterogeneous queries — different protocols for
/// different pairs — in ONE batched read, returning the union.
#[test]
fn find_many_resolves_heterogeneous_queries_in_one_read() -> Result<()> {
    let v2_factory = Address::repeat_byte(0xf2);
    let v3_factory = Address::repeat_byte(0xf3);
    let a = Address::repeat_byte(0x0a);
    let b = Address::repeat_byte(0x0b);
    let c = Address::repeat_byte(0x0c);
    let v2_pair_ab = Address::repeat_byte(0x21);
    let v3_pool_ac = Address::repeat_byte(0x35);

    let v2_cfg = UniswapV2FactoryConfig::uniswap_v2(v2_factory).with_fee_bps(30);
    let v3_cfg = UniswapV3FactoryConfig::uniswap_v3(v3_factory);

    let mut cache = CountingCache::default();
    cache.set(
        v2_factory,
        derive::v2_get_pair_slot(v2_cfg.get_pair_base_slot, a, b),
        word(v2_pair_ab),
    );
    cache.set(
        v3_factory,
        derive::v3_get_pool_slot(v3_cfg.get_pool_base_slot, a, c, 500),
        word(v3_pool_ac),
    );
    cache.set(
        v3_factory,
        derive::v3_fee_amount_tick_spacing_slot(v3_cfg.fee_amount_tick_spacing_base_slot, 500),
        U256::from(10),
    );

    let discovery = PoolDiscovery::for_registry(
        &registry_v2_v3(),
        FactoryConfig::default()
            .with_uniswap_v2(v2_cfg)
            .with_uniswap_v3(v3_cfg),
    );

    // (a,b) only on V2; (a,c) only on V3 — one shot.
    let found = discovery.find_many(
        &mut cache,
        [
            PoolQuery::pairs([(a, b)]).on(ProtocolId::UniswapV2),
            PoolQuery::pairs([(a, c)]).on(ProtocolId::UniswapV3),
        ],
    )?;

    let addrs: std::collections::HashSet<Option<Address>> =
        found.iter().map(|p| p.key.address()).collect();
    assert!(addrs.contains(&Some(v2_pair_ab)) && addrs.contains(&Some(v3_pool_ac)));
    assert_eq!(
        found.len(),
        2,
        "exactly the two requested pools; got {found:?}"
    );
    assert_eq!(cache.single_reads, 0);
    assert_eq!(
        cache.batch_reads, 1,
        "all sub-queries resolve in one batched read"
    );
    Ok(())
}

/// Overlapping sub-queries do not return the same pool twice, and an empty
/// query list is a no-op.
#[test]
fn find_many_dedups_and_empty_is_noop() -> Result<()> {
    let v2_factory = Address::repeat_byte(0xf2);
    let a = Address::repeat_byte(0x0a);
    let b = Address::repeat_byte(0x0b);
    let v2_pair = Address::repeat_byte(0x21);
    let v2_cfg = UniswapV2FactoryConfig::uniswap_v2(v2_factory).with_fee_bps(30);

    let mut cache = CountingCache::default();
    cache.set(
        v2_factory,
        derive::v2_get_pair_slot(v2_cfg.get_pair_base_slot, a, b),
        word(v2_pair),
    );

    let mut registry = AdapterRegistry::new();
    registry.register_adapter(Arc::new(UniswapV2Adapter::default()))?;
    let discovery =
        PoolDiscovery::for_registry(&registry, FactoryConfig::default().with_uniswap_v2(v2_cfg));

    // The same pair reached via an unfiltered query and a V2-filtered query.
    let found = discovery.find_many(
        &mut cache,
        [
            PoolQuery::pair(a, b),
            PoolQuery::pair(a, b).on(ProtocolId::UniswapV2),
        ],
    )?;
    assert_eq!(
        found
            .iter()
            .filter(|p| p.key == PoolKey::UniswapV2(v2_pair))
            .count(),
        1,
        "overlapping sub-queries must not duplicate a pool"
    );

    let empty: Vec<PoolQuery> = Vec::new();
    assert!(discovery.find_many(&mut cache, empty)?.is_empty());
    Ok(())
}

fn topic(address: Address) -> B256 {
    let mut bytes = [0u8; 32];
    bytes[12..].copy_from_slice(address.as_slice());
    B256::from(bytes)
}

/// A minimal external `PoolFactory` — implements only `find_pools` on a pair.
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
        _pair: (Address, Address),
    ) -> Result<Vec<DiscoveredPool>, DiscoveryError> {
        let registration = PoolRegistration::new(PoolKey::UniswapV2(self.pool))
            .with_state_address(self.pool)
            .with_metadata(ProtocolMetadata::UniswapV2(UniswapV2Metadata::default()));
        Ok(vec![DiscoveredPool::new(
            registration.key.clone(),
            registration,
            DiscoverySource::Query,
        )])
    }
    fn creation_sources(&self) -> Vec<EventSource> {
        Vec::new()
    }
    fn decode_creation(
        &self,
        _log: &Log,
        _context: CreationLogContext,
    ) -> Result<Option<DiscoveredPool>, DiscoveryError> {
        Ok(None)
    }
}
