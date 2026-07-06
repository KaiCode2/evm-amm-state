//! MANAGER-AUTHORED acceptance tests for the generalized concentrated-liquidity
//! discovery factory (`ClFactorySpec` + `ConcentratedLiquidityFactory`). The
//! implementation agent must make these pass WITHOUT modifying them.
//!
//! These pin DRIVER MECHANICS with synthetic configs — they do not depend on any
//! real on-chain factory address, init-code hash, or base slot. Real-constant
//! verification lives in the gated (`#[ignore]`, RPC) parity tests the agent adds.
//!
//! Scope reminder: fee-keyed (Uniswap/Sushi/Pancake) and tickSpacing-keyed
//! (Slipstream) only. Algebra / pair-only keying / dynamic fees are OUT.

use std::collections::HashMap;

use alloy_primitives::{Address, B256, Bytes, U256, address};
use anyhow::{Result, anyhow};
use evm_amm_state::adapters::factory::derive;
use evm_amm_state::adapters::{
    AdapterCache, CacheError, CallOutcome, ClFactorySpec, ConcentratedLiquidityFactory,
    DiscoveryError, PoolDiscovery, PoolFactory, PoolKey, PoolQuery, ProtocolId, ProtocolMetadata,
    SlotChange, StateDiff, StateUpdate, StateView,
};

// --- minimal counting cache (mirrors tests/discovery.rs) ---

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

fn spacing_word(spacing: i32) -> U256 {
    // int24 tickSpacing packed into a feeAmountTickSpacing storage word.
    U256::from(spacing as u64)
}

fn cl_metadata(
    pool: &evm_amm_state::adapters::DiscoveredPool,
) -> &evm_amm_state::adapters::V3Metadata {
    match &pool.registration.metadata {
        ProtocolMetadata::UniswapV3(m)
        | ProtocolMetadata::PancakeV3(m)
        | ProtocolMetadata::Slipstream(m) => m,
        other => panic!("expected V3-family metadata, got {other:?}"),
    }
}

// Synthetic constants — NOT real on-chain values.
const FEE_FACTORY: Address = address!("00000000000000000000000000000000000000f3");
const SPACING_FACTORY: Address = address!("00000000000000000000000000000000000000f5");
const QUOTER: Address = address!("00000000000000000000000000000000000000cc");
const TOKEN_A: Address = address!("000000000000000000000000000000000000000a");
const TOKEN_B: Address = address!("000000000000000000000000000000000000000b");
const POOL: Address = address!("00000000000000000000000000000000000000d0");

const GET_POOL_BASE: U256 = U256::from_limbs([5, 0, 0, 0]);
const FEE_TS_BASE: U256 = U256::from_limbs([4, 0, 0, 0]);

fn one_factory(spec: ClFactorySpec) -> PoolDiscovery {
    PoolDiscovery::new([Box::new(ConcentratedLiquidityFactory::new(spec)) as Box<dyn PoolFactory>])
}

/// Fee-keyed spec (Pancake shape) resolves a seeded `getPool` mapping, keys the
/// pool as PancakeV3, reads tickSpacing from `feeAmountTickSpacing`, and carries
/// the per-fork quoter — all in ONE batched read.
#[test]
fn fee_keyed_resolves_and_keys_by_protocol() -> Result<()> {
    let (t0, t1) = derive::sort_tokens(TOKEN_A, TOKEN_B);
    let fee = 2500u32;
    let spacing = 50i32;

    let spec = ClFactorySpec::fee_keyed(
        ProtocolId::PancakeV3,
        FEE_FACTORY,
        GET_POOL_BASE,
        FEE_TS_BASE,
        [fee],
    )
    .with_quoter(QUOTER);

    let mut cache = CountingCache::default();
    cache.set(
        FEE_FACTORY,
        derive::v3_get_pool_slot(GET_POOL_BASE, t0, t1, fee),
        word(POOL),
    );
    cache.set(
        FEE_FACTORY,
        derive::v3_fee_amount_tick_spacing_slot(FEE_TS_BASE, fee),
        spacing_word(spacing),
    );

    let discovery = one_factory(spec);
    let found = discovery.find(
        &mut cache,
        PoolQuery::pair(TOKEN_A, TOKEN_B).on(ProtocolId::PancakeV3),
    )?;

    assert_eq!(found.len(), 1, "exactly one pool");
    assert_eq!(
        found[0].key,
        PoolKey::PancakeV3(POOL),
        "keyed by spec.protocol"
    );
    let md = cl_metadata(&found[0]);
    assert_eq!(md.fee, Some(fee));
    assert_eq!(md.tick_spacing, Some(spacing));
    assert_eq!(
        md.quoter,
        Some(QUOTER),
        "per-fork quoter flows into metadata"
    );
    assert_eq!(cache.batch_reads, 1, "resolved in one batched read");
    assert_eq!(cache.single_reads, 0, "no per-slot reads");
    Ok(())
}

/// TickSpacing-keyed spec (Slipstream shape): the pool key IS the tickSpacing, so
/// discovery resolves with ONLY the getPool slot seeded — no feeAmountTickSpacing
/// read exists — and keys the pool as Slipstream with tick_spacing == the key.
#[test]
fn tick_spacing_keyed_resolves_without_fee_mapping() -> Result<()> {
    let (t0, t1) = derive::sort_tokens(TOKEN_A, TOKEN_B);
    let spacing = 100i32;

    let spec = ClFactorySpec::tick_spacing_keyed(
        ProtocolId::Slipstream,
        SPACING_FACTORY,
        GET_POOL_BASE,
        [spacing],
    )
    .with_quoter(QUOTER);

    let mut cache = CountingCache::default();
    // Only the spacing-keyed getPool slot is seeded — nothing else.
    cache.set(
        SPACING_FACTORY,
        derive::v3_get_pool_slot_by_spacing(GET_POOL_BASE, t0, t1, spacing),
        word(POOL),
    );

    let discovery = one_factory(spec);
    let found = discovery.find(
        &mut cache,
        PoolQuery::pair(TOKEN_A, TOKEN_B).on(ProtocolId::Slipstream),
    )?;

    assert_eq!(found.len(), 1, "resolves from the spacing key alone");
    assert_eq!(found[0].key, PoolKey::Slipstream(POOL));
    assert_eq!(
        cl_metadata(&found[0]).tick_spacing,
        Some(spacing),
        "tickSpacing is the key"
    );
    assert_eq!(cache.batch_reads, 1);
    Ok(())
}

/// A fee-keyed factory with several tiers resolves every tier in ONE batched read.
#[test]
fn fee_keyed_all_tiers_one_batched_read() -> Result<()> {
    let (t0, t1) = derive::sort_tokens(TOKEN_A, TOKEN_B);
    let tiers = [100u32, 500, 2500, 10_000];

    let spec = ClFactorySpec::fee_keyed(
        ProtocolId::PancakeV3,
        FEE_FACTORY,
        GET_POOL_BASE,
        FEE_TS_BASE,
        tiers,
    );

    let mut cache = CountingCache::default();
    // Only the 500 tier has a pool.
    cache.set(
        FEE_FACTORY,
        derive::v3_get_pool_slot(GET_POOL_BASE, t0, t1, 500),
        word(POOL),
    );
    cache.set(
        FEE_FACTORY,
        derive::v3_fee_amount_tick_spacing_slot(FEE_TS_BASE, 500),
        spacing_word(10),
    );

    let discovery = one_factory(spec);
    let found = discovery.find(
        &mut cache,
        PoolQuery::pair(TOKEN_A, TOKEN_B).on(ProtocolId::PancakeV3),
    )?;

    assert_eq!(found.len(), 1, "only the populated tier resolves");
    assert_eq!(found[0].key, PoolKey::PancakeV3(POOL));
    assert_eq!(cache.batch_reads, 1, "all tiers in one batched read");
    Ok(())
}

/// With `verify_derivations` on and a CREATE2 config, a mapping answer that
/// disagrees with the CREATE2 derivation is a hard `DerivationMismatch` — the
/// guardrail against a wrong init-code hash / base-slot config.
#[test]
fn derivation_mismatch_is_reported() -> Result<()> {
    let (t0, t1) = derive::sort_tokens(TOKEN_A, TOKEN_B);
    let fee = 500u32;
    // A deliberately wrong init hash so the CREATE2 derivation cannot match POOL.
    let wrong_hash = B256::repeat_byte(0xab);

    let spec = ClFactorySpec::fee_keyed(
        ProtocolId::PancakeV3,
        FEE_FACTORY,
        GET_POOL_BASE,
        FEE_TS_BASE,
        [fee],
    )
    .with_create2(None, wrong_hash)
    .with_verify_derivations(true);

    let mut cache = CountingCache::default();
    cache.set(
        FEE_FACTORY,
        derive::v3_get_pool_slot(GET_POOL_BASE, t0, t1, fee),
        word(POOL),
    );
    cache.set(
        FEE_FACTORY,
        derive::v3_fee_amount_tick_spacing_slot(FEE_TS_BASE, fee),
        spacing_word(10),
    );

    let discovery = one_factory(spec);
    let err = discovery
        .find(
            &mut cache,
            PoolQuery::pair(TOKEN_A, TOKEN_B).on(ProtocolId::PancakeV3),
        )
        .expect_err("wrong init hash must fail loudly");
    match err {
        DiscoveryError::DerivationMismatch { mapping, .. } => {
            assert_eq!(mapping, POOL, "mapping answer is the seeded pool");
        }
        other => return Err(anyhow!("expected DerivationMismatch, got {other:?}")),
    }
    Ok(())
}

/// A zero mapping answer means "no pool" — an empty result, not an error.
#[test]
fn zero_mapping_answer_is_empty_not_error() -> Result<()> {
    let spec = ClFactorySpec::fee_keyed(
        ProtocolId::PancakeV3,
        FEE_FACTORY,
        GET_POOL_BASE,
        FEE_TS_BASE,
        [500],
    );
    let mut cache = CountingCache::default(); // nothing seeded => all zero
    let discovery = one_factory(spec);
    let found = discovery.find(
        &mut cache,
        PoolQuery::pair(TOKEN_A, TOKEN_B).on(ProtocolId::PancakeV3),
    )?;
    assert!(found.is_empty(), "no seeded pool => empty, not error");
    Ok(())
}
