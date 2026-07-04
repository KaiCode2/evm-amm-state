//! MANAGER-AUTHORED acceptance test for Cycle 3: factory discovery resolves its
//! candidate slots in ONE batched read, not one blocking read per fee tier. The
//! implementation agent must make this pass WITHOUT modifying it.
//!
//! The property under test: discovery round-trips are O(1) in the number of fee
//! tiers, not O(N). The cache counts batched (`read_storage_slots`) vs single
//! (`read_storage_slot`) reads; a batched V3 query over four fee tiers must
//! issue exactly one batched read and zero single reads.

use std::collections::HashMap;

use alloy_primitives::{Address, Bytes, U256};
use anyhow::Result;
use evm_amm_state::adapters::factory::derive;
use evm_amm_state::adapters::{
    AdapterCache, AdapterRegistry, CacheError, CallOutcome, ConcentratedLiquidityAdapter,
    FactoryConfig, PoolDiscovery, SlotChange, StateDiff, StateUpdate, StateView,
    UniswapV3FactoryConfig, UniswapV3PoolQuery,
};

#[derive(Default)]
struct CountingCache {
    storage: HashMap<(Address, U256), U256>,
    single_reads: usize,
    batch_reads: usize,
}

impl CountingCache {
    fn with_storage(mut self, address: Address, slot: U256, value: U256) -> Self {
        self.storage.insert((address, slot), value);
        self
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
    fn read_storage_slot(&mut self, address: Address, slot: U256) -> Result<U256, CacheError> {
        self.single_reads += 1;
        Ok(self.storage(address, slot).unwrap_or_default())
    }
    // Override the default (loop) batch read so the test can observe batching.
    fn read_storage_slots(
        &mut self,
        slots: &[(Address, U256)],
    ) -> Result<Vec<U256>, CacheError> {
        self.batch_reads += 1;
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

fn address_word(address: Address) -> U256 {
    U256::from_be_slice(address.as_slice())
}

#[test]
fn v3_discovery_batches_all_fee_tier_reads_into_one_call() -> Result<()> {
    let factory = Address::repeat_byte(0xf3);
    let token_a = Address::repeat_byte(0xaa);
    let token_b = Address::repeat_byte(0xcc);
    let pool = Address::repeat_byte(0x33);
    let hit_fee = 500u32;
    let hit_tick_spacing = 10i32;
    let fees = [100u32, 500, 3_000, 10_000];

    let query = UniswapV3PoolQuery::pair(token_a, token_b);
    let (token0, token1) = query.tokens;
    let config = UniswapV3FactoryConfig::uniswap_v3(factory).with_fee_tiers(fees);

    // Only the 0.05% tier has a pool; the other three tiers resolve to zero.
    let pool_slot = derive::v3_get_pool_slot(config.get_pool_base_slot, token0, token1, hit_fee);
    let tick_slot = derive::v3_fee_amount_tick_spacing_slot(
        config.fee_amount_tick_spacing_base_slot,
        hit_fee,
    );
    let mut cache = CountingCache::default()
        .with_storage(factory, pool_slot, address_word(pool))
        .with_storage(factory, tick_slot, U256::from(hit_tick_spacing));

    let mut registry = AdapterRegistry::new();
    registry.register_adapter(std::sync::Arc::new(ConcentratedLiquidityAdapter::default()))?;
    let discovery =
        PoolDiscovery::for_registry(&registry, FactoryConfig::default().with_uniswap_v3(config));

    let found = discovery.find_uniswap_v3(&mut cache, query)?;

    assert_eq!(found.len(), 1, "only the 0.05% tier has a pool");
    assert_eq!(found[0].key.address(), Some(pool));
    assert_eq!(
        cache.single_reads, 0,
        "discovery must not issue any one-at-a-time reads; got {}",
        cache.single_reads
    );
    assert_eq!(
        cache.batch_reads, 1,
        "all fee-tier getPool + feeAmountTickSpacing slots must resolve in ONE batched read; got {} batched reads",
        cache.batch_reads
    );
    Ok(())
}
