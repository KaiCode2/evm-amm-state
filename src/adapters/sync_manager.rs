//! AMM-owned live sync driver around the generic reactive runtime.
//!
//! `evm-fork-cache` owns the expensive part of resync execution: grouping
//! [`ResyncRequest`](evm_fork_cache::reactive::ResyncRequest)s by block,
//! resolving them from block traces when possible, falling back to storage
//! fetchers, and applying authoritative values to
//! [`EvmCache`]. This module keeps the AMM-specific part local to this crate:
//! callers get a runtime that always executes resyncs, and failed storage repairs
//! are translated back into pool lifecycle status.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use alloy_network::Ethereum;
use alloy_primitives::{Address, U256};
use evm_fork_cache::cache::EvmCache;
use evm_fork_cache::reactive::{
    ReactiveBatchReport, ReactiveConfig, ReactiveError, ReactiveInputBatch, ReactiveReport,
    ReactiveRuntime, RegisterError, ResyncFailure, ResyncTarget,
};

use super::{
    AdapterCache, AdapterRegistry, AmmReactiveHandler, PoolKey, PoolRegistration, PoolStatus,
    ProtocolId, ProtocolMetadata, RegistryError,
};

/// Error constructing or running [`AmmSyncEngine`].
///
/// `#[non_exhaustive]`: variants track the upstream runtime's failure modes
/// and may grow — match with a wildcard arm.
#[derive(Debug)]
#[non_exhaustive]
pub enum AmmSyncError {
    /// The AMM reactive handler could not be registered.
    Register(RegisterError),
    /// The underlying reactive ingest failed.
    Reactive(ReactiveError),
    /// A registry mutation (mid-lifecycle pool registration) failed.
    Registry(RegistryError),
}

impl fmt::Display for AmmSyncError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Register(err) => write!(f, "failed to register AMM sync handler: {err}"),
            Self::Reactive(err) => write!(f, "AMM reactive ingest failed: {err}"),
            Self::Registry(err) => write!(f, "AMM registry mutation failed: {err}"),
        }
    }
}

impl std::error::Error for AmmSyncError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Register(err) => Some(err),
            Self::Reactive(err) => Some(err),
            Self::Registry(err) => Some(err),
        }
    }
}

impl From<RegistryError> for AmmSyncError {
    fn from(err: RegistryError) -> Self {
        Self::Registry(err)
    }
}

impl From<RegisterError> for AmmSyncError {
    fn from(err: RegisterError) -> Self {
        Self::Register(err)
    }
}

impl From<ReactiveError> for AmmSyncError {
    fn from(err: ReactiveError) -> Self {
        Self::Reactive(err)
    }
}

/// Summary returned by [`AmmSyncEngine::ingest_batch`].
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct AmmSyncBatchReport {
    /// Full upstream reactive report, including applied effects and resync
    /// details.
    pub reactive: ReactiveBatchReport<Ethereum>,
    /// Pools marked degraded because at least one of their requested authoritative
    /// resync targets failed.
    pub degraded_pools: Vec<PoolKey>,
    /// Previously-degraded pools flipped back to [`PoolStatus::Ready`] this batch
    /// because the resync phase refreshed the state they were waiting on and none
    /// of their targets failed again. Recovery is target-aware: a pool degraded by
    /// a tracked resync failure needs the specific slots it failed on refreshed
    /// (not merely any write at its address), while a pool degraded otherwise
    /// falls back to read-set coverage. A pool that covers no slots and has no
    /// tracked target (e.g. a failed cold-start with an empty read-set) can never
    /// be vouched for and stays degraded until cold-start is re-run.
    pub recovered_pools: Vec<PoolKey>,
    /// Number of authoritative state updates produced by executed resync reports.
    pub resync_state_updates: usize,
    /// Number of failed resync targets reported by the runtime.
    pub resync_failures: usize,
}

/// Live AMM sync driver.
///
/// This is a convenience owner for the intended live path:
///
/// 1. route AMM logs through [`AmmReactiveHandler`],
/// 2. execute every emitted resync via
///    [`ReactiveRuntime::ingest_batch_with_resync`],
/// 3. keep AMM pool status in sync with resync failures.
pub struct AmmSyncEngine {
    registry: AdapterRegistry,
    runtime: ReactiveRuntime<Ethereum>,
    config: ReactiveConfig,
    /// Outstanding failed-repair targets for each pool currently `Degraded` by a
    /// resync failure. Recovery is gated on THESE specific targets refreshing,
    /// not on any write at a pool's address (see `recover_resynced_pools` /
    /// `should_recover`). Pools degraded by other means (e.g. a cold-start miss)
    /// have no entry and fall back to read-set coverage.
    degraded_targets: HashMap<PoolKey, PendingTargets>,
}

impl AmmSyncEngine {
    /// Build an engine with [`ReactiveConfig::default`].
    pub fn new(registry: AdapterRegistry) -> Result<Self, AmmSyncError> {
        Self::with_config(registry, ReactiveConfig::default())
    }

    /// Build an engine with an explicit reactive runtime config.
    pub fn with_config(
        registry: AdapterRegistry,
        config: ReactiveConfig,
    ) -> Result<Self, AmmSyncError> {
        let runtime = runtime_for(&registry, config.clone())?;
        Ok(Self {
            registry,
            runtime,
            config,
            degraded_targets: HashMap::new(),
        })
    }

    /// Current pool registry.
    pub fn registry(&self) -> &AdapterRegistry {
        &self.registry
    }

    /// Current reactive runtime.
    pub fn runtime(&self) -> &ReactiveRuntime<Ethereum> {
        &self.runtime
    }

    /// Replace the registry and rebuild the underlying handler registration.
    ///
    /// `AmmReactiveHandler` owns a clone of the registry at registration time, so
    /// callers that add pools or update read-set metadata should replace the
    /// registry through this method rather than mutating a detached clone.
    ///
    /// Rebuilding is not free: the engine constructs a **fresh**
    /// [`ReactiveRuntime`], so any state the previous runtime accumulated
    /// across batches (reorg tracking, pending work) is discarded. Call this
    /// between batches — never mid-stream — and prefer batching several
    /// registry changes into one replacement.
    pub fn replace_registry(&mut self, registry: AdapterRegistry) -> Result<(), AmmSyncError> {
        let runtime = runtime_for(&registry, self.config.clone())?;
        self.registry = registry;
        self.runtime = runtime;
        // Drop recovery tracking for any pool the new registry no longer holds,
        // so re-registering the same key later can't inherit stale targets.
        if !self.degraded_targets.is_empty() {
            let registry = &self.registry;
            self.degraded_targets
                .retain(|key, _| registry.pool(key).is_some());
        }
        Ok(())
    }

    /// Register additional pools into the live engine.
    ///
    /// Atomic: on a duplicate key nothing changes. Rebuilds the runtime once
    /// for the whole batch (see [`replace_registry`](Self::replace_registry)
    /// for the cost), so prefer one call with many pools over many calls.
    ///
    /// Registration alone warms no state: cold-start the registrations first
    /// (or supply explicit read-set metadata), and widen the consumer-owned
    /// provider subscription if it filters by address.
    pub fn register_pools(
        &mut self,
        pools: impl IntoIterator<Item = PoolRegistration>,
    ) -> Result<(), AmmSyncError> {
        let mut registry = self.registry.clone();
        for pool in pools {
            registry.register_pool(pool)?;
        }
        self.replace_registry(registry)
    }

    /// Stop tracking pools, returning the removed registrations.
    ///
    /// Unknown keys are skipped; when nothing was removed the runtime is not
    /// rebuilt. Cache state the pools warmed stays in place — use
    /// [`unregister_pools_evicting`](Self::unregister_pools_evicting) to also
    /// release it. A consumer-owned provider subscription that filters by
    /// address is not updated by this call.
    pub fn unregister_pools(
        &mut self,
        keys: &[PoolKey],
    ) -> Result<Vec<PoolRegistration>, AmmSyncError> {
        let mut registry = self.registry.clone();
        let removed: Vec<PoolRegistration> = keys
            .iter()
            .filter_map(|key| registry.unregister_pool(key))
            .collect();
        if removed.is_empty() {
            return Ok(removed);
        }
        self.replace_registry(registry)?;
        Ok(removed)
    }

    /// [`unregister_pools`](Self::unregister_pools), then purge cache state
    /// owned exclusively by the removed pools.
    ///
    /// Conservative by construction: an address is purged wholesale only when
    /// no remaining pool references it; a shared address (e.g. the Balancer
    /// vault) only loses the removed pool's read-set slots that no remaining
    /// pool covers. State that cannot be provably attributed (e.g. lazily
    /// fetched quote-target bytecode) is left in place.
    pub fn unregister_pools_evicting(
        &mut self,
        keys: &[PoolKey],
        cache: &mut dyn AdapterCache,
    ) -> Result<Vec<PoolRegistration>, AmmSyncError> {
        let removed = self.unregister_pools(keys)?;
        evict_exclusive_state(cache, &removed, &self.registry);
        Ok(removed)
    }

    /// Ingest one batch and execute all emitted slot repairs.
    ///
    /// This deliberately calls `ingest_batch_with_resync`, not plain
    /// `ingest_batch`: Balancer, Curve, and V3 liquidity events rely on the
    /// resync phase to refresh slots whose final values are not carried in logs.
    ///
    /// Pool status tracks the resync outcomes both ways: a pool whose target
    /// failed is marked [`PoolStatus::Degraded`], and a degraded pool whose
    /// covered slots were authoritatively refreshed (with no failed targets
    /// this batch) is marked [`PoolStatus::Ready`] again.
    pub fn ingest_batch(
        &mut self,
        cache: &mut EvmCache,
        batch: ReactiveInputBatch<Ethereum>,
    ) -> Result<AmmSyncBatchReport, AmmSyncError> {
        let reactive = self.runtime.ingest_batch_with_resync(cache, batch)?;
        let resync_state_updates = resync_state_update_count(&reactive);
        let resync_failures = resync_failure_count(&reactive);
        let degraded_pools = self.mark_failed_resync_pools(&reactive);
        let recovered_pools = self.recover_resynced_pools(&reactive, &degraded_pools);

        Ok(AmmSyncBatchReport {
            reactive,
            degraded_pools,
            recovered_pools,
            resync_state_updates,
            resync_failures,
        })
    }

    fn mark_failed_resync_pools(&mut self, report: &ReactiveBatchReport<Ethereum>) -> Vec<PoolKey> {
        let mut degraded = Vec::new();
        for failure in resync_failures(report) {
            for key in pools_for_failure(&self.registry, failure) {
                if !degraded.contains(&key) {
                    degraded.push(key.clone());
                }
                // Remember the concrete slots/accounts this pool's repair failed
                // on, so recovery can require exactly them to refresh rather than
                // vouching for the pool on any later write at its address.
                let recorded = self
                    .registry
                    .pool(&key)
                    .map(|pool| PendingTargets::covered(&failure.target, pool));
                if let Some(targets) = recorded {
                    self.degraded_targets
                        .entry(key.clone())
                        .or_default()
                        .merge(targets);
                }
                if let Some(pool) = self.registry.pool_mut(&key) {
                    pool.status = PoolStatus::Degraded;
                }
            }
        }
        degraded
    }

    /// Flip degraded pools back to [`PoolStatus::Ready`] when this batch's resync
    /// phase authoritatively refreshed the state they were waiting on.
    ///
    /// Recovery is target-aware (see `should_recover`):
    ///
    /// - A pool degraded by a **tracked** resync failure recovers only when the
    ///   specific `(address, slot)` targets it failed on are refreshed — an
    ///   unrelated write at the same address does not vouch for it. This is what
    ///   keeps address-keyed protocols (Uniswap V2, the V3 family, Solidly),
    ///   whose read-set is the whole account, from recovering too eagerly.
    /// - A pool degraded by other means (e.g. a cold-start miss) has no recorded
    ///   target and falls back to read-set coverage: precise for discovered-slot
    ///   protocols (Curve, Balancer), address-level for the rest.
    ///
    /// A recovered pool's tracked targets are cleared. Conservative by
    /// construction: a pool that covers no slots and has no tracked target can
    /// never be vouched for, so it stays degraded.
    fn recover_resynced_pools(
        &mut self,
        report: &ReactiveBatchReport<Ethereum>,
        degraded_now: &[PoolKey],
    ) -> Vec<PoolKey> {
        let refreshed: Vec<(Address, U256)> = resynced_slot_writes(report).collect();
        if refreshed.is_empty() {
            return Vec::new();
        }
        let targets = &self.degraded_targets;
        let recovered: Vec<PoolKey> = self
            .registry
            .pools()
            .filter(|pool| pool.status == PoolStatus::Degraded)
            .filter(|pool| !degraded_now.contains(&pool.key))
            .filter(|pool| should_recover(pool, targets.get(&pool.key), &refreshed))
            .map(|pool| pool.key.clone())
            .collect();
        for key in &recovered {
            if let Some(pool) = self.registry.pool_mut(key) {
                pool.status = PoolStatus::Ready;
            }
            self.degraded_targets.remove(key);
        }
        recovered
    }
}

fn runtime_for(
    registry: &AdapterRegistry,
    config: ReactiveConfig,
) -> Result<ReactiveRuntime<Ethereum>, RegisterError> {
    let mut runtime = ReactiveRuntime::<Ethereum>::new(config);
    runtime.register_handler(Arc::new(AmmReactiveHandler::new(registry.clone())))?;
    Ok(runtime)
}

fn resync_state_update_count(report: &ReactiveBatchReport<Ethereum>) -> usize {
    report
        .reports
        .iter()
        .filter_map(|report| match report.as_ref() {
            ReactiveReport::Resynced(report) => Some(report.state_updates.len()),
            _ => None,
        })
        .sum()
}

fn resync_failure_count(report: &ReactiveBatchReport<Ethereum>) -> usize {
    resync_failures(report).count()
}

fn resync_failures(report: &ReactiveBatchReport<Ethereum>) -> impl Iterator<Item = &ResyncFailure> {
    report
        .reports
        .iter()
        .flat_map(|report| match report.as_ref() {
            ReactiveReport::Resynced(report) => report.failed.iter(),
            _ => [].iter(),
        })
}

/// `(address, slot)` pairs authoritatively refreshed by this batch's executed
/// resync reports.
fn resynced_slot_writes(
    report: &ReactiveBatchReport<Ethereum>,
) -> impl Iterator<Item = (Address, U256)> + '_ {
    report
        .reports
        .iter()
        .flat_map(|report| match report.as_ref() {
            ReactiveReport::Resynced(report) => report.state_updates.as_slice(),
            _ => &[],
        })
        .filter_map(|update| match update {
            evm_fork_cache::StateUpdate::Slot { address, slot, .. } => Some((*address, *slot)),
            _ => None,
        })
}

/// Purge cache state owned exclusively by `removed` pools, sparing anything a
/// `remaining` pool still references.
fn evict_exclusive_state(
    cache: &mut dyn AdapterCache,
    removed: &[PoolRegistration],
    remaining: &AdapterRegistry,
) {
    for pool in removed {
        for address in pool_state_addresses(pool) {
            let shared = remaining
                .pools()
                .any(|other| pool_state_addresses(other).contains(&address));
            if !shared {
                cache.purge_storage(address);
                continue;
            }
            let exclusive: Vec<U256> = pool_covered_slots_at(pool, address)
                .into_iter()
                .filter(|slot| {
                    !remaining
                        .pools()
                        .any(|other| pool_covers_storage_slot(other, address, *slot))
                })
                .collect();
            if !exclusive.is_empty() {
                cache.purge_slots(address, &exclusive);
            }
        }
    }
}

/// The addresses a pool's state lives at: its own key address plus any
/// configured state addresses (deduped).
fn pool_state_addresses(pool: &PoolRegistration) -> Vec<Address> {
    let mut addresses: Vec<Address> = pool.key.address().into_iter().collect();
    for address in &pool.state_addresses {
        if !addresses.contains(address) {
            addresses.push(*address);
        }
    }
    addresses
}

/// The slots `pool`'s metadata explicitly claims at `address` — the
/// attributable subset of what [`pool_covers_storage_slot`] recognizes, used
/// for slot-level eviction at shared addresses.
fn pool_covered_slots_at(pool: &PoolRegistration, address: Address) -> Vec<U256> {
    match &pool.metadata {
        ProtocolMetadata::BalancerV2(metadata)
            if metadata
                .vault
                .or_else(|| pool.state_addresses.first().copied())
                == Some(address) =>
        {
            metadata.balance_slots.clone()
        }
        ProtocolMetadata::Curve(metadata) if pool.key.address() == Some(address) => {
            metadata.discovered_slots.clone()
        }
        _ => Vec::new(),
    }
}

fn pools_for_failure(registry: &AdapterRegistry, failure: &ResyncFailure) -> Vec<PoolKey> {
    registry
        .pools()
        .filter(|pool| pool_matches_resync_target(pool, &failure.target))
        .map(|pool| pool.key.clone())
        .collect()
}

fn pool_matches_resync_target(pool: &PoolRegistration, target: &ResyncTarget) -> bool {
    match target {
        ResyncTarget::StorageSlot { address, slot } => {
            pool_covers_storage_slot(pool, *address, *slot)
        }
        ResyncTarget::StorageSlots { address, slots } => slots
            .iter()
            .any(|slot| pool_covers_storage_slot(pool, *address, *slot)),
        ResyncTarget::Account { address, .. } => pool_owns_address(pool, *address),
    }
}

/// The concrete repair targets a `Degraded` pool is waiting on, recorded when a
/// resync fails so recovery can require exactly them — not any write at the
/// pool's address. See `AmmSyncEngine::recover_resynced_pools`.
#[derive(Clone, Debug, Default)]
struct PendingTargets {
    /// `(address, slot)` repairs that failed; each must be refreshed to recover.
    slots: Vec<(Address, U256)>,
    /// Whole-account repairs that failed; any refresh at the address clears them
    /// (an account resync refreshes the account, with no finer signal).
    accounts: Vec<Address>,
}

impl PendingTargets {
    /// The subset of `target` that `pool` actually covers (mirrors
    /// `pool_matches_resync_target`'s per-slot coverage check).
    fn covered(target: &ResyncTarget, pool: &PoolRegistration) -> Self {
        let mut pending = Self::default();
        match target {
            ResyncTarget::StorageSlot { address, slot } => {
                if pool_covers_storage_slot(pool, *address, *slot) {
                    pending.slots.push((*address, *slot));
                }
            }
            ResyncTarget::StorageSlots { address, slots } => {
                for slot in slots {
                    if pool_covers_storage_slot(pool, *address, *slot) {
                        pending.slots.push((*address, *slot));
                    }
                }
            }
            ResyncTarget::Account { address, .. } => {
                if pool_owns_address(pool, *address) {
                    pending.accounts.push(*address);
                }
            }
        }
        pending
    }

    /// Fold `other`'s targets in, de-duplicated.
    fn merge(&mut self, other: Self) {
        for slot in other.slots {
            if !self.slots.contains(&slot) {
                self.slots.push(slot);
            }
        }
        for address in other.accounts {
            if !self.accounts.contains(&address) {
                self.accounts.push(address);
            }
        }
    }

    fn is_empty(&self) -> bool {
        self.slots.is_empty() && self.accounts.is_empty()
    }

    /// Whether `refreshed` (this batch's authoritative slot writes) covers every
    /// outstanding target: every recorded slot present, and every recorded
    /// account seeing at least one write at its address.
    fn satisfied_by(&self, refreshed: &[(Address, U256)]) -> bool {
        let slots_ok = self.slots.iter().all(|target| refreshed.contains(target));
        let accounts_ok = self
            .accounts
            .iter()
            .all(|address| refreshed.iter().any(|(a, _)| a == address));
        slots_ok && accounts_ok
    }
}

/// Whether a `Degraded` `pool` should flip back to `Ready` given this batch's
/// `refreshed` slot writes and its recorded failed `targets` (if any).
///
/// With a non-empty tracked target set, recovery requires exactly those to
/// refresh; otherwise it falls back to read-set coverage. An empty set is
/// treated as untracked (coverage fallback).
fn should_recover(
    pool: &PoolRegistration,
    targets: Option<&PendingTargets>,
    refreshed: &[(Address, U256)],
) -> bool {
    match targets {
        Some(pending) if !pending.is_empty() => pending.satisfied_by(refreshed),
        _ => refreshed
            .iter()
            .any(|(address, slot)| pool_covers_storage_slot(pool, *address, *slot)),
    }
}

fn pool_covers_storage_slot(pool: &PoolRegistration, address: Address, slot: U256) -> bool {
    match pool.protocol() {
        ProtocolId::BalancerV2 => {
            let ProtocolMetadata::BalancerV2(metadata) = &pool.metadata else {
                return false;
            };
            metadata
                .vault
                .or_else(|| pool.state_addresses.first().copied())
                == Some(address)
                && metadata.balance_slots.contains(&slot)
        }
        ProtocolId::Curve => {
            let ProtocolMetadata::Curve(metadata) = &pool.metadata else {
                return false;
            };
            pool.key.address() == Some(address) && metadata.discovered_slots.contains(&slot)
        }
        ProtocolId::UniswapV3 | ProtocolId::PancakeV3 | ProtocolId::Slipstream => {
            pool.key.address() == Some(address)
        }
        ProtocolId::UniswapV2 | ProtocolId::SolidlyV2 => pool.key.address() == Some(address),
        #[cfg(feature = "experimental-protocols")]
        ProtocolId::BalancerV3 | ProtocolId::Erc4626 | ProtocolId::UniswapV4 => {
            pool_owns_address(pool, address)
        }
        ProtocolId::Custom(_) => pool_owns_address(pool, address),
    }
}

fn pool_owns_address(pool: &PoolRegistration, address: Address) -> bool {
    pool.key.address() == Some(address) || pool.state_addresses.contains(&address)
}

#[cfg(test)]
mod tests {
    use super::*;

    // An address-keyed V3 pool: `pool_covers_storage_slot` returns true for any
    // slot at its address, so it is the case the target-tracking fix protects.
    fn v3_pool(address: Address) -> PoolRegistration {
        PoolRegistration::new(PoolKey::UniswapV3(address))
    }

    #[test]
    fn tracked_target_recovers_only_on_that_slot() {
        // The reviewer's case: a pool degraded by a failed repair on `target`
        // must NOT recover when an unrelated slot at the same address refreshes —
        // only when `target` itself does.
        let address = Address::repeat_byte(0xaa);
        let target = U256::from(1);
        let unrelated = U256::from(2);
        let pool = v3_pool(address);
        let pending = PendingTargets {
            slots: vec![(address, target)],
            accounts: Vec::new(),
        };

        assert!(!should_recover(
            &pool,
            Some(&pending),
            &[(address, unrelated)]
        ));
        assert!(should_recover(&pool, Some(&pending), &[(address, target)]));
        // A superset (target plus others) also recovers.
        assert!(should_recover(
            &pool,
            Some(&pending),
            &[(address, unrelated), (address, target)],
        ));
    }

    #[test]
    fn untracked_degradation_falls_back_to_address_coverage() {
        // Without a recorded target (e.g. a cold-start miss), an address-keyed
        // pool keeps the prior coverage behavior: any same-address write vouches,
        // a write elsewhere does not.
        let address = Address::repeat_byte(0xbb);
        let pool = v3_pool(address);
        assert!(should_recover(&pool, None, &[(address, U256::from(9))]));
        assert!(!should_recover(
            &pool,
            None,
            &[(Address::repeat_byte(0xcc), U256::from(9))],
        ));
    }

    #[test]
    fn empty_pending_is_treated_as_untracked() {
        let address = Address::repeat_byte(0xdd);
        let pool = v3_pool(address);
        let pending = PendingTargets::default();
        assert!(should_recover(
            &pool,
            Some(&pending),
            &[(address, U256::from(3))]
        ));
    }

    #[test]
    fn covered_records_only_slots_the_pool_covers() {
        let address = Address::repeat_byte(0xee);
        let elsewhere = Address::repeat_byte(0x01);
        let pool = v3_pool(address);

        let here = ResyncTarget::StorageSlots {
            address,
            slots: vec![U256::from(1), U256::from(2)],
        };
        let recorded = PendingTargets::covered(&here, &pool);
        assert_eq!(
            recorded.slots,
            vec![(address, U256::from(1)), (address, U256::from(2))],
        );

        // A slot on a different address is not this pool's to recover on.
        let other = ResyncTarget::StorageSlot {
            address: elsewhere,
            slot: U256::from(5),
        };
        assert!(PendingTargets::covered(&other, &pool).is_empty());
    }
}
