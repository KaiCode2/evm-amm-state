//! Immutable, versioned AMM state publications for concurrent consumers.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use evm_fork_cache::cache::EvmSnapshot;

use super::{
    AdapterInstanceId, AdapterKey, AdapterRegistry, AmmChangeSet, AmmOwnershipIndex, AmmRuntimeId,
    AmmStatePoint, AmmStateVersion, PoolInstanceId, PoolKey, PoolRegistration, PoolStateRevision,
};

/// Registry/ownership divergence rejected before publishing a topology snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum AdapterRegistrySnapshotError {
    /// A registry pool has no active generation in the ownership index.
    RegistryPoolMissingOwnership(PoolKey),
    /// An active ownership generation has no registry registration.
    OwnershipPoolMissingRegistry(PoolInstanceId),
    /// A registry adapter family has no active ownership generation.
    RegistryAdapterMissingOwnership(AdapterKey),
    /// An active adapter generation has no corresponding registry family.
    OwnershipAdapterMissingRegistry(AdapterInstanceId),
    /// A registry pool has no adapter serving its protocol.
    RegistryPoolMissingAdapter(PoolKey),
    /// An active pool generation has no adapter ownership record.
    OwnershipPoolMissingAdapter(PoolInstanceId),
    /// A pool's ownership record points at a different adapter family.
    PoolAdapterMismatch {
        /// Pool generation whose adapter ownership diverged.
        pool: Box<PoolInstanceId>,
        /// Adapter family selected by the registry.
        registry: Box<AdapterKey>,
        /// Adapter generation recorded by ownership.
        ownership: Box<AdapterInstanceId>,
    },
}

impl fmt::Display for AdapterRegistrySnapshotError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "adapter registry snapshot rejected: {self:?}")
    }
}

impl std::error::Error for AdapterRegistrySnapshotError {}

/// Immutable registry and generation-ownership view published with a state snapshot.
///
/// The contained [`AdapterRegistry`] is cloned only when topology changes. Its
/// adapter implementations remain shared through their existing `Arc`s.
pub struct AdapterRegistrySnapshot {
    registry: Arc<AdapterRegistry>,
    active_pools: BTreeMap<PoolKey, PoolInstanceId>,
    active_adapters: BTreeMap<AdapterKey, AdapterInstanceId>,
}

impl AdapterRegistrySnapshot {
    /// Build a checked immutable view of matching registry and ownership state.
    pub fn try_new(
        registry: &AdapterRegistry,
        ownership: &AmmOwnershipIndex,
    ) -> Result<Self, AdapterRegistrySnapshotError> {
        let mut registry_pools: Vec<_> = registry.pools().collect();
        registry_pools.sort_by(|left, right| left.key.cmp(&right.key));
        for pool in &registry_pools {
            if ownership.active_pool(&pool.key).is_none() {
                return Err(AdapterRegistrySnapshotError::RegistryPoolMissingOwnership(
                    pool.key.clone(),
                ));
            }
        }
        for instance in ownership.pools() {
            if registry.pool(instance.key()).is_none() {
                return Err(AdapterRegistrySnapshotError::OwnershipPoolMissingRegistry(
                    instance.clone(),
                ));
            }
        }
        for pool in registry_pools {
            let Some(adapter) = registry.adapter(pool.protocol()) else {
                return Err(AdapterRegistrySnapshotError::RegistryPoolMissingAdapter(
                    pool.key.clone(),
                ));
            };
            let registry_adapter = AdapterKey::new(adapter.protocol(), adapter.protocols());
            let Some(instance) = ownership.active_pool(&pool.key) else {
                return Err(AdapterRegistrySnapshotError::RegistryPoolMissingOwnership(
                    pool.key.clone(),
                ));
            };
            let Some(owned_adapter) = ownership.adapter_for_pool(instance) else {
                return Err(AdapterRegistrySnapshotError::OwnershipPoolMissingAdapter(
                    instance.clone(),
                ));
            };
            if owned_adapter.key() != &registry_adapter
                || ownership.active_adapter(&registry_adapter) != Some(owned_adapter)
            {
                return Err(AdapterRegistrySnapshotError::PoolAdapterMismatch {
                    pool: Box::new(instance.clone()),
                    registry: Box::new(registry_adapter),
                    ownership: Box::new(owned_adapter.clone()),
                });
            }
        }

        let registry_adapters: BTreeMap<AdapterKey, ()> = registry
            .adapters()
            .map(|adapter| (AdapterKey::new(adapter.protocol(), adapter.protocols()), ()))
            .collect();
        for key in registry_adapters.keys() {
            if ownership.active_adapter(key).is_none() {
                return Err(
                    AdapterRegistrySnapshotError::RegistryAdapterMissingOwnership(key.clone()),
                );
            }
        }
        for instance in ownership.adapters() {
            if !registry_adapters.contains_key(instance.key()) {
                return Err(
                    AdapterRegistrySnapshotError::OwnershipAdapterMissingRegistry(instance.clone()),
                );
            }
        }

        let active_pools = ownership
            .pools()
            .map(|instance| (instance.key().clone(), instance.clone()))
            .collect();
        let active_adapters = ownership
            .adapters()
            .map(|instance| (instance.key().clone(), instance.clone()))
            .collect();
        Ok(Self {
            registry: Arc::new(registry.clone()),
            active_pools,
            active_adapters,
        })
    }

    /// Immutable adapter registry represented by this topology snapshot.
    pub fn registry(&self) -> &AdapterRegistry {
        &self.registry
    }

    /// Active generation for a logical pool key.
    pub fn pool_instance(&self, key: &PoolKey) -> Option<&PoolInstanceId> {
        self.active_pools.get(key)
    }

    /// Resolve a registration only when `instance` is the active generation.
    pub fn pool(&self, instance: &PoolInstanceId) -> Option<&PoolRegistration> {
        (self.active_pools.get(instance.key()) == Some(instance))
            .then(|| self.registry.pool(instance.key()))
            .flatten()
    }

    /// Active generation for an adapter-family key.
    pub fn adapter_instance(&self, key: &AdapterKey) -> Option<&AdapterInstanceId> {
        self.active_adapters.get(key)
    }

    /// Resolve an adapter only when `instance` is the active generation.
    pub fn adapter(&self, instance: &AdapterInstanceId) -> Option<&Arc<dyn super::AmmAdapter>> {
        (self.active_adapters.get(instance.key()) == Some(instance))
            .then(|| instance.key().protocols().first().copied())
            .flatten()
            .and_then(|protocol| self.registry.adapter(protocol))
    }

    /// Number of active pool registrations.
    pub fn pool_count(&self) -> usize {
        self.active_pools.len()
    }

    /// Number of active adapter-family registrations.
    pub fn adapter_count(&self) -> usize {
        self.active_adapters.len()
    }

    /// Active pool generations in canonical logical-key order.
    pub fn pools(&self) -> impl Iterator<Item = (&PoolKey, &PoolInstanceId)> {
        self.active_pools.iter()
    }

    /// Active adapter generations in canonical family-key order.
    pub fn adapters(&self) -> impl Iterator<Item = (&AdapterKey, &AdapterInstanceId)> {
        self.active_adapters.iter()
    }
}

/// Canonically ordered quote-relevant revision by active pool generation.
pub type PoolRevisionMap = BTreeMap<PoolInstanceId, PoolStateRevision>;

/// Immutable coherent state publication used by search and other readers.
///
/// Every field describes the same post-block point and state version. Consumers
/// clone the outer `Arc<AmmStateSnapshot>` and create isolated EVM overlays from
/// [`cache_snapshot`](Self::cache_snapshot) without borrowing the cache-owning
/// runtime actor.
pub struct AmmStateSnapshot {
    runtime_id: AmmRuntimeId,
    version: AmmStateVersion,
    point: AmmStatePoint,
    interest_revision: u64,
    cache: Arc<EvmSnapshot>,
    registry: Arc<AdapterRegistrySnapshot>,
    pool_revisions: Arc<PoolRevisionMap>,
}

impl AmmStateSnapshot {
    #[cfg_attr(not(feature = "live-runtime"), allow(dead_code))]
    pub(crate) fn new(
        runtime_id: AmmRuntimeId,
        version: AmmStateVersion,
        point: AmmStatePoint,
        interest_revision: u64,
        cache: Arc<EvmSnapshot>,
        registry: Arc<AdapterRegistrySnapshot>,
        pool_revisions: Arc<PoolRevisionMap>,
    ) -> Self {
        Self {
            runtime_id,
            version,
            point,
            interest_revision,
            cache,
            registry,
            pool_revisions,
        }
    }

    /// Process-unique lineage of the runtime that published this snapshot.
    pub const fn runtime_id(&self) -> AmmRuntimeId {
        self.runtime_id
    }

    /// Monotonic coherent state version.
    pub const fn version(&self) -> AmmStateVersion {
        self.version
    }

    /// Explicit post-block point represented by every snapshot component.
    pub const fn point(&self) -> AmmStatePoint {
        self.point
    }

    /// Subscriber interest revision reconciled by this publication.
    pub const fn interest_revision(&self) -> u64 {
        self.interest_revision
    }

    /// Immutable fork-cache state for isolated overlays and reads.
    pub fn cache(&self) -> &EvmSnapshot {
        &self.cache
    }

    /// Clone the immutable cache snapshot for an isolated parallel overlay.
    pub fn cache_snapshot(&self) -> Arc<EvmSnapshot> {
        Arc::clone(&self.cache)
    }

    /// Immutable registry topology and generation view.
    pub fn registry(&self) -> &AdapterRegistrySnapshot {
        &self.registry
    }

    /// Clone the immutable registry topology snapshot.
    pub fn registry_snapshot(&self) -> Arc<AdapterRegistrySnapshot> {
        Arc::clone(&self.registry)
    }

    /// Quote-relevant revision for one active pool generation.
    pub fn pool_revision(&self, pool: &PoolInstanceId) -> Option<PoolStateRevision> {
        self.pool_revisions.get(pool).copied()
    }

    /// Complete pool revision map.
    pub fn pool_revisions(&self) -> &PoolRevisionMap {
        &self.pool_revisions
    }

    /// Clone the immutable pool-revision map.
    pub fn pool_revisions_snapshot(&self) -> Arc<PoolRevisionMap> {
        Arc::clone(&self.pool_revisions)
    }
}

/// Reliable publication unit pairing changes with their exact immutable state.
pub struct AmmStateCommit {
    snapshot: Arc<AmmStateSnapshot>,
    changes: Arc<AmmChangeSet>,
}

impl AmmStateCommit {
    #[cfg_attr(not(feature = "live-runtime"), allow(dead_code))]
    pub(crate) fn new(snapshot: Arc<AmmStateSnapshot>, changes: Arc<AmmChangeSet>) -> Self {
        debug_assert_eq!(snapshot.version(), changes.version());
        debug_assert_eq!(snapshot.point(), changes.point());
        Self { snapshot, changes }
    }

    /// Exact immutable state produced by this commit.
    pub fn snapshot(&self) -> &Arc<AmmStateSnapshot> {
        &self.snapshot
    }

    /// Typed changes that produced the snapshot.
    pub fn changes(&self) -> &Arc<AmmChangeSet> {
        &self.changes
    }
}
