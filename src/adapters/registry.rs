use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use alloy_primitives::{Address, B256, Log};

use super::{
    AdapterCache, AmmAdapter, CacheError, DeferredOutcome, DeferredWork, EventRoute, EventSource,
    PoolKey, PoolRegistration, ProtocolId, RepairAction,
};

/// Registry of tracked AMM pools and protocol adapters.
#[derive(Clone)]
pub struct AdapterRegistry {
    adapters: HashMap<ProtocolId, Arc<dyn AmmAdapter>>,
    pools: HashMap<PoolKey, PoolRegistration>,
    /// Whether [`cold_start`](Self::cold_start) seeds and verifies adapter
    /// runtime bytecode (an optimization over the lazy real-code fetch).
    /// Defaults to `true`; opt out via [`with_code_seeding`](Self::with_code_seeding).
    pub(crate) code_seeding: bool,
    /// Whether [`cold_start_many`](Self::cold_start_many) / [`cold_start_primed`](Self::cold_start_primed)
    /// derive an unknown read-set with one `eth_createAccessList` call before
    /// warming (the two-shot first-boot fast path). Defaults to `true`; opt out
    /// via [`with_access_list_discovery`](Self::with_access_list_discovery) on a
    /// provider that lacks `eth_createAccessList` (a per-pool failure already
    /// falls back to local discovery, so this is only a round-trip optimization).
    pub(crate) access_list_discovery: bool,
}

impl Default for AdapterRegistry {
    fn default() -> Self {
        Self {
            adapters: HashMap::new(),
            pools: HashMap::new(),
            code_seeding: true,
            access_list_discovery: true,
        }
    }
}

impl AdapterRegistry {
    /// An empty registry with code-seeding enabled.
    pub fn new() -> Self {
        Self::default()
    }

    /// Enable or disable verified-code-seeding during
    /// [`cold_start`](Self::cold_start).
    ///
    /// When `false`, cold-start performs no seeding and no verification call;
    /// the pool's runtime code is fetched lazily at first simulate as usual.
    /// Defaults to `true`.
    pub fn with_code_seeding(mut self, enabled: bool) -> Self {
        self.code_seeding = enabled;
        self
    }

    /// Enable or disable `eth_createAccessList`-based read-set discovery during
    /// [`cold_start_many`](Self::cold_start_many) / [`cold_start_primed`](Self::cold_start_primed).
    ///
    /// When `true` (the default), a layout-free pool with no known read-set
    /// (Curve / Balancer on first boot) has its `get_dy` / `getPoolTokens`
    /// read-set derived by a single `eth_createAccessList` call and bulk-loaded,
    /// so the subsequent cold-start runs warm instead of faulting each slot
    /// serially. A provider that lacks `eth_createAccessList`, or a per-pool
    /// failure, falls back to local discovery automatically — so disabling this
    /// only avoids the (cheap, self-recovering) attempt.
    pub fn with_access_list_discovery(mut self, enabled: bool) -> Self {
        self.access_list_discovery = enabled;
        self
    }

    /// Register a pool. Errors [`RegistryError::DuplicatePool`] if its key is
    /// already registered.
    pub fn register_pool(&mut self, registration: PoolRegistration) -> Result<(), RegistryError> {
        if self.pools.contains_key(&registration.key) {
            return Err(RegistryError::DuplicatePool(registration.key));
        }

        self.pools.insert(registration.key.clone(), registration);
        Ok(())
    }

    /// Remove a pool registration, returning it if it was present.
    ///
    /// The inverse of [`register_pool`](Self::register_pool). Removal only
    /// stops routing/dispatch from this registry — cache state warmed for the
    /// pool is untouched (`AmmSyncEngine::unregister_pools_evicting` also
    /// releases that).
    pub fn unregister_pool(&mut self, key: &PoolKey) -> Option<PoolRegistration> {
        self.pools.remove(key)
    }

    /// Register an adapter under every id it [`serves`](AmmAdapter::protocols).
    /// Errors [`RegistryError::DuplicateAdapter`] if any of those ids is taken
    /// (no partial insert).
    pub fn register_adapter(&mut self, adapter: Arc<dyn AmmAdapter>) -> Result<(), RegistryError> {
        // Validate every claimed id up front so a multi-protocol adapter never
        // partially inserts when one of its ids collides.
        let protocols = adapter.protocols();
        for protocol in &protocols {
            if self.adapters.contains_key(protocol) {
                return Err(RegistryError::DuplicateAdapter(*protocol));
            }
        }

        // Same `Arc` stored under every id in the family.
        for protocol in protocols {
            self.adapters.insert(protocol, adapter.clone());
        }
        Ok(())
    }

    /// Remove an adapter — under **every** protocol id it serves — returning
    /// it. The inverse of [`register_adapter`](Self::register_adapter).
    ///
    /// Fails with [`RegistryError::AdapterInUse`] while any registered pool
    /// still dispatches to one of those ids (unregister the pools first), so
    /// a registry can never route a pool to a missing adapter. Returns
    /// `Ok(None)` when nothing is registered under `protocol`.
    pub fn unregister_adapter(
        &mut self,
        protocol: ProtocolId,
    ) -> Result<Option<Arc<dyn AmmAdapter>>, RegistryError> {
        let Some(adapter) = self.adapters.get(&protocol).cloned() else {
            return Ok(None);
        };
        let served = adapter.protocols();
        if let Some(pool) = self
            .pools
            .values()
            .find(|pool| served.contains(&pool.key.protocol()))
        {
            return Err(RegistryError::AdapterInUse {
                protocol: pool.key.protocol(),
                pool: pool.key.clone(),
            });
        }
        for id in served {
            self.adapters.remove(&id);
        }
        Ok(Some(adapter))
    }

    /// The adapter registered for `protocol`, if any.
    pub fn adapter(&self, protocol: ProtocolId) -> Option<&Arc<dyn AmmAdapter>> {
        self.adapters.get(&protocol)
    }

    /// Iterate the registered adapters (a family adapter appears once per id).
    pub fn adapters(&self) -> impl Iterator<Item = &Arc<dyn AmmAdapter>> {
        self.adapters.values()
    }

    /// The registration for `key`, if tracked.
    pub fn pool(&self, key: &PoolKey) -> Option<&PoolRegistration> {
        self.pools.get(key)
    }

    /// A mutable borrow of the registration for `key`, if tracked.
    pub fn pool_mut(&mut self, key: &PoolKey) -> Option<&mut PoolRegistration> {
        self.pools.get_mut(key)
    }

    /// Iterate the tracked pool registrations.
    pub fn pools(&self) -> impl Iterator<Item = &PoolRegistration> {
        self.pools.values()
    }

    /// Route `log` to the pool it belongs to (generic emitter/topic routing,
    /// then each adapter's `route_log` fallback).
    pub fn route_log(&self, log: &Log) -> Option<&PoolRegistration> {
        if let Some(pool) = self.route_log_generic(log) {
            return Some(pool);
        }

        // A family adapter is stored under several ids; dedup by pointer so its
        // `route_log` is consulted at most once.
        let mut seen: Vec<*const dyn AmmAdapter> = Vec::new();
        for adapter in self.adapters.values() {
            let ptr = Arc::as_ptr(adapter);
            if seen.contains(&ptr) {
                continue;
            }
            seen.push(ptr);

            if let Some(key) = adapter.route_log(log, self)
                && let Some(pool) = self.pools.get(&key)
            {
                return Some(pool);
            }
        }

        None
    }

    pub(crate) fn route_log_generic(&self, log: &Log) -> Option<&PoolRegistration> {
        self.pools.values().find(|registration| {
            registration
                .event_sources
                .iter()
                .any(|source| event_source_matches(source, &registration.key, log))
        })
    }

    /// The sorted, de-duplicated set of `topic0`s across every tracked pool's
    /// event sources (a log-subscription filter).
    pub fn subscription_topics(&self) -> Vec<B256> {
        let mut topics: Vec<B256> = self
            .pools
            .values()
            .flat_map(|pool| pool.event_sources.iter())
            .flat_map(|source| source.topics.iter().copied())
            .collect();

        topics.sort_unstable_by(|a, b| a.as_slice().cmp(b.as_slice()));
        topics.dedup();
        topics
    }

    /// The full [`SubscriptionSpec`] (every tracked pool's event sources).
    pub fn subscription_spec(&self) -> SubscriptionSpec {
        SubscriptionSpec {
            sources: self
                .pools
                .values()
                .flat_map(|pool| pool.event_sources.iter().cloned())
                .collect(),
        }
    }

    /// The number of tracked pools.
    pub fn len(&self) -> usize {
        self.pools.len()
    }

    /// Whether no pools are tracked.
    pub fn is_empty(&self) -> bool {
        self.pools.is_empty()
    }

    /// Execute the [`DeferredWork`] produced by a `Lazy`
    /// [`cold_start`](Self::cold_start) (or any other source) against `cache`.
    ///
    /// `cold_start` returns
    /// [`ColdStartOutcome::ReadyWithDeferred`](super::ColdStartOutcome::ReadyWithDeferred)
    /// for the `Lazy` policy but deliberately leaves the deferred slots unwarmed;
    /// this driver is the explicit, consumer-invoked step that warms them when the
    /// consumer is ready.
    ///
    /// Handling per variant:
    /// - [`DeferredWork::VerifySlots`] and
    ///   [`DeferredWork::Repair`]`(`[`RepairAction::VerifySlots`]`)` →
    ///   [`AdapterCache::verify_slots`]; the returned [`SlotChange`](super::SlotChange)s
    ///   accumulate into [`DeferredOutcome::verified`].
    /// - [`DeferredWork::ColdStart`], [`DeferredWork::Custom`], and any other
    ///   [`DeferredWork::Repair`] variant are *not* executed here (they need
    ///   repair execution / re-cold-start-by-key, out of scope for this driver);
    ///   they are pushed verbatim into [`DeferredOutcome::unhandled`] rather than
    ///   dropped or panicked on.
    ///
    /// Takes `&self`: warming `VerifySlots` mutates only the `cache`, not the
    /// registry. Errors from `verify_slots` propagate via the returned `Result`.
    pub fn run_deferred(
        &self,
        deferred: &[DeferredWork],
        cache: &mut dyn AdapterCache,
    ) -> Result<DeferredOutcome, CacheError> {
        let mut outcome = DeferredOutcome::default();

        for work in deferred {
            match work {
                DeferredWork::VerifySlots(slots)
                | DeferredWork::Repair(RepairAction::VerifySlots(slots)) => {
                    outcome.verified.extend(cache.verify_slots(slots)?);
                }
                DeferredWork::Repair(_)
                | DeferredWork::ColdStart { .. }
                | DeferredWork::Custom(_) => {
                    outcome.unhandled.push(work.clone());
                }
            }
        }

        Ok(outcome)
    }
}

impl fmt::Debug for AdapterRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AdapterRegistry")
            .field("adapter_count", &self.adapters.len())
            .field("pools", &self.pools)
            .finish()
    }
}

/// The set of event sources to subscribe for a registry's tracked pools.
#[non_exhaustive]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SubscriptionSpec {
    /// Every event source to subscribe across the tracked pools.
    pub sources: Vec<EventSource>,
}

impl SubscriptionSpec {
    /// Construct a subscription spec from a set of event sources.
    pub fn new(sources: Vec<EventSource>) -> Self {
        Self { sources }
    }
}

/// Shared per-source routing predicate: does `log` belong to the pool `key` via
/// the emitter/topic filter and routing rule of `source`?
///
/// This is the single source of truth used by both [`AdapterRegistry::route_log_generic`]
/// and the reactive handler's adapter-derived fallback loop.
pub(crate) fn event_source_matches(source: &EventSource, key: &PoolKey, log: &Log) -> bool {
    if source.emitter != log.address {
        return false;
    }

    let topics = log.topics();
    if !source.topics.is_empty()
        && !topics
            .first()
            .is_some_and(|topic0| source.topics.contains(topic0))
    {
        return false;
    }

    match source.route {
        EventRoute::Direct => true,
        EventRoute::IndexedAddress { topic_index } => topics
            .get(topic_index)
            .map(topic_address)
            .is_some_and(|address| key.address() == Some(address)),
        EventRoute::IndexedBytes32 { topic_index } => topics
            .get(topic_index)
            .is_some_and(|topic| key.bytes32() == Some(*topic)),
        EventRoute::AdapterDefined => false,
    }
}

fn topic_address(topic: &B256) -> Address {
    Address::from_slice(&topic.as_slice()[12..])
}

/// Errors raised while mutating an [`AdapterRegistry`].
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RegistryError {
    /// A pool with this key is already registered.
    DuplicatePool(PoolKey),
    /// An adapter for this protocol id is already registered.
    DuplicateAdapter(ProtocolId),
    /// The adapter still serves at least one registered pool and cannot be
    /// unregistered until those pools are removed.
    AdapterInUse {
        /// A protocol id (of the adapter's served set) with a live pool.
        protocol: ProtocolId,
        /// One of the pools still dispatching to the adapter.
        pool: PoolKey,
    },
}

impl fmt::Display for RegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicatePool(key) => write!(f, "pool is already registered: {key:?}"),
            Self::DuplicateAdapter(protocol) => {
                write!(f, "adapter is already registered: {protocol:?}")
            }
            Self::AdapterInUse { protocol, pool } => {
                write!(f, "adapter for {protocol:?} still serves pool {pool:?}")
            }
        }
    }
}

impl std::error::Error for RegistryError {}
