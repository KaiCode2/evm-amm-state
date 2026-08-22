use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use alloy_primitives::{Address, B256, Log};

use super::quote_warmup::{QuoteReadSetLimits, QuoteWarmup};
use super::{
    AdapterCache, AmmAdapter, CacheError, DeferredOutcome, DeferredWork, EventRoute, EventSource,
    PoolKey, PoolRegistration, ProtocolId, ProtocolMismatch, RepairAction, SimConfig,
};
use evm_fork_cache::access_set::StorageAccessList as UpstreamStorageAccessList;

/// Registry of tracked AMM pools and protocol adapters.
#[derive(Clone)]
pub struct AdapterRegistry {
    adapters: HashMap<ProtocolId, Arc<dyn AmmAdapter>>,
    pools: HashMap<PoolKey, PoolRegistration>,
    /// Whether [`cold_start`](Self::cold_start) seeds and verifies adapter
    /// runtime bytecode (an optimization over the lazy real-code fetch).
    /// Defaults to `true`; opt out via [`with_code_seeding`](Self::with_code_seeding).
    pub(crate) code_seeding: bool,
    /// Quote-target configuration used by an eager
    /// [`cold_start_many`](Self::cold_start_many) to warm the canonical quote
    /// entrypoints' bytecode (see [`PoolRegistration::quote_code_targets`]).
    /// Defaults to [`SimConfig::default`]; set it with
    /// [`with_sim_config`](Self::with_sim_config) to match what you pass to
    /// [`simulate_swap`](super::AmmAdapter::simulate_swap).
    pub(crate) sim_config: SimConfig,
    /// Representative quote read sets learned against an exact canonical
    /// baseline and replayed as offline speculative-readiness probes.
    pub(crate) quote_read_sets: HashMap<QuoteWarmup, UpstreamStorageAccessList>,
    /// Bound for each learned representative quote manifest.
    pub(crate) quote_read_set_limits: QuoteReadSetLimits,
    /// Once representative warming is enabled, every affected speculative pool
    /// must have at least one learned quote manifest before it can be published.
    pub(crate) quote_readiness_required: bool,
}

impl Default for AdapterRegistry {
    fn default() -> Self {
        Self {
            adapters: HashMap::new(),
            pools: HashMap::new(),
            code_seeding: true,
            sim_config: SimConfig::default(),
            quote_read_sets: HashMap::new(),
            quote_read_set_limits: QuoteReadSetLimits::default(),
            quote_readiness_required: false,
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

    /// Set the [`SimConfig`] used by an eager
    /// [`cold_start_many`](Self::cold_start_many) to pre-warm the pools' quote
    /// entrypoints' bytecode (QuoterV2 / Router02 / vault). It should match the
    /// `SimConfig` you later pass to
    /// [`simulate_swap`](super::AmmAdapter::simulate_swap) so the warmed target
    /// is the one you quote against; the default warms the canonical
    /// Ethereum-mainnet quoter/router.
    pub fn with_sim_config(mut self, config: SimConfig) -> Self {
        if self.sim_config != config {
            self.quote_read_sets.clear();
        }
        self.sim_config = config;
        self
    }

    /// Set bounded dependency growth for representative quote manifests.
    pub fn with_quote_read_set_limits(mut self, limits: QuoteReadSetLimits) -> Self {
        self.quote_read_set_limits = limits;
        self
    }

    /// Active representative quote dependency limits.
    pub const fn quote_read_set_limits(&self) -> QuoteReadSetLimits {
        self.quote_read_set_limits
    }

    /// Require representative quote readiness for speculative publications.
    ///
    /// Successful [`warm_quote_read_sets`](Self::warm_quote_read_sets) enables
    /// this automatically. This setter is useful when constructing a registry
    /// that must fail closed before its first warmup has completed.
    pub const fn with_quote_readiness_required(mut self, required: bool) -> Self {
        self.quote_readiness_required = required;
        self
    }

    /// Whether affected speculative pools require learned quote manifests.
    pub const fn quote_readiness_required(&self) -> bool {
        self.quote_readiness_required
    }

    /// Whether the registry has any learned representative quote dependencies.
    ///
    /// Canonical runtimes use this as a cheap guard before inspecting the
    /// published cache snapshot for newly learned speculative slots.
    pub fn has_quote_read_sets(&self) -> bool {
        !self.quote_read_sets.is_empty()
    }

    /// Register a pool.
    ///
    /// Fails closed on two conditions, neither of which mutates the registry:
    ///
    /// - [`RegistryError::DuplicatePool`] if its key is already registered.
    /// - [`RegistryError::ProtocolMismatch`] if its [`PoolKey`] and
    ///   [`ProtocolMetadata`](super::ProtocolMetadata) name different venues —
    ///   see [`PoolRegistration::check_protocol_agreement`]. This is the gate
    ///   that keeps a cross-venue registration out of the system: adapters
    ///   dispatch on the key while storage layouts resolve from the metadata, so
    ///   admitting one would quote a pool through one venue's ABI against
    ///   another venue's storage. The registration is rejected, never repaired —
    ///   which of the two venues the caller meant is not knowable here.
    pub fn register_pool(&mut self, registration: PoolRegistration) -> Result<(), RegistryError> {
        if self.pools.contains_key(&registration.key) {
            return Err(RegistryError::DuplicatePool(registration.key));
        }
        registration.check_protocol_agreement()?;

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
        self.quote_read_sets.retain(|warmup, _| &warmup.pool != key);
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
        Ok(self.unregister_adapter_prevalidated(protocol))
    }

    /// Detach an adapter family after the caller has proved it owns no pools.
    ///
    /// This crate-private commit primitive deliberately performs no pool scan;
    /// transactional lifecycle code preflights dependency ownership once, then
    /// uses the same infallible detach in the primary and routing registries.
    pub(crate) fn unregister_adapter_prevalidated(
        &mut self,
        protocol: ProtocolId,
    ) -> Option<Arc<dyn AmmAdapter>> {
        let adapter = self.adapters.get(&protocol).cloned()?;
        for id in adapter.protocols() {
            self.adapters.remove(&id);
        }
        Some(adapter)
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
        self.quote_read_sets.retain(|warmup, _| &warmup.pool != key);
        self.pools.get_mut(key)
    }

    /// Change only a pool's lifecycle status without invalidating learned quote
    /// dependencies.
    ///
    /// Status transitions do not change code, metadata, storage layout, or
    /// routing identity, so the representative quote manifests remain valid.
    /// Full registration replacements continue to use [`Self::pool_mut`] and
    /// invalidate them conservatively.
    pub(crate) fn update_pool_status(
        &mut self,
        key: &PoolKey,
        status: super::PoolStatus,
    ) -> Option<PoolRegistration> {
        let registration = self.pools.get_mut(key)?;
        registration.status = status;
        Some(registration.clone())
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

        // A family adapter is stored under several ids; consult its `route_log`
        // only at its first occurrence. Checking earlier entries by pointer
        // (the adapter map holds at most a handful of families) avoids
        // allocating a dedup set on every log that reaches this fallback.
        for (index, adapter) in self.adapters.values().enumerate() {
            let first_occurrence = !self
                .adapters
                .values()
                .take(index)
                .any(|earlier| Arc::ptr_eq(earlier, adapter));
            if !first_occurrence {
                continue;
            }

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

    /// The event sources to subscribe/route for `pool`: its stored
    /// `event_sources` unioned with any its adapter derives via
    /// [`AmmAdapter::event_sources`](super::AmmAdapter::event_sources),
    /// de-duplicated. This is the exact set [`AmmReactiveHandler`] subscribes to,
    /// so the public subscription helpers below reflect the full adapter+pool
    /// universe rather than only the sources persisted on the registration.
    ///
    /// [`AmmReactiveHandler`]: super::AmmReactiveHandler
    pub(crate) fn event_sources_for(&self, pool: &PoolRegistration) -> Vec<EventSource> {
        let mut sources = pool.event_sources.clone();
        if let Some(adapter) = self.adapter(pool.protocol()) {
            for source in adapter.event_sources(pool) {
                if !sources.contains(&source) {
                    sources.push(source);
                }
            }
        }
        sources
    }

    /// The sorted, de-duplicated set of `topic0`s across every tracked pool's
    /// event sources — stored *and* adapter-derived (see `event_sources_for`) —
    /// as a log-subscription filter.
    pub fn subscription_topics(&self) -> Vec<B256> {
        let mut topics: Vec<B256> = self
            .subscription_spec()
            .sources
            .iter()
            .flat_map(|source| source.topics.iter().copied())
            .collect();

        topics.sort_unstable_by(|a, b| a.as_slice().cmp(b.as_slice()));
        topics.dedup();
        topics
    }

    /// The full [`SubscriptionSpec`]: every tracked pool's event sources,
    /// including the adapter-derived sources (see `event_sources_for`) that the
    /// reactive handler subscribes to — not only the sources persisted on each
    /// registration.
    pub fn subscription_spec(&self) -> SubscriptionSpec {
        SubscriptionSpec {
            sources: self
                .pools
                .values()
                .flat_map(|pool| self.event_sources_for(pool))
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
    /// The registration's pool key and protocol metadata name different venues
    /// — see [`PoolRegistration::check_protocol_agreement`].
    ProtocolMismatch(ProtocolMismatch),
}

impl From<ProtocolMismatch> for RegistryError {
    fn from(mismatch: ProtocolMismatch) -> Self {
        Self::ProtocolMismatch(mismatch)
    }
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
            Self::ProtocolMismatch(mismatch) => write!(f, "{mismatch}"),
        }
    }
}

impl std::error::Error for RegistryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::ProtocolMismatch(mismatch) => Some(mismatch),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::storage::V3StorageLayout;
    use crate::adapters::{CustomPoolKey, ProtocolMetadata, V3Metadata};

    fn addr(byte: u8) -> Address {
        Address::repeat_byte(byte)
    }

    /// The gate: a cross-venue registration is refused, and the registry is left
    /// exactly as it was. Rejecting without inserting is the whole point —
    /// admitting the pool is what let the wrong-ABI quote happen downstream.
    #[test]
    fn register_pool_rejects_a_cross_venue_registration_without_inserting() {
        let key = PoolKey::UniswapV3(addr(0x11));
        let registration = PoolRegistration::new(key.clone()).with_metadata(
            ProtocolMetadata::Slipstream(V3Metadata::default().with_tick_spacing(100)),
        );

        let mut registry = AdapterRegistry::new();
        let error = registry
            .register_pool(registration)
            .expect_err("a Uniswap V3 key carrying Slipstream metadata must be refused");

        match error {
            RegistryError::ProtocolMismatch(mismatch) => {
                assert_eq!(mismatch.key, key);
                assert_eq!(mismatch.key_protocol, ProtocolId::UniswapV3);
                assert_eq!(mismatch.metadata_protocol, ProtocolId::Slipstream);
            }
            other => panic!("expected ProtocolMismatch, got {other:?}"),
        }

        assert!(registry.pool(&key).is_none(), "rejection must not insert");
        assert!(registry.is_empty());
    }

    /// A rejected registration must not disturb one already tracked, and must not
    /// consume the key.
    #[test]
    fn rejection_leaves_an_existing_registration_intact() {
        let good_key = PoolKey::Slipstream(addr(0x21));
        let good = PoolRegistration::new(good_key.clone()).with_metadata(
            ProtocolMetadata::Slipstream(V3Metadata::default().with_tick_spacing(100)),
        );
        let mut registry = AdapterRegistry::new();
        registry.register_pool(good).expect("valid registration");

        let bad_key = PoolKey::UniswapV3(addr(0x22));
        assert!(
            registry
                .register_pool(
                    PoolRegistration::new(bad_key.clone())
                        .with_metadata(ProtocolMetadata::PancakeV3(V3Metadata::default()))
                )
                .is_err()
        );

        assert_eq!(registry.len(), 1);
        assert!(registry.pool(&good_key).is_some());
        assert!(registry.pool(&bad_key).is_none());

        // The key is still free: the caller can fix the metadata and retry.
        registry
            .register_pool(
                PoolRegistration::new(bad_key.clone())
                    .with_metadata(ProtocolMetadata::UniswapV3(V3Metadata::default())),
            )
            .expect("the corrected registration must be accepted");
        assert!(registry.pool(&bad_key).is_some());
    }

    /// Registrations that must keep registering: the pre-cold-start `Unknown`
    /// default, a matching family pair, a deliberate non-default storage layout,
    /// and the third-party `Custom` hatch.
    #[test]
    fn legitimate_registrations_still_register() {
        let mut registry = AdapterRegistry::new();

        registry
            .register_pool(PoolRegistration::new(PoolKey::UniswapV3(addr(0x31))))
            .expect("the `new` default carries Unknown metadata");

        registry
            .register_pool(
                PoolRegistration::new(PoolKey::Slipstream(addr(0x32))).with_metadata(
                    ProtocolMetadata::Slipstream(V3Metadata::default().with_tick_spacing(200)),
                ),
            )
            .expect("a matching family pair");

        registry
            .register_pool(
                PoolRegistration::new(PoolKey::UniswapV3(addr(0x33))).with_metadata(
                    ProtocolMetadata::UniswapV3(
                        V3Metadata::default()
                            .with_tick_spacing(100)
                            // A fork's own slots, deliberately supplied under the
                            // matching family variant.
                            .with_storage_layout(V3StorageLayout::slipstream(100)),
                    ),
                ),
            )
            .expect("an explicit non-default storage layout is a legitimate override");

        registry
            .register_pool(
                PoolRegistration::new(PoolKey::Custom(CustomPoolKey::Address {
                    protocol: "acme-v1",
                    address: addr(0x34),
                }))
                .with_metadata(ProtocolMetadata::Custom(std::sync::Arc::new(0u8))),
            )
            .expect("the third-party extension hatch");

        assert_eq!(registry.len(), 4);
    }

    /// A duplicate key is still reported as a duplicate, not misreported as a
    /// mismatch.
    #[test]
    fn duplicate_detection_is_unchanged() {
        let key = PoolKey::UniswapV3(addr(0x41));
        let mut registry = AdapterRegistry::new();
        registry
            .register_pool(PoolRegistration::new(key.clone()))
            .expect("first registration");

        assert_eq!(
            registry.register_pool(PoolRegistration::new(key.clone())),
            Err(RegistryError::DuplicatePool(key)),
        );
    }
}
