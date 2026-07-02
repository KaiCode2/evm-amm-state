use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use alloy_primitives::{Address, B256, Log};

use super::{
    AdapterCache, AmmAdapter, CacheError, DeferredOutcome, DeferredWork, EventRoute, EventSource,
    PoolKey, PoolRegistration, ProtocolId, RepairAction,
};

/// Registry of tracked AMM pools and protocol adapters.
#[derive(Clone, Default)]
pub struct AdapterRegistry {
    adapters: HashMap<ProtocolId, Arc<dyn AmmAdapter>>,
    pools: HashMap<PoolKey, PoolRegistration>,
}

impl AdapterRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register_pool(&mut self, registration: PoolRegistration) -> Result<(), RegistryError> {
        if self.pools.contains_key(&registration.key) {
            return Err(RegistryError::DuplicatePool(registration.key));
        }

        self.pools.insert(registration.key.clone(), registration);
        Ok(())
    }

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

    pub fn adapter(&self, protocol: ProtocolId) -> Option<&Arc<dyn AmmAdapter>> {
        self.adapters.get(&protocol)
    }

    pub fn adapters(&self) -> impl Iterator<Item = &Arc<dyn AmmAdapter>> {
        self.adapters.values()
    }

    pub fn pool(&self, key: &PoolKey) -> Option<&PoolRegistration> {
        self.pools.get(key)
    }

    pub fn pools(&self) -> impl Iterator<Item = &PoolRegistration> {
        self.pools.values()
    }

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

    pub fn subscription_spec(&self) -> SubscriptionSpec {
        SubscriptionSpec {
            sources: self
                .pools
                .values()
                .flat_map(|pool| pool.event_sources.iter().cloned())
                .collect(),
        }
    }

    pub fn len(&self) -> usize {
        self.pools.len()
    }

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

#[non_exhaustive]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SubscriptionSpec {
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
    DuplicatePool(PoolKey),
    DuplicateAdapter(ProtocolId),
}

impl fmt::Display for RegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicatePool(key) => write!(f, "pool is already registered: {key:?}"),
            Self::DuplicateAdapter(protocol) => {
                write!(f, "adapter is already registered: {protocol:?}")
            }
        }
    }
}

impl std::error::Error for RegistryError {}
