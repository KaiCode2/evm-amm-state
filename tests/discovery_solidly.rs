//! MANAGER-AUTHORED acceptance tests for Solidly V2 (Aerodrome/Velodrome)
//! factory discovery via a DerivedSlot read of `getPool[t0][t1][bool stable]`.
//! The implementation agent must make these pass WITHOUT modifying them.
//!
//! Synthetic configs + seeded slots — no dependence on real on-chain constants
//! (those are pinned by the gated `tests/discovery_solidly_rpc.rs`).

use std::collections::HashMap;

use alloy_primitives::{Address, Bytes, U256, address};
use anyhow::{Result, anyhow};
use evm_amm_state::adapters::factory::derive;
use evm_amm_state::adapters::{
    AdapterCache, CacheError, CallOutcome, DiscoveryError, PoolDiscovery, PoolFactory, PoolKey,
    PoolQuery, ProtocolId, ProtocolMetadata, SlotChange, SolidlyFactory, SolidlyFactoryConfig,
    SolidlyStorageLayout, StateDiff, StateUpdate, StateView,
};

#[derive(Default)]
struct CountingCache {
    storage: HashMap<(Address, U256), U256>,
    single_reads: usize,
    batch_reads: usize,
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

fn solidly_metadata(
    pool: &evm_amm_state::adapters::DiscoveredPool,
) -> &evm_amm_state::adapters::SolidlyV2Metadata {
    match &pool.registration.metadata {
        ProtocolMetadata::SolidlyV2(m) => m,
        other => panic!("expected SolidlyV2 metadata, got {other:?}"),
    }
}

const FACTORY: Address = address!("00000000000000000000000000000000000000f7");
const TOKEN_A: Address = address!("000000000000000000000000000000000000000a");
const TOKEN_B: Address = address!("000000000000000000000000000000000000000b");
const POOL_STABLE: Address = address!("00000000000000000000000000000000000000d1");
const POOL_VOLATILE: Address = address!("00000000000000000000000000000000000000d2");
const GET_POOL_BASE: U256 = U256::from_limbs([3, 0, 0, 0]);

fn layout() -> SolidlyStorageLayout {
    SolidlyStorageLayout::new(U256::from(1), U256::from(2), U256::from(3), U256::from(4))
}

fn discovery() -> PoolDiscovery {
    let cfg = SolidlyFactoryConfig::new(FACTORY, GET_POOL_BASE, layout());
    PoolDiscovery::new([Box::new(SolidlyFactory::new(cfg)) as Box<dyn PoolFactory>])
}

/// A pair yields BOTH its stable and volatile pools, keyed as SolidlyV2 with the
/// correct `stable` flag, in ONE batched read.
#[test]
fn resolves_stable_and_volatile() -> Result<()> {
    let (t0, t1) = derive::sort_tokens(TOKEN_A, TOKEN_B);
    let mut cache = CountingCache::default();
    cache.set(
        FACTORY,
        derive::solidly_get_pool_slot(GET_POOL_BASE, t0, t1, true),
        word(POOL_STABLE),
    );
    cache.set(
        FACTORY,
        derive::solidly_get_pool_slot(GET_POOL_BASE, t0, t1, false),
        word(POOL_VOLATILE),
    );

    let found = discovery().find(
        &mut cache,
        PoolQuery::pair(TOKEN_A, TOKEN_B).on(ProtocolId::SolidlyV2),
    )?;

    assert_eq!(found.len(), 2, "stable + volatile");
    let stable = found
        .iter()
        .find(|p| solidly_metadata(p).stable == Some(true))
        .ok_or_else(|| anyhow!("no stable pool"))?;
    let volatile = found
        .iter()
        .find(|p| solidly_metadata(p).stable == Some(false))
        .ok_or_else(|| anyhow!("no volatile pool"))?;
    assert_eq!(stable.key, PoolKey::SolidlyV2(POOL_STABLE));
    assert_eq!(volatile.key, PoolKey::SolidlyV2(POOL_VOLATILE));
    assert_eq!(cache.batch_reads, 1, "both variants in one batched read");
    assert_eq!(cache.single_reads, 0);
    Ok(())
}

/// A discovered pool carries the fork's storage layout (needed for cold-start).
#[test]
fn metadata_carries_layout() -> Result<()> {
    let (t0, t1) = derive::sort_tokens(TOKEN_A, TOKEN_B);
    let mut cache = CountingCache::default();
    cache.set(
        FACTORY,
        derive::solidly_get_pool_slot(GET_POOL_BASE, t0, t1, false),
        word(POOL_VOLATILE),
    );

    let found = discovery().find(
        &mut cache,
        PoolQuery::pair(TOKEN_A, TOKEN_B).on(ProtocolId::SolidlyV2),
    )?;
    assert_eq!(found.len(), 1);
    let md = solidly_metadata(&found[0]);
    assert_eq!(md.stable, Some(false));
    assert_eq!(
        md.storage_layout,
        Some(layout()),
        "fork storage layout attached"
    );
    Ok(())
}

/// No pool at either variant => empty result, not an error.
#[test]
fn zero_is_empty() -> Result<()> {
    let mut cache = CountingCache::default();
    let found = discovery().find(
        &mut cache,
        PoolQuery::pair(TOKEN_A, TOKEN_B).on(ProtocolId::SolidlyV2),
    )?;
    assert!(found.is_empty());
    Ok(())
}

/// A CREATE2 config + verify on turns a wrong init hash into DerivationMismatch.
#[test]
fn derivation_mismatch_is_reported() -> Result<()> {
    let (t0, t1) = derive::sort_tokens(TOKEN_A, TOKEN_B);
    let cfg = SolidlyFactoryConfig::new(FACTORY, GET_POOL_BASE, layout())
        .with_create2(None, alloy_primitives::B256::repeat_byte(0xcd))
        .with_verify_derivations(true);
    let mut cache = CountingCache::default();
    cache.set(
        FACTORY,
        derive::solidly_get_pool_slot(GET_POOL_BASE, t0, t1, false),
        word(POOL_VOLATILE),
    );

    let err = PoolDiscovery::new([Box::new(SolidlyFactory::new(cfg)) as Box<dyn PoolFactory>])
        .find(
            &mut cache,
            PoolQuery::pair(TOKEN_A, TOKEN_B).on(ProtocolId::SolidlyV2),
        )
        .expect_err("wrong init hash must fail");
    match err {
        DiscoveryError::DerivationMismatch { mapping, .. } => assert_eq!(mapping, POOL_VOLATILE),
        other => return Err(anyhow!("expected DerivationMismatch, got {other:?}")),
    }
    Ok(())
}
