//! Combined discovery-wiring test: proves `PoolDiscovery::for_registry` fans a
//! `FactoryConfig` out to EVERY protocol's factory driver through each adapter's
//! `pool_factories` hook. The per-protocol driver mechanics are already covered
//! by `tests/discovery_cl.rs` / `discovery_solidly.rs`; this test only asserts
//! the *wiring* — that a scoped `find(PoolQuery::pair(..).on(P))` reaches a
//! registered factory for `P` and therefore does NOT return
//! `DiscoveryError::MissingFactory(P)`.
//!
//! Over an empty cache every DerivedSlot factory (V2 `getPair`, V3/Pancake/
//! Slipstream `getPool`, Solidly `getPool[t0][t1][stable]`) reads zero words and
//! resolves to `Ok(vec![])`. An empty result is success — it means the factory
//! was wired and consulted. A `MissingFactory(P)` would mean the adapter's
//! `pool_factories` never emitted a driver for `P`.

use alloy_primitives::{Address, Bytes, U256, address};
use anyhow::{Result, anyhow};
use evm_amm_state::adapters::{
    AdapterCache, AdapterRegistry, CacheError, CallOutcome, ConcentratedLiquidityAdapter,
    DiscoveryError, FactoryConfig, PoolDiscovery, PoolQuery, ProtocolId, SlotChange,
    SolidlyV2Adapter, StateDiff, StateUpdate, StateView, UniswapV2Adapter,
};

// Synthetic addresses — this test never touches a real chain or real constants.
const UNISWAP_V2_FACTORY: Address = address!("00000000000000000000000000000000000000f2");
const UNISWAP_V3_FACTORY: Address = address!("00000000000000000000000000000000000000f3");
const PANCAKE_V3_FACTORY: Address = address!("00000000000000000000000000000000000000f4");
const SLIPSTREAM_FACTORY: Address = address!("00000000000000000000000000000000000000f5");
const SOLIDLY_FACTORY: Address = address!("00000000000000000000000000000000000000f7");

const TOKEN_A: Address = address!("000000000000000000000000000000000000000a");
const TOKEN_B: Address = address!("000000000000000000000000000000000000000b");

/// A combined cache that satisfies every DerivedSlot factory with "nothing
/// found": storage reads are all zero, so each factory resolves to `Ok(vec![])`.
#[derive(Default)]
struct CombinedMockCache;

impl StateView for CombinedMockCache {
    fn storage(&self, _address: Address, _slot: U256) -> Option<U256> {
        None
    }
}

impl AdapterCache for CombinedMockCache {
    fn cached_storage(&self, _address: Address, _slot: U256) -> Option<U256> {
        None
    }
    fn apply_updates(&mut self, _updates: &[StateUpdate]) -> StateDiff {
        StateDiff::default()
    }
    fn verify_slots(&mut self, _slots: &[(Address, U256)]) -> Result<Vec<SlotChange>, CacheError> {
        Ok(Vec::new())
    }
    fn purge_storage(&mut self, _address: Address) -> StateDiff {
        StateDiff::default()
    }
    fn purge_slots(&mut self, _address: Address, _slots: &[U256]) -> StateDiff {
        StateDiff::default()
    }
    fn read_storage_slot(&mut self, _address: Address, _slot: U256) -> Result<U256, CacheError> {
        Ok(U256::ZERO)
    }
    fn read_storage_slots(&mut self, slots: &[(Address, U256)]) -> Result<Vec<U256>, CacheError> {
        // Every DerivedSlot factory sees zeros => no pools, but consulted.
        Ok(vec![U256::ZERO; slots.len()])
    }
    fn call_raw(
        &mut self,
        _from: Address,
        _to: Address,
        _calldata: Bytes,
        _commit: bool,
    ) -> Result<CallOutcome, CacheError> {
        // Every discovery factory under test is DerivedSlot (no ViewCall), so no
        // call is expected. Keep the mock strict: an unexpected call reverts
        // rather than silently succeeding.
        Ok(CallOutcome::Revert {
            output: Bytes::new(),
            gas_used: 0,
        })
    }
}

/// A registry with every finished discovery-capable adapter registered.
fn registry_with_all_adapters() -> Result<AdapterRegistry> {
    let mut registry = AdapterRegistry::new();
    registry.register_adapter(std::sync::Arc::new(UniswapV2Adapter::default()))?;
    registry.register_adapter(std::sync::Arc::new(ConcentratedLiquidityAdapter::default()))?;
    registry.register_adapter(std::sync::Arc::new(SolidlyV2Adapter::default()))?;
    Ok(registry)
}

/// A `FactoryConfig` naming one factory for every protocol under test, through
/// the per-protocol builder conveniences.
fn combined_config() -> FactoryConfig {
    FactoryConfig::default()
        .with_uniswap_v2_factory(UNISWAP_V2_FACTORY)
        .with_uniswap_v3_factory(UNISWAP_V3_FACTORY)
        .with_pancake_v3_factory(PANCAKE_V3_FACTORY)
        .with_slipstream_factory(SLIPSTREAM_FACTORY)
        .with_solidly_factory(SOLIDLY_FACTORY)
}

/// The full protocol set this slice's discovery serves. `for_registry` must emit
/// at least one factory for each, so a `.on(P)` query never hits `MissingFactory`.
const WIRED_PROTOCOLS: [ProtocolId; 5] = [
    ProtocolId::UniswapV2,
    ProtocolId::UniswapV3,
    ProtocolId::PancakeV3,
    ProtocolId::Slipstream,
    ProtocolId::SolidlyV2,
];

/// `PoolDiscovery::for_registry` wires a factory for EVERY protocol in the
/// config: a query scoped to each protocol resolves (to an empty set over the
/// empty cache) instead of erroring `MissingFactory`.
#[test]
fn for_registry_wires_every_protocol_factory() -> Result<()> {
    let registry = registry_with_all_adapters()?;
    let discovery = PoolDiscovery::for_registry(&registry, combined_config());
    let mut cache = CombinedMockCache;

    for protocol in WIRED_PROTOCOLS {
        let result = discovery.find(&mut cache, PoolQuery::pair(TOKEN_A, TOKEN_B).on(protocol));
        match result {
            // Empty is the expected "wired, nothing found" outcome over the
            // empty cache — the point is only that the factory was consulted.
            Ok(found) => assert!(
                found.is_empty(),
                "{protocol:?}: empty cache should discover no pools, got {}",
                found.len()
            ),
            Err(DiscoveryError::MissingFactory(missing)) => {
                return Err(anyhow!(
                    "for_registry did not wire a factory for {protocol:?} (MissingFactory({missing:?}))"
                ));
            }
            Err(other) => {
                return Err(anyhow!("{protocol:?}: unexpected discovery error: {other}"));
            }
        }
    }
    Ok(())
}

/// Positive end-to-end wiring check across the whole set at once: an UNSCOPED
/// query fans out to all wired factories in one batched pass, never errors on a
/// missing factory, and (over the empty cache) resolves to an empty union. This
/// exercises the same `for_registry` fan-out the loop above checks per protocol,
/// but through the batched all-factory path.
#[test]
fn unscoped_query_fans_out_to_all_factories() -> Result<()> {
    let registry = registry_with_all_adapters()?;
    let discovery = PoolDiscovery::for_registry(&registry, combined_config());
    let mut cache = CombinedMockCache;

    let found = discovery.find(&mut cache, PoolQuery::pair(TOKEN_A, TOKEN_B))?;
    assert!(
        found.is_empty(),
        "empty cache yields no pools across any factory, got {}",
        found.len()
    );
    Ok(())
}

/// Guards the wiring premise from the other side: a protocol whose factory is
/// NOT in the config must still error `MissingFactory` when scoped. Balancer V2
/// has no discovery factory in this slice, so `.on(BalancerV2)` proves the
/// discovery front-end really is factory-gated (i.e. the passing cases above are
/// not vacuous). Uses a bare registry so this test holds regardless of which
/// adapter features are enabled.
#[test]
fn scoped_query_without_factory_still_errors() -> Result<()> {
    let discovery = PoolDiscovery::new(std::iter::empty());
    let mut cache = CombinedMockCache;

    let err = discovery
        .find(
            &mut cache,
            PoolQuery::pair(TOKEN_A, TOKEN_B).on(ProtocolId::BalancerV2),
        )
        .expect_err("no BalancerV2 factory => MissingFactory");
    match err {
        DiscoveryError::MissingFactory(ProtocolId::BalancerV2) => Ok(()),
        other => Err(anyhow!("expected MissingFactory(BalancerV2), got {other}")),
    }
}
