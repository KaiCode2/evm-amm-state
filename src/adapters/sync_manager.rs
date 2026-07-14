//! AMM-owned live sync driver around the generic reactive runtime.
//!
//! `evm-fork-cache` owns the expensive part of resync execution: grouping
//! [`ResyncRequest`]s by block,
//! resolving them from block traces when possible, falling back to storage
//! fetchers, and applying authoritative values to
//! [`EvmCache`]. This module keeps the AMM-specific part local to this crate:
//! callers get a runtime that always executes resyncs, and failed storage repairs
//! are translated back into pool lifecycle status.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt;
use std::sync::Arc;

use alloy_network::Ethereum;
use alloy_primitives::{Address, U256};
use evm_fork_cache::cache::EvmCache;
use evm_fork_cache::reactive::{
    HandlerId, ReactiveBatchReport, ReactiveConfig, ReactiveError, ReactiveInputBatch,
    ReactiveInterest, ReactiveReport, ReactiveRuntime, RegisterError, ResyncFailure, ResyncId,
    ResyncRequest, ResyncTarget,
};

use super::{
    AdapterCache, AdapterEvent, AdapterGeneration, AdapterInstanceId, AdapterKey, AdapterRegistry,
    AmmAdapter, AmmChangeImpact, AmmOwnershipError, AmmOwnershipIndex, AmmPoolReactiveHandler,
    AmmPoolReactiveHandlerError, AmmReactiveRoutingContext, AmmReactiveSignal, OwnerRuntimeState,
    PoolGeneration, PoolInstanceId, PoolKey, PoolOwnership, PoolRegistration, PoolRuntimeState,
    PoolStateDependencies, PoolStatus, RegistryError, RepairAction, RuntimeLifecycleMap,
    RuntimeOwnerId, RuntimeSequenceOverflow, RuntimeWorkId, StateSlot, UpdateQuality,
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
    /// A pool-scoped reactive handler could not be constructed.
    PoolHandler(AmmPoolReactiveHandlerError),
    /// Runtime resource ownership could not be indexed consistently.
    Ownership(AmmOwnershipError),
    /// A generation counter was exhausted.
    Sequence(RuntimeSequenceOverflow),
    /// A generation-scoped handler expected by lifecycle ownership was absent.
    MissingPoolHandler(HandlerId),
    /// Internal registry, routing, and ownership views disagreed during commit.
    LifecycleInvariant(&'static str),
    /// A future generation is already reserved for this logical pool.
    PoolReservationConflict(PoolInstanceId),
    /// A reservation token no longer names the currently reserved generation.
    StalePoolReservation(PoolInstanceId),
    /// A registration was presented for a different logical pool than its reservation.
    PoolReservationKeyMismatch(PoolInstanceId),
    /// Only a ready registration may become active through a reservation commit.
    PoolReservationNotReady(PoolKey),
    /// A refresh token does not name the currently active pool generation.
    StalePoolRefresh(PoolInstanceId),
    /// A refresh replacement has a different logical key than its target generation.
    PoolRefreshKeyMismatch(PoolInstanceId),
    /// Only a ready replacement may be committed through the refresh seam.
    PoolRefreshNotReady(PoolKey),
}

impl fmt::Display for AmmSyncError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Register(err) => write!(f, "failed to register AMM sync handler: {err}"),
            Self::Reactive(err) => write!(f, "AMM reactive ingest failed: {err}"),
            Self::Registry(err) => write!(f, "AMM registry mutation failed: {err}"),
            Self::PoolHandler(err) => write!(f, "failed to build AMM pool handler: {err}"),
            Self::Ownership(err) => write!(f, "failed to index AMM ownership: {err}"),
            Self::Sequence(err) => write!(f, "failed to allocate AMM runtime identity: {err}"),
            Self::MissingPoolHandler(handler) => {
                write!(f, "AMM lifecycle handler is missing: {handler}")
            }
            Self::LifecycleInvariant(message) => {
                write!(f, "AMM lifecycle invariant failed: {message}")
            }
            Self::PoolReservationConflict(instance) => {
                write!(f, "AMM pool generation is already reserved: {instance:?}")
            }
            Self::StalePoolReservation(instance) => {
                write!(f, "AMM pool generation reservation is stale: {instance:?}")
            }
            Self::PoolReservationKeyMismatch(reservation) => write!(
                f,
                "AMM pool reservation does not match the registration: {reservation:?}"
            ),
            Self::PoolReservationNotReady(key) => {
                write!(f, "AMM reserved pool registration is not ready: {key:?}")
            }
            Self::StalePoolRefresh(instance) => {
                write!(f, "AMM pool refresh generation is stale: {instance:?}")
            }
            Self::PoolRefreshKeyMismatch(instance) => write!(
                f,
                "AMM pool refresh does not match the replacement registration: {instance:?}"
            ),
            Self::PoolRefreshNotReady(key) => {
                write!(f, "AMM pool refresh registration is not ready: {key:?}")
            }
        }
    }
}

impl std::error::Error for AmmSyncError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Register(err) => Some(err),
            Self::Reactive(err) => Some(err),
            Self::Registry(err) => Some(err),
            Self::PoolHandler(err) => Some(err),
            Self::Ownership(err) => Some(err),
            Self::Sequence(err) => Some(err),
            Self::MissingPoolHandler(_) => None,
            Self::LifecycleInvariant(_) => None,
            Self::PoolReservationConflict(_)
            | Self::StalePoolReservation(_)
            | Self::PoolReservationKeyMismatch(_)
            | Self::PoolReservationNotReady(_)
            | Self::StalePoolRefresh(_)
            | Self::PoolRefreshKeyMismatch(_)
            | Self::PoolRefreshNotReady(_) => None,
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

impl From<AmmPoolReactiveHandlerError> for AmmSyncError {
    fn from(err: AmmPoolReactiveHandlerError) -> Self {
        Self::PoolHandler(err)
    }
}

impl From<AmmOwnershipError> for AmmSyncError {
    fn from(err: AmmOwnershipError) -> Self {
        Self::Ownership(err)
    }
}

impl From<RuntimeSequenceOverflow> for AmmSyncError {
    fn from(err: RuntimeSequenceOverflow) -> Self {
        Self::Sequence(err)
    }
}

/// Cache treatment requested by a lifecycle removal.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum AmmEvictionPolicy {
    /// Keep warmed state available for reuse and re-registration.
    #[default]
    Retain,
    /// Purge only state proven exclusive to removed pools.
    Exclusive,
}

/// Deterministic cache work performed by one lifecycle transaction.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AmmEvictionReport {
    policy: AmmEvictionPolicy,
    purged_accounts: Vec<Address>,
    purged_slots: Vec<StateSlot>,
}

impl AmmEvictionReport {
    /// Requested cache treatment.
    pub const fn policy(&self) -> AmmEvictionPolicy {
        self.policy
    }

    /// Canonically ordered whole-account purges.
    pub fn purged_accounts(&self) -> &[Address] {
        &self.purged_accounts
    }

    /// Canonically ordered exact slot purges.
    pub fn purged_slots(&self) -> &[StateSlot] {
        &self.purged_slots
    }
}

/// Pool resources detached by one synchronous lifecycle transaction.
#[derive(Clone, Debug)]
pub struct AmmRemovedPool {
    instance: PoolInstanceId,
    registration: PoolRegistration,
    cancelled_work: Vec<RuntimeWorkId>,
    detached_resyncs: Vec<ResyncId>,
    cancelled_resyncs: Vec<ResyncRequest>,
}

impl AmmRemovedPool {
    /// Removed generation-scoped pool identity.
    pub const fn instance(&self) -> &PoolInstanceId {
        &self.instance
    }

    /// Removed registry entry.
    pub const fn registration(&self) -> &PoolRegistration {
        &self.registration
    }

    /// Generation-owned work rejected by removal.
    pub fn cancelled_work(&self) -> &[RuntimeWorkId] {
        &self.cancelled_work
    }

    /// Generation-owned request ids detached from the ownership index.
    pub fn detached_resyncs(&self) -> &[ResyncId] {
        &self.detached_resyncs
    }

    /// Still-queued upstream requests cancelled by exact id.
    pub fn cancelled_resyncs(&self) -> &[ResyncRequest] {
        &self.cancelled_resyncs
    }
}

/// Typed result of one atomic synchronous lifecycle transaction.
#[derive(Clone, Debug, Default)]
pub struct AmmLifecycleReport {
    registered_pools: Vec<PoolInstanceId>,
    removed_pools: Vec<AmmRemovedPool>,
    registered_adapters: Vec<AdapterInstanceId>,
    removed_adapters: Vec<AdapterInstanceId>,
    cancelled_adapter_work: Vec<RuntimeWorkId>,
    eviction: AmmEvictionReport,
}

impl AmmLifecycleReport {
    /// Newly accepted pool generations, in logical-key order.
    pub fn registered_pools(&self) -> &[PoolInstanceId] {
        &self.registered_pools
    }

    /// Removed pool generations, in logical-key order.
    pub fn removed_pools(&self) -> &[AmmRemovedPool] {
        &self.removed_pools
    }

    /// Newly accepted adapter-family generations.
    pub fn registered_adapters(&self) -> &[AdapterInstanceId] {
        &self.registered_adapters
    }

    /// Removed adapter-family generations.
    pub fn removed_adapters(&self) -> &[AdapterInstanceId] {
        &self.removed_adapters
    }

    /// Work detached from removed adapter-family generations.
    pub fn cancelled_adapter_work(&self) -> &[RuntimeWorkId] {
        &self.cancelled_adapter_work
    }

    /// Cache treatment performed by the transaction.
    pub const fn eviction(&self) -> &AmmEvictionReport {
        &self.eviction
    }
}

/// Exact generation-scoped subscriber adoption required by a future pool commit.
#[derive(Clone, Debug)]
pub struct AmmPoolSubscriptionPlan {
    instance: PoolInstanceId,
    handler: HandlerId,
    interests: Vec<ReactiveInterest<Ethereum>>,
}

/// Opaque claim on one future pool generation.
///
/// Reserving allocates and tombstone-protects the generation without installing
/// registry, reactive-handler, or ownership state. Clones identify the same
/// claim; an engine accepts it only while the matching claim is outstanding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AmmPoolGenerationReservation {
    instance: PoolInstanceId,
}

impl AmmPoolGenerationReservation {
    /// Exact generation that a successful commit will install.
    pub const fn instance(&self) -> &PoolInstanceId {
        &self.instance
    }
}

impl AmmPoolSubscriptionPlan {
    /// Pool generation that the corresponding lifecycle commit will accept.
    pub const fn instance(&self) -> &PoolInstanceId {
        &self.instance
    }

    /// Exact generation-scoped owner installed in the subscriber.
    pub const fn handler(&self) -> &HandlerId {
        &self.handler
    }

    /// Provider and local-routing interests owned by this generation.
    pub fn interests(&self) -> &[ReactiveInterest<Ethereum>] {
        &self.interests
    }
}

/// Prevalidated same-generation pool refresh awaiting subscriber fencing.
#[derive(Debug)]
pub struct AmmPreparedPoolRefresh {
    instance: PoolInstanceId,
    previous_registration: PoolRegistration,
    replacement_registration: PoolRegistration,
    previous_ownership: PoolOwnership,
    replacement_ownership: PoolOwnership,
    replacement_handler: Arc<AmmPoolReactiveHandler>,
    previous_subscription: AmmPoolSubscriptionPlan,
    replacement_subscription: AmmPoolSubscriptionPlan,
}

impl AmmPreparedPoolRefresh {
    /// Exact active generation this refresh may update.
    pub const fn instance(&self) -> &PoolInstanceId {
        &self.instance
    }

    /// Registration visible when the refresh was prepared.
    pub const fn previous_registration(&self) -> &PoolRegistration {
        &self.previous_registration
    }

    /// Ready registration that will replace the previous metadata.
    pub const fn replacement_registration(&self) -> &PoolRegistration {
        &self.replacement_registration
    }

    /// Subscriber owner and interests currently installed for the generation.
    pub const fn previous_subscription(&self) -> &AmmPoolSubscriptionPlan {
        &self.previous_subscription
    }

    /// Subscriber owner and interests required by the replacement registration.
    pub const fn replacement_subscription(&self) -> &AmmPoolSubscriptionPlan {
        &self.replacement_subscription
    }
}

/// Result of one committed same-generation pool refresh.
#[derive(Clone, Debug)]
pub struct AmmPoolRefreshReport {
    instance: PoolInstanceId,
    previous_registration: PoolRegistration,
    replacement_registration: PoolRegistration,
    previous_subscription: AmmPoolSubscriptionPlan,
    replacement_subscription: AmmPoolSubscriptionPlan,
}

impl AmmPoolRefreshReport {
    /// Exact pool generation that remained active across the refresh.
    pub const fn instance(&self) -> &PoolInstanceId {
        &self.instance
    }

    /// Registration replaced by the commit.
    pub const fn previous_registration(&self) -> &PoolRegistration {
        &self.previous_registration
    }

    /// Registration visible after the commit.
    pub const fn replacement_registration(&self) -> &PoolRegistration {
        &self.replacement_registration
    }

    /// Subscriber owner and interests installed before the commit.
    pub const fn previous_subscription(&self) -> &AmmPoolSubscriptionPlan {
        &self.previous_subscription
    }

    /// Subscriber owner and interests required after the commit.
    pub const fn replacement_subscription(&self) -> &AmmPoolSubscriptionPlan {
        &self.replacement_subscription
    }
}

struct PreparedPoolAddition {
    registration: PoolRegistration,
    instance: PoolInstanceId,
    ownership: PoolOwnership,
    handler: Arc<AmmPoolReactiveHandler>,
    lifecycle: PoolRuntimeState,
}

impl PreparedPoolAddition {
    fn subscription_plan(&self) -> AmmPoolSubscriptionPlan {
        AmmPoolSubscriptionPlan {
            instance: self.instance.clone(),
            handler: self.ownership.handler().clone(),
            interests: self.handler.interests(),
        }
    }
}

#[derive(Clone, Debug, Default)]
struct RuntimeGenerationLedger {
    pools: BTreeMap<PoolKey, PoolGeneration>,
    adapters: BTreeMap<AdapterKey, AdapterGeneration>,
}

impl RuntimeGenerationLedger {
    fn next_pool(&self, key: &PoolKey) -> Result<PoolGeneration, RuntimeSequenceOverflow> {
        self.pools
            .get(key)
            .copied()
            .map_or(Ok(PoolGeneration::new(0)), PoolGeneration::checked_next)
    }

    fn next_adapter(&self, key: &AdapterKey) -> Result<AdapterGeneration, RuntimeSequenceOverflow> {
        self.adapters.get(key).copied().map_or(
            Ok(AdapterGeneration::new(0)),
            AdapterGeneration::checked_next,
        )
    }
}

/// Final AMM-specific effect of one synchronous ingest batch on a pool.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum AmmSyncPoolChangeKind {
    /// Quote-relevant state changed.
    Updated,
    /// The pool became unavailable for normal search policy.
    Degraded,
    /// The pool recovered normal search eligibility.
    Recovered,
    /// A routed event had an impact the synchronous path could not prove.
    UnknownImpact,
}

/// Provenance of a synchronous pool-state change.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum AmmSyncChangeSource {
    /// State was applied directly from a decoded event.
    Direct,
    /// State came from an authoritative resync completed by this ingest call.
    AuthoritativeResync,
    /// Both direct event effects and authoritative resync contributed.
    DirectAndAuthoritativeResync,
    /// Only pool eligibility/lifecycle status changed.
    LifecycleOnly,
    /// The synchronous path cannot prove the complete impact.
    Unknown,
}

impl AmmSyncChangeSource {
    fn with_direct(self) -> Self {
        match self {
            Self::AuthoritativeResync | Self::DirectAndAuthoritativeResync => {
                Self::DirectAndAuthoritativeResync
            }
            Self::Direct | Self::LifecycleOnly => Self::Direct,
            Self::Unknown => Self::Unknown,
        }
    }

    fn with_authoritative_resync(self) -> Self {
        match self {
            Self::Direct | Self::DirectAndAuthoritativeResync => Self::DirectAndAuthoritativeResync,
            Self::AuthoritativeResync | Self::LifecycleOnly => Self::AuthoritativeResync,
            Self::Unknown => Self::Unknown,
        }
    }
}

/// One logical pool's deterministic post-resync change from synchronous ingest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AmmSyncPoolChange {
    pool: PoolKey,
    kind: AmmSyncPoolChangeKind,
    impact: AmmChangeImpact,
    source: AmmSyncChangeSource,
}

/// Canonical continuity incident observed during synchronous ingest.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum AmmSyncIncident {
    /// Previously canonical blocks were dropped.
    Reorg {
        /// Dropped blocks in upstream journal order.
        dropped_blocks: Vec<evm_fork_cache::reactive::BlockRef>,
    },
    /// A forward block range was not observed.
    Gap {
        /// First missed block, inclusive.
        from: u64,
        /// Last missed block, inclusive.
        to: u64,
    },
    /// A tracked account root changed without a covering decoder.
    CoverageGap {
        /// Account with an unknown changed slot.
        address: Address,
        /// Block at which the gap was detected.
        block: u64,
    },
}

impl AmmSyncPoolChange {
    /// Construct a synchronous pool change.
    pub const fn new(pool: PoolKey, kind: AmmSyncPoolChangeKind, impact: AmmChangeImpact) -> Self {
        Self {
            pool,
            kind,
            impact,
            source: AmmSyncChangeSource::Direct,
        }
    }

    /// Construct a synchronous pool change with explicit provenance.
    pub const fn with_source(
        pool: PoolKey,
        kind: AmmSyncPoolChangeKind,
        impact: AmmChangeImpact,
        source: AmmSyncChangeSource,
    ) -> Self {
        Self {
            pool,
            kind,
            impact,
            source,
        }
    }

    /// Logical affected pool.
    pub const fn pool(&self) -> &PoolKey {
        &self.pool
    }

    /// Final change kind after direct effects and repairs.
    pub const fn kind(&self) -> AmmSyncPoolChangeKind {
        self.kind
    }

    /// Search-facing impact.
    pub const fn impact(&self) -> AmmChangeImpact {
        self.impact
    }

    /// Provenance of the final synchronous state/lifecycle effect.
    pub const fn source(&self) -> AmmSyncChangeSource {
        self.source
    }
}

/// Generation-scoped adapter repair surfaced by synchronous ingestion.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct AmmPendingRepair {
    pool: PoolInstanceId,
    action: RepairAction,
}

impl AmmPendingRepair {
    fn new(pool: PoolInstanceId, action: RepairAction) -> Self {
        Self { pool, action }
    }

    /// Exact pool generation that emitted the repair.
    pub const fn pool(&self) -> &PoolInstanceId {
        &self.pool
    }

    /// Adapter follow-up work requested for the pool.
    pub const fn action(&self) -> &RepairAction {
        &self.action
    }
}

/// Summary returned by [`AmmSyncEngine::ingest_batch`].
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct AmmSyncBatchReport {
    /// Full upstream reactive report, including applied effects and resync
    /// details.
    pub reactive: ReactiveBatchReport<Ethereum>,
    /// Canonically ordered pools whose state or search eligibility changed.
    ///
    /// Attribution is derived after direct effects, authoritative resyncs, and
    /// degraded/recovered status transitions have all completed.
    pub affected_pools: Vec<PoolKey>,
    /// Canonically ordered final AMM-specific changes for affected pools.
    ///
    /// These changes intentionally carry logical [`PoolKey`] values rather
    /// than runtime generations/versions; the compatibility sync engine is not
    /// the authoritative lifecycle allocator or post-block commit fence.
    pub pool_changes: Vec<AmmSyncPoolChange>,
    /// Canonical continuity/coverage incidents observed during this ingest call.
    pub incidents: Vec<AmmSyncIncident>,
    /// Whether attribution is necessarily conservative and all tracked state
    /// must be refreshed before trust is restored.
    pub requires_full_refresh: bool,
    /// Pools marked degraded because a required repair failed or was not
    /// executable, a routed event could not be decoded, continuity was lost,
    /// or tracked-account coverage became uncertain.
    pub degraded_pools: Vec<PoolKey>,
    /// Previously-degraded pools flipped back to [`PoolStatus::Ready`] this batch
    /// because the resync phase refreshed the state they were waiting on and none
    /// of their targets failed again. Recovery is target-aware: a pool degraded by
    /// a tracked resync failure needs the specific slots it failed on refreshed
    /// (not merely any write at its address). An untracked cold-start degradation
    /// requires every slot in an enumerable Curve/Balancer read set. Unknown
    /// impact, continuity loss, account-target failure, and address-wide pools
    /// require an explicit authoritative refresh/registry replacement.
    pub recovered_pools: Vec<PoolKey>,
    /// Number of authoritative state updates produced by executed resync reports.
    pub resync_state_updates: usize,
    /// Number of failed resync targets reported by the runtime.
    pub resync_failures: usize,
    /// Canonically ordered generation-scoped repair work surfaced by adapters.
    /// Repeated signals from one exact pool generation are combined into one
    /// action using [`RepairAction`]'s normal merge semantics.
    pub pending_repairs: Vec<AmmPendingRepair>,
}

impl AmmSyncBatchReport {
    /// Typed adapter repair signals available to higher-level orchestration.
    ///
    /// This is additive observability: repair effects already lowered into the
    /// synchronous reactive runtime still execute normally. A scheduler should
    /// route only the variants it owns, such as [`RepairAction::ColdStart`].
    pub fn pending_repairs(&self) -> &[AmmPendingRepair] {
        &self.pending_repairs
    }
}

/// Live AMM sync driver.
///
/// This is a convenience owner for the intended live path:
///
/// 1. route AMM logs through one [`AmmPoolReactiveHandler`] per pool,
/// 2. execute every emitted resync via
///    [`ReactiveRuntime::ingest_batch_with_resync`],
/// 3. keep AMM pool status in sync with resync failures.
pub struct AmmSyncEngine {
    registry: AdapterRegistry,
    ownership: AmmOwnershipIndex,
    runtime: ReactiveRuntime<Ethereum>,
    routing: AmmReactiveRoutingContext,
    generations: RuntimeGenerationLedger,
    reservations: BTreeMap<PoolKey, PoolInstanceId>,
    lifecycles: RuntimeLifecycleMap,
    config: ReactiveConfig,
    /// Explicitly evicted pool generations whose effects still occur in the
    /// bounded upstream rollback journal. The fence re-applies ownership-aware
    /// eviction after each ingest until that generation ages out or is rolled
    /// back, preventing a late reorg from rematerializing purged cache state.
    eviction_fences: Vec<PoolOwnership>,
    /// Outstanding recovery requirements for degraded pools. Failed repairs
    /// retain their exact targets; unknown-impact and continuity failures retain
    /// an explicit-refresh barrier that ordinary event resyncs cannot clear.
    degraded_targets: HashMap<PoolKey, PendingTargets>,
    degraded_pool_count: usize,
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
        let (ownership, generations) =
            ownership_for_registry(&registry, &RuntimeGenerationLedger::default())?;
        let routing = AmmReactiveRoutingContext::new(Arc::new(registry.clone()));
        let runtime = runtime_for(&ownership, routing.clone(), config.clone())?;
        let lifecycles = lifecycle_map_for(&registry, &ownership);
        let degraded_pool_count = registry
            .pools()
            .filter(|pool| pool.status == PoolStatus::Degraded)
            .count();
        Ok(Self {
            registry,
            ownership,
            runtime,
            routing,
            generations,
            reservations: BTreeMap::new(),
            lifecycles,
            config,
            eviction_fences: Vec::new(),
            degraded_targets: HashMap::new(),
            degraded_pool_count,
        })
    }

    /// Current pool registry.
    pub fn registry(&self) -> &AdapterRegistry {
        &self.registry
    }

    /// Number of active registrations whose current status is degraded.
    pub const fn degraded_pool_count(&self) -> usize {
        self.degraded_pool_count
    }

    /// Whether an active degraded registration exists other than `excluded`.
    pub fn has_other_degraded_pool(&self, excluded: &PoolKey) -> bool {
        let excluded_is_degraded = self
            .registry
            .pool(excluded)
            .is_some_and(|pool| pool.status == PoolStatus::Degraded);
        self.degraded_pool_count > usize::from(excluded_is_degraded)
    }

    /// Generation-scoped state, handler, adapter, emitter, work, and resync ownership.
    pub const fn ownership(&self) -> &AmmOwnershipIndex {
        &self.ownership
    }

    /// Mutable ownership access for the serialized actor lifecycle coordinator.
    #[cfg(feature = "live-runtime")]
    pub(crate) fn ownership_mut(&mut self) -> &mut AmmOwnershipIndex {
        &mut self.ownership
    }

    /// Latest lifecycle state for every accepted pool and adapter generation.
    ///
    /// Removed generations remain as tombstones so stale work can be rejected
    /// without aliasing a later registration of the same logical key.
    pub const fn lifecycles(&self) -> &RuntimeLifecycleMap {
        &self.lifecycles
    }

    /// Acknowledge continuous canonical delivery for exact pool generations.
    ///
    /// This is the subscriber-to-engine half of the `Searchable -> Live`
    /// transition. The complete batch is prevalidated before any lifecycle is
    /// changed, stale generations are rejected, and repeated acknowledgement
    /// of an already-live generation is idempotent.
    pub fn acknowledge_live_delivery(
        &mut self,
        pools: &[PoolInstanceId],
    ) -> Result<Vec<PoolInstanceId>, AmmSyncError> {
        let pools: BTreeSet<_> = pools.iter().cloned().collect();
        let mut transitions = Vec::new();
        for pool in &pools {
            if self.ownership.active_pool(pool.key()) != Some(pool) {
                return Err(AmmSyncError::LifecycleInvariant(
                    "subscriber acknowledged a stale pool generation",
                ));
            }
            match self.lifecycles.pool(pool) {
                Some(PoolRuntimeState::Searchable) => transitions.push(pool.clone()),
                Some(PoolRuntimeState::Live) => {}
                Some(_) => {
                    return Err(AmmSyncError::LifecycleInvariant(
                        "subscriber acknowledged a pool that was not searchable",
                    ));
                }
                None => {
                    return Err(AmmSyncError::LifecycleInvariant(
                        "subscriber acknowledged a pool absent from lifecycle state",
                    ));
                }
            }
        }
        for pool in &transitions {
            self.lifecycles
                .set_pool(pool.clone(), PoolRuntimeState::Live);
        }
        Ok(transitions)
    }

    /// Current reactive runtime.
    pub fn runtime(&self) -> &ReactiveRuntime<Ethereum> {
        &self.runtime
    }

    /// Replace the registry and rebuild the underlying handler registration.
    ///
    /// Pool-scoped handlers own immutable registration/dependency snapshots, so
    /// callers that add pools or update read-set metadata should replace the
    /// registry through this method rather than mutating a detached clone.
    ///
    /// Rebuilding is not free: the engine constructs a **fresh**
    /// [`ReactiveRuntime`], so any state the previous runtime accumulated
    /// across batches (reorg tracking, pending work) is discarded. Call this
    /// between batches — never mid-stream. Every accepted member receives the
    /// checked successor of its retained generation, so this compatibility
    /// escape hatch cannot alias work from the discarded runtime. Ordinary
    /// lifecycle changes should use [`add_pools`](Self::add_pools),
    /// [`remove_pools`](Self::remove_pools), and the adapter lifecycle methods.
    pub fn replace_registry(&mut self, registry: AdapterRegistry) -> Result<(), AmmSyncError> {
        if !self.reservations.is_empty() {
            return Err(AmmSyncError::LifecycleInvariant(
                "cannot replace the registry while pool generations are reserved",
            ));
        }
        let (ownership, generations) = ownership_for_registry(&registry, &self.generations)?;
        let routing = AmmReactiveRoutingContext::new(Arc::new(registry.clone()));
        let runtime = runtime_for(&ownership, routing.clone(), self.config.clone())?;
        let mut lifecycles = self.lifecycles.clone();
        for pool in self.ownership.pools() {
            lifecycles.set_pool(pool.clone(), PoolRuntimeState::Removed);
        }
        for adapter in self.ownership.adapters() {
            lifecycles.set_adapter(adapter.clone(), OwnerRuntimeState::Removed);
        }
        for (pool, state) in lifecycle_map_for(&registry, &ownership).pools() {
            lifecycles.set_pool(pool.clone(), state);
        }
        for (adapter, state) in lifecycle_map_for(&registry, &ownership).adapters() {
            lifecycles.set_adapter(adapter.clone(), state);
        }
        let degraded_pool_count = registry
            .pools()
            .filter(|pool| pool.status == PoolStatus::Degraded)
            .count();
        self.registry = registry;
        self.ownership = ownership;
        self.runtime = runtime;
        self.routing = routing;
        self.degraded_pool_count = degraded_pool_count;
        self.generations = generations;
        self.lifecycles = lifecycles;
        self.eviction_fences.clear();
        // Every replacement receives a fresh generation. Exact recovery
        // targets and explicit-refresh fences belong to discarded generations
        // and must never alias the replacement, even when its caller-supplied
        // status is also Degraded.
        self.degraded_targets.clear();
        Ok(())
    }

    /// Register additional pools into the live engine.
    ///
    /// Compatibility wrapper over [`add_pools`](Self::add_pools): on a duplicate
    /// or invalid batch nothing changes, and accepted handlers are installed in
    /// the existing runtime without losing journals or pending work.
    ///
    /// Registration alone warms no state: cold-start the registrations first
    /// (or supply explicit read-set metadata), and widen the consumer-owned
    /// provider subscription if it filters by address.
    pub fn register_pools(
        &mut self,
        pools: impl IntoIterator<Item = PoolRegistration>,
    ) -> Result<(), AmmSyncError> {
        self.add_pools(pools).map(|_| ())
    }

    /// Atomically add pools without reconstructing the reactive runtime.
    ///
    /// Existing journal, hook, health, freshness, and pending-work state is
    /// retained. The returned identities are generation scoped; a key removed
    /// and later re-added receives the checked successor generation.
    pub fn add_pools(
        &mut self,
        pools: impl IntoIterator<Item = PoolRegistration>,
    ) -> Result<AmmLifecycleReport, AmmSyncError> {
        let registrations: Vec<_> = pools.into_iter().collect();
        let prepared = self.prepare_pool_additions(registrations)?;

        self.commit_prepared_pool_additions(prepared)
    }

    /// Reserve the next generation for a logical pool without making it active.
    ///
    /// The generation is consumed immediately and appears as [`PoolRuntimeState::Queued`],
    /// but no registry entry, reactive handler, or ownership is installed. A
    /// duplicate reservation or ordinary [`add_pools`](Self::add_pools) call for
    /// the same key is rejected until this claim is committed or cancelled.
    pub fn reserve_pool_generation(
        &mut self,
        key: PoolKey,
    ) -> Result<AmmPoolGenerationReservation, AmmSyncError> {
        if self.registry.pool(&key).is_some() {
            return Err(RegistryError::DuplicatePool(key).into());
        }
        if let Some(instance) = self.reservations.get(&key) {
            return Err(AmmSyncError::PoolReservationConflict(instance.clone()));
        }
        if self.registry.adapter(key.protocol()).is_none() {
            return Err(AmmOwnershipError::MissingAdapter(key).into());
        }

        let generation = self.generations.next_pool(&key)?;
        let instance = PoolInstanceId::new(key.clone(), generation);
        self.generations.pools.insert(key.clone(), generation);
        self.reservations.insert(key, instance.clone());
        self.lifecycles
            .set_pool(instance.clone(), PoolRuntimeState::Queued);
        Ok(AmmPoolGenerationReservation { instance })
    }

    /// Commit a ready registration into exactly the reserved generation.
    ///
    /// A failed commit leaves the reservation intact so its caller can repair
    /// the registration or explicitly cancel the attempt. No other generation
    /// can consume or alias the claim while it remains outstanding.
    pub fn commit_pool_reservation(
        &mut self,
        reservation: &AmmPoolGenerationReservation,
        registration: PoolRegistration,
    ) -> Result<AmmLifecycleReport, AmmSyncError> {
        self.validate_pool_reservation(reservation, &registration)?;

        let prepared = self.prepare_pool_addition(registration, reservation.instance.clone())?;
        let report = self.commit_prepared_pool_additions(vec![prepared])?;
        self.reservations.remove(reservation.instance.key());
        Ok(report)
    }

    /// Preview the exact subscriber owner and interests for a reserved commit.
    ///
    /// The same reservation, ready registration, handler identity, and interest
    /// derivation are validated by [`commit_pool_reservation`](Self::commit_pool_reservation),
    /// allowing a subscriber transaction to fence delivery before topology is
    /// made visible.
    pub fn preview_reserved_pool_subscription(
        &self,
        reservation: &AmmPoolGenerationReservation,
        registration: &PoolRegistration,
    ) -> Result<AmmPoolSubscriptionPlan, AmmSyncError> {
        self.validate_pool_reservation(reservation, registration)?;
        self.prepare_pool_addition(registration.clone(), reservation.instance.clone())
            .map(|prepared| prepared.subscription_plan())
    }

    /// Cancel and tombstone an exact future-generation claim.
    ///
    /// Returns `false` for an already-consumed, already-cancelled, or stale
    /// token. Such a token can never cancel a newer reservation for the same
    /// logical pool.
    pub fn cancel_pool_reservation(&mut self, reservation: &AmmPoolGenerationReservation) -> bool {
        if self.reservations.get(reservation.instance.key()) != Some(&reservation.instance) {
            return false;
        }
        self.reservations.remove(reservation.instance.key());
        self.lifecycles
            .set_pool(reservation.instance.clone(), PoolRuntimeState::Removed);
        true
    }

    fn validate_pool_reservation(
        &self,
        reservation: &AmmPoolGenerationReservation,
        registration: &PoolRegistration,
    ) -> Result<(), AmmSyncError> {
        if registration.key != *reservation.instance.key() {
            return Err(AmmSyncError::PoolReservationKeyMismatch(
                reservation.instance.clone(),
            ));
        }
        if registration.status != PoolStatus::Ready {
            return Err(AmmSyncError::PoolReservationNotReady(
                registration.key.clone(),
            ));
        }
        if self.reservations.get(reservation.instance.key()) != Some(&reservation.instance) {
            return Err(AmmSyncError::StalePoolReservation(
                reservation.instance.clone(),
            ));
        }
        Ok(())
    }

    fn commit_prepared_pool_additions(
        &mut self,
        prepared: Vec<PreparedPoolAddition>,
    ) -> Result<AmmLifecycleReport, AmmSyncError> {
        let mut installed = Vec::new();
        for pool in &prepared {
            if let Err(error) = self.runtime.register_handler(pool.handler.clone()) {
                for handler in installed.iter().rev() {
                    self.runtime.unregister_handler(handler);
                }
                return Err(error.into());
            }
            installed.push(pool.ownership.handler().clone());
        }

        let mut committed = Vec::new();
        for pool in &prepared {
            if let Err(error) = self.registry.register_pool(pool.registration.clone()) {
                self.rollback_added_pools(&committed, &installed);
                return Err(error.into());
            }
            if let Err(error) = self.routing.register_pool(pool.registration.clone()) {
                self.registry.unregister_pool(&pool.registration.key);
                self.rollback_added_pools(&committed, &installed);
                return Err(error.into());
            }
            if let Err(error) = self.ownership.insert_pool(pool.ownership.clone()) {
                self.registry.unregister_pool(&pool.registration.key);
                self.routing.unregister_pool(&pool.registration.key);
                self.rollback_added_pools(&committed, &installed);
                return Err(error.into());
            }
            committed.push(pool.instance.clone());
        }

        let readopted_keys: HashSet<_> = prepared
            .iter()
            .map(|pool| pool.instance.key().clone())
            .collect();
        for pool in prepared {
            self.generations
                .pools
                .insert(pool.instance.key().clone(), pool.instance.generation());
            self.lifecycles
                .set_pool(pool.instance.clone(), pool.lifecycle);
        }
        self.degraded_pool_count = self.degraded_pool_count.saturating_add(
            committed
                .iter()
                .filter(|instance| {
                    self.registry
                        .pool(instance.key())
                        .is_some_and(|pool| pool.status == PoolStatus::Degraded)
                })
                .count(),
        );
        if !readopted_keys.is_empty() {
            // Re-registration re-adopts each logical pool's state. A fence for
            // an older explicitly evicted generation must not later override a
            // Retain removal of a replacement generation. Retain once for the
            // complete batch rather than rescanning fences per registration.
            self.eviction_fences
                .retain(|fence| !readopted_keys.contains(fence.instance().key()));
        }

        Ok(AmmLifecycleReport {
            registered_pools: committed,
            ..AmmLifecycleReport::default()
        })
    }

    /// Preview the exact subscriber owners/interests for a future atomic add.
    ///
    /// This performs the same validation and generation allocation preflight
    /// as [`add_pools`](Self::add_pools) without changing runtime, registry, or
    /// generation state. The result stays valid while the engine is exclusively
    /// borrowed and no lifecycle transaction intervenes.
    pub fn preview_pool_subscriptions(
        &self,
        pools: &[PoolRegistration],
    ) -> Result<Vec<AmmPoolSubscriptionPlan>, AmmSyncError> {
        self.prepare_pool_additions(pools.to_vec()).map(|prepared| {
            prepared
                .iter()
                .map(PreparedPoolAddition::subscription_plan)
                .collect()
        })
    }

    /// Subscriber owners/interests for every currently active pool generation.
    pub fn active_pool_subscriptions(&self) -> Result<Vec<AmmPoolSubscriptionPlan>, AmmSyncError> {
        let mut instances: Vec<_> = self.ownership.pools().cloned().collect();
        instances.sort();
        instances
            .into_iter()
            .map(|instance| {
                let handler = AmmPoolReactiveHandler::with_routing_context(
                    self.routing.clone(),
                    instance.clone(),
                )?;
                Ok(AmmPoolSubscriptionPlan {
                    handler: AmmPoolReactiveHandler::handler_id(&instance),
                    interests: handler.interests(),
                    instance,
                })
            })
            .collect()
    }

    /// Prevalidate an exact-generation metadata/handler/ownership refresh.
    ///
    /// The returned token exposes the old and replacement subscriber interests
    /// so an asynchronous owner can fence transport changes before committing.
    /// Preparing performs no mutation.
    pub fn prepare_pool_refresh(
        &self,
        instance: PoolInstanceId,
        replacement: PoolRegistration,
    ) -> Result<AmmPreparedPoolRefresh, AmmSyncError> {
        if self.ownership.active_pool(instance.key()) != Some(&instance) {
            return Err(AmmSyncError::StalePoolRefresh(instance));
        }
        if replacement.key != *instance.key() {
            return Err(AmmSyncError::PoolRefreshKeyMismatch(instance));
        }
        if replacement.status != PoolStatus::Ready {
            return Err(AmmSyncError::PoolRefreshNotReady(replacement.key));
        }
        let previous_registration =
            self.registry
                .pool(instance.key())
                .cloned()
                .ok_or(AmmSyncError::LifecycleInvariant(
                    "active refresh pool was absent from the registry",
                ))?;
        let previous_ownership = self
            .ownership
            .pool(&instance)
            .cloned()
            .ok_or_else(|| AmmOwnershipError::UnknownPool(instance.clone()))?;
        let adapter = self
            .registry
            .adapter(replacement.protocol())
            .cloned()
            .ok_or_else(|| AmmOwnershipError::MissingAdapter(replacement.key.clone()))?;
        let routing_registry = self.routing.registry();
        let routing_adapter = routing_registry.adapter(replacement.protocol()).ok_or(
            AmmSyncError::LifecycleInvariant(
                "refresh adapter was absent from the routing registry",
            ),
        )?;
        if routing_registry.pool(instance.key()).is_none()
            || !Arc::ptr_eq(&adapter, routing_adapter)
        {
            return Err(AmmSyncError::LifecycleInvariant(
                "refresh registry views diverged before preparation",
            ));
        }
        let adapter_key = AdapterKey::new(adapter.protocol(), adapter.protocols());
        if previous_ownership.adapter().key() != &adapter_key
            || self.ownership.active_adapter(&adapter_key) != Some(previous_ownership.adapter())
        {
            return Err(AmmSyncError::LifecycleInvariant(
                "refresh adapter family diverged from active ownership",
            ));
        }
        let sources = self.registry.event_sources_for(&replacement);
        let replacement_ownership = PoolOwnership::new(
            instance.clone(),
            previous_ownership.adapter().clone(),
            adapter.state_dependencies(&replacement),
            sources.iter().map(|source| source.emitter),
        )?;
        let replacement_handler = Arc::new(AmmPoolReactiveHandler::from_registration(
            self.routing.clone(),
            instance.clone(),
            replacement.clone(),
            adapter,
            sources,
        ));
        let previous_interests = self
            .runtime
            .handler_interests(previous_ownership.handler())
            .ok_or_else(|| AmmSyncError::MissingPoolHandler(previous_ownership.handler().clone()))?
            .to_vec();
        let previous_subscription = AmmPoolSubscriptionPlan {
            instance: instance.clone(),
            handler: previous_ownership.handler().clone(),
            interests: previous_interests,
        };
        let replacement_subscription = AmmPoolSubscriptionPlan {
            instance: instance.clone(),
            handler: replacement_ownership.handler().clone(),
            interests: replacement_handler.interests(),
        };

        Ok(AmmPreparedPoolRefresh {
            instance,
            previous_registration,
            replacement_registration: replacement,
            previous_ownership,
            replacement_ownership,
            replacement_handler,
            previous_subscription,
            replacement_subscription,
        })
    }

    /// Commit one prevalidated refresh without allocating a new pool generation.
    ///
    /// Runtime-global journal, health, hooks, pending resyncs, and unrelated
    /// handlers remain in place. Any stale or divergent token is rejected before
    /// the first mutation.
    pub fn commit_pool_refresh(
        &mut self,
        prepared: AmmPreparedPoolRefresh,
    ) -> Result<AmmPoolRefreshReport, AmmSyncError> {
        let AmmPreparedPoolRefresh {
            instance,
            previous_registration,
            replacement_registration,
            previous_ownership,
            replacement_ownership,
            replacement_handler,
            previous_subscription,
            replacement_subscription,
        } = prepared;
        if self.ownership.active_pool(instance.key()) != Some(&instance)
            || self.ownership.pool(&instance) != Some(&previous_ownership)
        {
            return Err(AmmSyncError::StalePoolRefresh(instance));
        }
        if self.registry.pool(instance.key()).is_none()
            || self.routing.registry().pool(instance.key()).is_none()
        {
            return Err(AmmSyncError::LifecycleInvariant(
                "refresh pool was absent from a registry view",
            ));
        }
        if !self.runtime.contains_handler(previous_ownership.handler()) {
            return Err(AmmSyncError::MissingPoolHandler(
                previous_ownership.handler().clone(),
            ));
        }

        let old_handler = self
            .runtime
            .unregister_handler(previous_ownership.handler())
            .ok_or_else(|| {
                AmmSyncError::MissingPoolHandler(previous_ownership.handler().clone())
            })?;
        if let Err(error) = self.runtime.register_handler(replacement_handler) {
            if self.runtime.register_handler(old_handler).is_err() {
                return Err(AmmSyncError::LifecycleInvariant(
                    "pool refresh handler rollback failed",
                ));
            }
            return Err(error.into());
        }

        let registration = self
            .registry
            .pool_mut(instance.key())
            .expect("refresh registry presence was prevalidated");
        *registration = replacement_registration.clone();
        if !self.routing.update_pool(replacement_registration.clone()) {
            *self
                .registry
                .pool_mut(instance.key())
                .expect("refresh registry remains present") = previous_registration.clone();
            let _ = self
                .runtime
                .unregister_handler(previous_ownership.handler());
            if self.runtime.register_handler(old_handler).is_err() {
                return Err(AmmSyncError::LifecycleInvariant(
                    "pool refresh routing rollback failed",
                ));
            }
            return Err(AmmSyncError::LifecycleInvariant(
                "refresh routing registration disappeared during commit",
            ));
        }
        if let Err(error) = self.ownership.replace_pool(replacement_ownership) {
            *self
                .registry
                .pool_mut(instance.key())
                .expect("refresh registry remains present") = previous_registration.clone();
            let routing_rollback = self.routing.update_pool(previous_registration.clone());
            let _ = self
                .runtime
                .unregister_handler(previous_ownership.handler());
            let handler_rollback = self.runtime.register_handler(old_handler).is_ok();
            if !routing_rollback || !handler_rollback {
                return Err(AmmSyncError::LifecycleInvariant(
                    "pool refresh ownership rollback failed",
                ));
            }
            return Err(error.into());
        }

        self.degraded_targets.remove(instance.key());
        match (
            previous_registration.status == PoolStatus::Degraded,
            replacement_registration.status == PoolStatus::Degraded,
        ) {
            (false, true) => self.degraded_pool_count = self.degraded_pool_count.saturating_add(1),
            (true, false) => self.degraded_pool_count = self.degraded_pool_count.saturating_sub(1),
            _ => {}
        }
        if self.lifecycles.pool(&instance) != Some(PoolRuntimeState::Live) {
            self.lifecycles
                .set_pool(instance.clone(), PoolRuntimeState::Searchable);
        }

        Ok(AmmPoolRefreshReport {
            instance,
            previous_registration,
            replacement_registration,
            previous_subscription,
            replacement_subscription,
        })
    }

    fn prepare_pool_additions(
        &self,
        mut registrations: Vec<PoolRegistration>,
    ) -> Result<Vec<PreparedPoolAddition>, AmmSyncError> {
        registrations.sort_by(|left, right| left.key.cmp(&right.key));

        let mut prepared = Vec::with_capacity(registrations.len());
        let mut previous: Option<&PoolKey> = None;
        for registration in &registrations {
            if previous == Some(&registration.key)
                || self.registry.pool(&registration.key).is_some()
            {
                return Err(RegistryError::DuplicatePool(registration.key.clone()).into());
            }
            if let Some(instance) = self.reservations.get(&registration.key) {
                return Err(AmmSyncError::PoolReservationConflict(instance.clone()));
            }
            previous = Some(&registration.key);

            let generation = self.generations.next_pool(&registration.key)?;
            let instance = PoolInstanceId::new(registration.key.clone(), generation);
            prepared.push(self.prepare_pool_addition(registration.clone(), instance)?);
        }

        Ok(prepared)
    }

    fn prepare_pool_addition(
        &self,
        registration: PoolRegistration,
        instance: PoolInstanceId,
    ) -> Result<PreparedPoolAddition, AmmSyncError> {
        let adapter = self
            .registry
            .adapter(registration.protocol())
            .cloned()
            .ok_or_else(|| AmmOwnershipError::MissingAdapter(registration.key.clone()))?;
        let adapter_key = AdapterKey::new(adapter.protocol(), adapter.protocols());
        let adapter_instance = self
            .ownership
            .active_adapter(&adapter_key)
            .cloned()
            .ok_or_else(|| AmmOwnershipError::MissingAdapter(registration.key.clone()))?;
        let sources = self.registry.event_sources_for(&registration);
        let ownership = PoolOwnership::new(
            instance.clone(),
            adapter_instance,
            adapter.state_dependencies(&registration),
            sources.iter().map(|source| source.emitter),
        )?;
        if self.runtime.contains_handler(ownership.handler()) {
            return Err(RegisterError::DuplicateHandler(ownership.handler().clone()).into());
        }
        let handler = Arc::new(AmmPoolReactiveHandler::from_registration(
            self.routing.clone(),
            instance.clone(),
            registration.clone(),
            adapter,
            sources,
        ));
        let lifecycle = pool_runtime_state(registration.status);
        Ok(PreparedPoolAddition {
            registration,
            instance,
            ownership,
            handler,
            lifecycle,
        })
    }

    fn rollback_added_pools(&mut self, committed: &[PoolInstanceId], handlers: &[HandlerId]) {
        for instance in committed.iter().rev() {
            self.registry.unregister_pool(instance.key());
            self.routing.unregister_pool(instance.key());
            self.ownership.remove_pool(instance);
        }
        for handler in handlers.iter().rev() {
            self.runtime.unregister_handler(handler);
        }
    }

    /// Atomically register one adapter family under all protocols it serves.
    pub fn add_adapter(
        &mut self,
        adapter: Arc<dyn AmmAdapter>,
    ) -> Result<AmmLifecycleReport, AmmSyncError> {
        let key = AdapterKey::new(adapter.protocol(), adapter.protocols());
        let generation = self.generations.next_adapter(&key)?;
        let instance = AdapterInstanceId::new(key.clone(), generation);

        self.registry.register_adapter(adapter.clone())?;
        if let Err(error) = self.routing.register_adapter(adapter.clone()) {
            self.registry
                .unregister_adapter(adapter.protocol())
                .expect("new adapter family cannot be in use");
            return Err(error.into());
        }
        if let Err(error) = self.ownership.insert_adapter(instance.clone()) {
            self.routing
                .unregister_adapter(adapter.protocol())
                .expect("new routing adapter cannot be in use");
            self.registry
                .unregister_adapter(adapter.protocol())
                .expect("new adapter family cannot be in use");
            return Err(error.into());
        }

        self.generations.adapters.insert(key, generation);
        self.lifecycles
            .set_adapter(instance.clone(), OwnerRuntimeState::Active);
        Ok(AmmLifecycleReport {
            registered_adapters: vec![instance],
            ..AmmLifecycleReport::default()
        })
    }

    /// Remove an unused adapter family without disturbing other families.
    ///
    /// Removal is rejected while any pool depends on the family. Use
    /// [`remove_adapter_cascade`](Self::remove_adapter_cascade) for an explicit
    /// dependent-pool teardown.
    pub fn remove_adapter(
        &mut self,
        protocol: super::ProtocolId,
    ) -> Result<AmmLifecycleReport, AmmSyncError> {
        let Some(adapter) = self.registry.adapter(protocol).cloned() else {
            return Ok(AmmLifecycleReport::default());
        };
        let key = AdapterKey::new(adapter.protocol(), adapter.protocols());
        let Some(instance) = self.ownership.active_adapter(&key).cloned() else {
            return Err(AmmOwnershipError::UnknownAdapter(AdapterInstanceId::new(
                key,
                AdapterGeneration::new(0),
            ))
            .into());
        };

        if let Some(pool) = self.ownership.pools_for_adapter(&instance).first() {
            return Err(RegistryError::AdapterInUse {
                protocol: pool.key().protocol(),
                pool: pool.key().clone(),
            }
            .into());
        }
        if !self.ownership.discovery_for_adapter(&instance).is_empty() {
            return Err(AmmOwnershipError::AdapterInUse(instance).into());
        }
        if self.routing.registry().adapter(protocol).is_none() {
            return Err(AmmSyncError::LifecycleInvariant(
                "active adapter was absent from the routing registry",
            ));
        }

        Ok(self.detach_adapter_prevalidated(protocol, instance))
    }

    fn detach_adapter_prevalidated(
        &mut self,
        protocol: super::ProtocolId,
        instance: AdapterInstanceId,
    ) -> AmmLifecycleReport {
        self.lifecycles
            .set_adapter(instance.clone(), OwnerRuntimeState::Removing);
        let cancelled_adapter_work = self
            .ownership
            .work_for_owner(&RuntimeOwnerId::Adapter(instance.clone()));
        self.registry
            .unregister_adapter_prevalidated(protocol)
            .expect("adapter detach was preflighted against the primary registry");
        self.routing
            .unregister_adapter_prevalidated(protocol)
            .expect("adapter detach was preflighted against the routing registry");
        assert!(
            self.ownership.remove_adapter_prevalidated(&instance),
            "adapter detach was preflighted against ownership"
        );
        self.lifecycles
            .set_adapter(instance.clone(), OwnerRuntimeState::Removed);
        AmmLifecycleReport {
            removed_adapters: vec![instance],
            cancelled_adapter_work,
            ..AmmLifecycleReport::default()
        }
    }

    /// Atomically remove an adapter family and every pool that depends on it.
    ///
    /// Warmed cache state is retained. This operation is explicit because a
    /// default [`remove_adapter`](Self::remove_adapter) never cascades.
    pub fn remove_adapter_cascade(
        &mut self,
        protocol: super::ProtocolId,
    ) -> Result<AmmLifecycleReport, AmmSyncError> {
        self.remove_adapter_cascade_with_cache(protocol, None)
    }

    /// Cascade adapter removal and evict cache state exclusive to its pools.
    pub fn remove_adapter_cascade_evicting(
        &mut self,
        protocol: super::ProtocolId,
        cache: &mut dyn AdapterCache,
    ) -> Result<AmmLifecycleReport, AmmSyncError> {
        self.remove_adapter_cascade_with_cache(protocol, Some(cache))
    }

    fn remove_adapter_cascade_with_cache(
        &mut self,
        protocol: super::ProtocolId,
        cache: Option<&mut dyn AdapterCache>,
    ) -> Result<AmmLifecycleReport, AmmSyncError> {
        let Some(adapter) = self.registry.adapter(protocol).cloned() else {
            return Ok(AmmLifecycleReport::default());
        };
        let key = AdapterKey::new(adapter.protocol(), adapter.protocols());
        let Some(instance) = self.ownership.active_adapter(&key).cloned() else {
            return Err(AmmOwnershipError::UnknownAdapter(AdapterInstanceId::new(
                key,
                AdapterGeneration::new(0),
            ))
            .into());
        };
        let pool_keys: Vec<_> = self
            .ownership
            .pools_for_adapter(&instance)
            .into_iter()
            .map(|pool| pool.into_parts().0)
            .collect();
        let removed_ownership: Vec<_> = self
            .ownership
            .pools_for_adapter(&instance)
            .iter()
            .filter_map(|pool| self.ownership.pool(pool))
            .cloned()
            .collect();

        // Complete preflight before the first destructive operation. With an
        // exclusive `&mut self`, all subsequent registry/index removals are
        // deterministic and cannot be invalidated concurrently.
        for pool in &pool_keys {
            let owned = self
                .ownership
                .active_pool(pool)
                .and_then(|instance| self.ownership.pool(instance))
                .ok_or_else(|| {
                    AmmOwnershipError::UnknownPool(
                        self.ownership
                            .active_pool(pool)
                            .cloned()
                            .unwrap_or_else(|| {
                                PoolInstanceId::new(pool.clone(), PoolGeneration::new(0))
                            }),
                    )
                })?;
            if !self.runtime.contains_handler(owned.handler()) {
                return Err(AmmSyncError::MissingPoolHandler(owned.handler().clone()));
            }
            if self.registry.pool(pool).is_none() {
                return Err(AmmOwnershipError::UnknownPool(owned.instance().clone()).into());
            }
        }
        {
            let routing = self.routing.registry();
            if routing.adapter(protocol).is_none()
                || pool_keys.iter().any(|pool| routing.pool(pool).is_none())
            {
                return Err(AmmSyncError::LifecycleInvariant(
                    "cascade preflight found divergent routing ownership",
                ));
            }
        }

        let mut pools = self.remove_pools(&pool_keys)?;
        let adapter = self.detach_adapter_prevalidated(protocol, instance);
        pools.removed_adapters.extend(adapter.removed_adapters);
        pools
            .registered_adapters
            .extend(adapter.registered_adapters);
        pools
            .cancelled_adapter_work
            .extend(adapter.cancelled_adapter_work);
        if let Some(cache) = cache {
            pools.eviction = evict_exclusive_state(cache, &removed_ownership, &self.ownership);
            self.record_eviction_fences(&removed_ownership);
        }
        Ok(pools)
    }

    /// Stop tracking pools, returning the removed registrations.
    ///
    /// Compatibility wrapper over [`remove_pools`](Self::remove_pools). Unknown
    /// keys are skipped. Cache state the pools warmed stays in place — use
    /// [`unregister_pools_evicting`](Self::unregister_pools_evicting) to also
    /// release it. Subscriber acknowledgement remains a Stage 4 responsibility;
    /// this synchronous engine owns only runtime/registry/index mutation.
    pub fn unregister_pools(
        &mut self,
        keys: &[PoolKey],
    ) -> Result<Vec<PoolRegistration>, AmmSyncError> {
        self.remove_pools(keys).map(|report| {
            report
                .removed_pools
                .into_iter()
                .map(|removed| removed.registration)
                .collect()
        })
    }

    /// Atomically remove the active generations of `keys` in place.
    ///
    /// Unknown keys are ignored. Cache state is retained; use
    /// [`remove_pools_evicting`](Self::remove_pools_evicting) for explicit,
    /// ownership-aware eviction.
    pub fn remove_pools(&mut self, keys: &[PoolKey]) -> Result<AmmLifecycleReport, AmmSyncError> {
        self.remove_pools_with_policy(keys, AmmEvictionPolicy::Retain, None)
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
        self.remove_pools_evicting(keys, cache).map(|report| {
            report
                .removed_pools
                .into_iter()
                .map(|removed| removed.registration)
                .collect()
        })
    }

    /// Remove pools and purge only cache state proven exclusive to them.
    pub fn remove_pools_evicting(
        &mut self,
        keys: &[PoolKey],
        cache: &mut dyn AdapterCache,
    ) -> Result<AmmLifecycleReport, AmmSyncError> {
        self.remove_pools_with_policy(keys, AmmEvictionPolicy::Exclusive, Some(cache))
    }

    fn remove_pools_with_policy(
        &mut self,
        keys: &[PoolKey],
        policy: AmmEvictionPolicy,
        cache: Option<&mut dyn AdapterCache>,
    ) -> Result<AmmLifecycleReport, AmmSyncError> {
        struct PreparedRemoval {
            instance: PoolInstanceId,
            ownership: PoolOwnership,
            handler: HandlerId,
            lifecycle: PoolRuntimeState,
            resyncs: Vec<ResyncId>,
        }

        let mut keys = keys.to_vec();
        keys.sort();
        keys.dedup();
        let mut prepared = Vec::new();
        for key in keys {
            let Some(instance) = self.ownership.active_pool(&key).cloned() else {
                continue;
            };
            let ownership = self
                .ownership
                .pool(&instance)
                .cloned()
                .ok_or_else(|| AmmOwnershipError::UnknownPool(instance.clone()))?;
            if !self.runtime.contains_handler(ownership.handler()) {
                return Err(AmmSyncError::MissingPoolHandler(
                    ownership.handler().clone(),
                ));
            }
            self.registry
                .pool(&key)
                .cloned()
                .ok_or_else(|| AmmOwnershipError::UnknownPool(instance.clone()))?;
            prepared.push(PreparedRemoval {
                resyncs: self.ownership.resyncs_for_pool(&instance),
                lifecycle: self.lifecycles.pool(&instance).ok_or(
                    AmmSyncError::LifecycleInvariant(
                        "active pool was absent from the lifecycle map",
                    ),
                )?,
                instance,
                handler: ownership.handler().clone(),
                ownership,
            });
        }
        {
            let routing = self.routing.registry();
            if prepared
                .iter()
                .any(|pool| routing.pool(pool.instance.key()).is_none())
            {
                return Err(AmmSyncError::LifecycleInvariant(
                    "pool removal preflight found divergent routing ownership",
                ));
            }
        }
        let mut resync_owner_by_id = HashMap::new();
        let mut resync_ids = Vec::new();
        for (pool_index, pool) in prepared.iter().enumerate() {
            for id in &pool.resyncs {
                match resync_owner_by_id.get(id) {
                    None => {
                        resync_owner_by_id.insert(id.clone(), pool_index);
                        resync_ids.push(id.clone());
                    }
                    Some(existing) if *existing == pool_index => {}
                    Some(_) => {
                        return Err(AmmSyncError::LifecycleInvariant(
                            "one resync id was owned by multiple active pool generations",
                        ));
                    }
                }
            }
        }

        for pool in &prepared {
            self.lifecycles
                .set_pool(pool.instance.clone(), PoolRuntimeState::Removing);
        }
        let mut removed_handlers = Vec::with_capacity(prepared.len());
        for pool in &prepared {
            if let Some(handler) = self.runtime.unregister_handler(&pool.handler) {
                removed_handlers.push(handler);
            } else {
                for handler in removed_handlers {
                    self.runtime.register_handler(handler)?;
                }
                for prepared in &prepared {
                    self.lifecycles
                        .set_pool(prepared.instance.clone(), prepared.lifecycle);
                }
                return Err(AmmSyncError::MissingPoolHandler(pool.handler.clone()));
            }
        }

        let cancelled_resyncs = self.runtime.cancel_pending_resyncs_by_id(&resync_ids);
        let mut cancelled_by_pool: Vec<Vec<ResyncRequest>> =
            (0..prepared.len()).map(|_| Vec::new()).collect();
        for request in cancelled_resyncs {
            if let Some(pool_index) = resync_owner_by_id.get(&request.id) {
                cancelled_by_pool[*pool_index].push(request);
            }
        }

        let mut removed_pools = Vec::with_capacity(prepared.len());
        for (pool_index, pool) in prepared.iter().enumerate() {
            let detached = self
                .ownership
                .remove_pool(&pool.instance)
                .expect("pool removal was completely prevalidated");
            let registration = self
                .registry
                .unregister_pool(pool.instance.key())
                .expect("registry removal was completely prevalidated");
            if registration.status == PoolStatus::Degraded {
                self.degraded_pool_count = self.degraded_pool_count.saturating_sub(1);
            }
            self.routing.unregister_pool(pool.instance.key());
            self.degraded_targets.remove(pool.instance.key());
            self.lifecycles
                .set_pool(pool.instance.clone(), PoolRuntimeState::Removed);
            removed_pools.push(AmmRemovedPool {
                instance: pool.instance.clone(),
                registration,
                cancelled_work: detached.work().to_vec(),
                detached_resyncs: detached.resyncs().to_vec(),
                cancelled_resyncs: std::mem::take(&mut cancelled_by_pool[pool_index]),
            });
        }

        let removed_ownership: Vec<_> =
            prepared.iter().map(|pool| pool.ownership.clone()).collect();
        let eviction = match (policy, cache) {
            (AmmEvictionPolicy::Exclusive, Some(cache)) => {
                let eviction = evict_exclusive_state(cache, &removed_ownership, &self.ownership);
                self.record_eviction_fences(&removed_ownership);
                eviction
            }
            _ => AmmEvictionReport {
                policy,
                ..AmmEvictionReport::default()
            },
        };

        Ok(AmmLifecycleReport {
            removed_pools,
            eviction,
            ..AmmLifecycleReport::default()
        })
    }

    fn record_eviction_fences(&mut self, removed: &[PoolOwnership]) {
        let journaled_handlers = self.runtime.journaled_handler_ids();
        for ownership in removed {
            // One active generation can be removed only once, so a successful
            // lifecycle transaction cannot record this instance twice.
            if journaled_handlers.contains(ownership.handler()) {
                self.eviction_fences.push(ownership.clone());
            }
        }
    }

    fn enforce_eviction_fences(&mut self, cache: &mut dyn AdapterCache) {
        if self.eviction_fences.is_empty() {
            return;
        }
        evict_exclusive_state(cache, &self.eviction_fences, &self.ownership);
        self.retire_eviction_fences();
    }

    fn retire_eviction_fences(&mut self) {
        let journaled_handlers = self.runtime.journaled_handler_ids();
        self.eviction_fences
            .retain(|ownership| journaled_handlers.contains(ownership.handler()));
    }

    /// Ingest one batch and execute all emitted slot repairs.
    ///
    /// This deliberately calls `ingest_batch_with_resync`, not plain
    /// `ingest_batch`: Balancer, Curve, and V3 liquidity events rely on the
    /// resync phase to refresh slots whose final values are not carried in logs.
    ///
    /// Pool status tracks attributable resync outcomes both ways: a pool whose
    /// target failed is marked [`PoolStatus::Degraded`], and it recovers only
    /// after every exact failed target (or complete enumerable read set) is
    /// authoritatively refreshed. Unknown impact and continuity/coverage loss
    /// remain fenced until an explicit authoritative refresh replaces the
    /// registration with a non-degraded status.
    pub fn ingest_batch(
        &mut self,
        cache: &mut EvmCache,
        batch: ReactiveInputBatch<Ethereum>,
    ) -> Result<AmmSyncBatchReport, AmmSyncError> {
        let reactive = match self.runtime.ingest_batch_with_resync(cache, batch) {
            Ok(reactive) => reactive,
            Err(error) => {
                // Ingest can recover a reorg before a later record fails. Keep
                // explicitly evicted generations absent before propagating the
                // error even though no complete batch report is available.
                self.enforce_eviction_fences(cache);
                return Err(error.into());
            }
        };
        self.finish_ingest(cache, reactive, false)
    }

    /// Apply canonical input without performing provider I/O on the cache-owner thread.
    ///
    /// Required repairs remain generation-scoped in the returned report and the
    /// affected pools are fenced degraded until an authoritative background
    /// refresh is committed.
    #[cfg(feature = "live-runtime")]
    pub(crate) fn ingest_batch_deferred_repairs(
        &mut self,
        cache: &mut EvmCache,
        batch: ReactiveInputBatch<Ethereum>,
    ) -> Result<AmmSyncBatchReport, AmmSyncError> {
        let reactive = match self.runtime.ingest_batch(cache, batch) {
            Ok(reactive) => reactive,
            Err(error) => {
                self.enforce_eviction_fences(cache);
                return Err(error.into());
            }
        };
        self.finish_ingest(cache, reactive, true)
    }

    fn finish_ingest(
        &mut self,
        cache: &mut EvmCache,
        reactive: ReactiveBatchReport<Ethereum>,
        defer_repairs: bool,
    ) -> Result<AmmSyncBatchReport, AmmSyncError> {
        if reactive
            .reports
            .iter()
            .any(|report| matches!(report.as_ref(), ReactiveReport::Reorg(_)))
        {
            self.enforce_eviction_fences(cache);
        } else {
            // Ordinary canonical progress only ages fences. Re-purging on every
            // block would needlessly bump the cache snapshot generation.
            self.retire_eviction_fences();
        }
        let tracked_resyncs = self.track_report_resyncs(&reactive)?;
        let mut pending_repairs = collect_pending_repairs(&reactive);
        let resync_state_updates = resync_state_update_count(&reactive);
        let resync_failures = resync_failure_count(&reactive);
        let mut uncertain_pools = self.mark_failed_resync_pools(&reactive);
        let mut explicit_refresh_pools = self.mark_decode_failed_pools(&reactive);
        if defer_repairs {
            explicit_refresh_pools.extend(
                pending_repairs
                    .iter()
                    .map(|repair| repair.pool().key().clone()),
            );
            for repair in &pending_repairs {
                self.set_pool_status(repair.pool().key(), PoolStatus::Degraded);
            }
            self.runtime.cancel_pending_resyncs_by_id(&tracked_resyncs);
        }
        explicit_refresh_pools.extend(self.mark_unresolved_event_pools(&reactive));
        explicit_refresh_pools.extend(self.mark_ambiguous_purge_pools(&reactive));
        let incidents = sync_incidents(&reactive);
        explicit_refresh_pools.extend(self.mark_coverage_gap_pools(&incidents));
        let requires_full_refresh = incidents.iter().any(|incident| {
            matches!(
                incident,
                AmmSyncIncident::Reorg { .. } | AmmSyncIncident::Gap { .. }
            )
        });
        if requires_full_refresh {
            explicit_refresh_pools.extend(self.mark_all_pools_degraded());
            pending_repairs = self
                .ownership
                .pools()
                .map(|pool| {
                    AmmPendingRepair::new(
                        pool.clone(),
                        RepairAction::ColdStart {
                            pool: pool.key().clone(),
                            policy: super::ColdStartPolicy::Eager,
                        },
                    )
                })
                .collect();
        }
        explicit_refresh_pools.sort();
        explicit_refresh_pools.dedup();
        self.require_explicit_refresh(&explicit_refresh_pools);
        uncertain_pools.extend(explicit_refresh_pools);
        uncertain_pools.sort();
        uncertain_pools.dedup();
        let mut degraded_pools = uncertain_pools.clone();
        degraded_pools.sort();
        degraded_pools.dedup();
        let mut recovered_pools = self.recover_resynced_pools(&reactive, &degraded_pools);
        recovered_pools.sort();
        recovered_pools.dedup();
        let mut pool_changes = sync_pool_changes(
            &self.ownership,
            &reactive,
            &degraded_pools,
            &recovered_pools,
            &uncertain_pools,
        );
        if requires_full_refresh {
            for change in &mut pool_changes {
                change.kind = AmmSyncPoolChangeKind::Degraded;
                change.impact = change.impact.union(AmmChangeImpact::quoteability());
                change.source = AmmSyncChangeSource::Unknown;
            }
        }
        let affected_pools = pool_changes
            .iter()
            .map(|change| change.pool.clone())
            .collect();

        for resync in tracked_resyncs {
            self.ownership.untrack_resync(&resync);
        }

        Ok(AmmSyncBatchReport {
            reactive,
            affected_pools,
            pool_changes,
            incidents,
            requires_full_refresh,
            degraded_pools,
            recovered_pools,
            resync_state_updates,
            resync_failures,
            pending_repairs,
        })
    }

    fn track_report_resyncs(
        &mut self,
        report: &ReactiveBatchReport<Ethereum>,
    ) -> Result<Vec<ResyncId>, AmmSyncError> {
        let mut tracked = Vec::new();
        let mut pending: Vec<(PoolInstanceId, ResyncId)> = Vec::new();
        for applied in &report.applied {
            let Some(pool) = self
                .ownership
                .pool_for_handler(&applied.handler_id)
                .cloned()
            else {
                continue;
            };
            for request in &applied.resyncs {
                match self.ownership.resync_owner(&request.id) {
                    Some(owner) if owner == &pool => {
                        if !tracked.contains(&request.id) {
                            tracked.push(request.id.clone());
                        }
                        continue;
                    }
                    Some(_) => {
                        return Err(AmmOwnershipError::DuplicateResync(request.id.clone()).into());
                    }
                    None => {
                        if let Some((owner, _)) = pending
                            .iter()
                            .find(|(_, pending_id)| pending_id == &request.id)
                        {
                            if owner != &pool {
                                return Err(
                                    AmmOwnershipError::DuplicateResync(request.id.clone()).into()
                                );
                            }
                        } else {
                            pending.push((pool.clone(), request.id.clone()));
                        }
                    }
                }
                if !tracked.contains(&request.id) {
                    tracked.push(request.id.clone());
                }
            }
        }
        for (pool, resync) in pending {
            self.ownership.track_resync(pool, resync)?;
        }
        Ok(tracked)
    }

    fn mark_failed_resync_pools(&mut self, report: &ReactiveBatchReport<Ethereum>) -> Vec<PoolKey> {
        let mut degraded = Vec::new();
        for failure in resync_failures(report) {
            for instance in pools_for_failure(&self.ownership, failure) {
                let key = instance.key().clone();
                if !degraded.contains(&key) {
                    degraded.push(key.clone());
                }
                // Remember the concrete slots/accounts this pool's repair failed
                // on, so recovery can require exactly them to refresh rather than
                // vouching for the pool on any later write at its address.
                let recorded = PendingTargets::covered(&failure.target, &instance, &self.ownership);
                self.degraded_targets
                    .entry(key.clone())
                    .or_default()
                    .merge(recorded);
                self.set_pool_status(&key, PoolStatus::Degraded);
            }
        }
        degraded
    }

    fn mark_decode_failed_pools(&mut self, report: &ReactiveBatchReport<Ethereum>) -> Vec<PoolKey> {
        let mut degraded = BTreeMap::new();
        for signal in report
            .applied
            .iter()
            .flat_map(|applied| &applied.hook_signals)
        {
            let Some(signal) = signal
                .payload
                .as_deref()
                .and_then(|payload| payload.downcast_ref::<AmmReactiveSignal>())
            else {
                continue;
            };
            let pool = match signal {
                AmmReactiveSignal::DecodeError { pool, .. } => pool,
                AmmReactiveSignal::PoolDecodeError { instance, .. } => instance.key(),
                AmmReactiveSignal::Event(_)
                | AmmReactiveSignal::PoolEvent { .. }
                | AmmReactiveSignal::PoolRepair { .. } => continue,
            };
            if self.set_pool_status(pool, PoolStatus::Degraded) {
                degraded.insert(pool.clone(), ());
            }
        }
        degraded.into_keys().collect()
    }

    fn mark_unresolved_event_pools(
        &mut self,
        report: &ReactiveBatchReport<Ethereum>,
    ) -> Vec<PoolKey> {
        let mut degraded = BTreeMap::new();
        for applied in &report.applied {
            for signal in applied.hook_signals.iter().filter_map(|signal| {
                signal
                    .payload
                    .as_deref()?
                    .downcast_ref::<AmmReactiveSignal>()
            }) {
                let (event, pool) = match signal {
                    AmmReactiveSignal::Event(event) => (event, &event.pool),
                    AmmReactiveSignal::PoolEvent { instance, event } => (event, instance.key()),
                    AmmReactiveSignal::DecodeError { .. }
                    | AmmReactiveSignal::PoolDecodeError { .. }
                    | AmmReactiveSignal::PoolRepair { .. } => continue,
                };
                if !event_is_unresolved(event, applied.resyncs.is_empty()) {
                    continue;
                }
                if self.set_pool_status(pool, PoolStatus::Degraded) {
                    degraded.insert(pool.clone(), ());
                }
            }
        }
        degraded.into_keys().collect()
    }

    fn mark_all_pools_degraded(&mut self) -> Vec<PoolKey> {
        let keys: Vec<_> = self
            .ownership
            .pools()
            .map(|instance| instance.key().clone())
            .collect();
        for key in &keys {
            self.set_pool_status(key, PoolStatus::Degraded);
        }
        keys
    }

    fn mark_ambiguous_purge_pools(
        &mut self,
        report: &ReactiveBatchReport<Ethereum>,
    ) -> Vec<PoolKey> {
        let direct = report
            .applied
            .iter()
            .flat_map(|applied| &applied.diff.purged);
        let authoritative = resync_reports(report).flat_map(|resync| &resync.diff.purged);
        let mut affected = BTreeMap::new();
        for purged in direct.chain(authoritative) {
            let evm_fork_cache::PurgeScope::Slots(slots) = &purged.scope else {
                continue;
            };
            let mut unique = slots.clone();
            unique.sort_unstable();
            unique.dedup();
            if purged.slots_removed == 0 || purged.slots_removed == unique.len() {
                continue;
            }
            for pool in unique.iter().flat_map(|slot| {
                self.ownership
                    .pools_for_slot(StateSlot::new(purged.address, *slot))
            }) {
                affected.insert(pool.key().clone(), ());
            }
        }
        let affected: Vec<_> = affected.into_keys().collect();
        for key in &affected {
            self.set_pool_status(key, PoolStatus::Degraded);
        }
        affected
    }

    fn mark_coverage_gap_pools(&mut self, incidents: &[AmmSyncIncident]) -> Vec<PoolKey> {
        let addresses: Vec<_> = incidents
            .iter()
            .filter_map(|incident| match incident {
                AmmSyncIncident::CoverageGap { address, .. } => Some(*address),
                _ => None,
            })
            .collect();
        let mut affected: Vec<_> = addresses
            .into_iter()
            .flat_map(|address| self.ownership.pools_for_address(address))
            .map(|instance| instance.key().clone())
            .collect();
        affected.sort();
        affected.dedup();
        for key in &affected {
            self.set_pool_status(key, PoolStatus::Degraded);
        }
        affected
    }

    fn require_explicit_refresh(&mut self, pools: &[PoolKey]) {
        for pool in pools {
            self.degraded_targets
                .entry(pool.clone())
                .or_default()
                .require_explicit_refresh = true;
        }
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
    /// - A pool degraded by an untracked cold-start miss recovers only when every
    ///   slot in its enumerable discovered read set refreshes. Address-wide
    ///   protocols cannot prove completeness from one event resync.
    /// - Unknown event impact, continuity/coverage loss, and account-target
    ///   failures install an explicit-refresh fence. A later partial event repair
    ///   cannot clear it; a caller must replace the registration only after an
    ///   authoritative cold-start/verification has made it non-degraded.
    ///
    /// A recovered pool's tracked targets are cleared. Conservative by
    /// construction: a pool without an enumerable complete read set cannot be
    /// vouched for by the compatibility resync path, so it stays degraded.
    fn recover_resynced_pools(
        &mut self,
        report: &ReactiveBatchReport<Ethereum>,
        degraded_now: &[PoolKey],
    ) -> Vec<PoolKey> {
        let refreshed = resync_refreshes(report);
        if refreshed.is_empty() {
            return Vec::new();
        }
        let targets = &self.degraded_targets;
        let mut candidates: Vec<_> = refreshed
            .slots
            .iter()
            .flat_map(|(address, slot)| {
                self.ownership
                    .pools_for_slot(StateSlot::new(*address, *slot))
            })
            .collect();
        candidates.sort();
        candidates.dedup();
        let recovered: Vec<PoolKey> = candidates
            .into_iter()
            .filter(|instance| {
                self.registry
                    .pool(instance.key())
                    .is_some_and(|pool| pool.status == PoolStatus::Degraded)
            })
            .filter(|instance| !degraded_now.contains(instance.key()))
            .filter(|instance| {
                self.ownership.pool(instance).is_some_and(|ownership| {
                    should_recover(
                        ownership.dependencies(),
                        targets.get(instance.key()),
                        &refreshed,
                    )
                })
            })
            .map(|instance| instance.key().clone())
            .collect();
        for key in &recovered {
            self.set_pool_status(key, PoolStatus::Ready);
            self.degraded_targets.remove(key);
        }
        recovered
    }

    fn set_pool_status(&mut self, key: &PoolKey, status: PoolStatus) -> bool {
        let Some(registration) = self.registry.pool_mut(key) else {
            return false;
        };
        match (
            registration.status == PoolStatus::Degraded,
            status == PoolStatus::Degraded,
        ) {
            (false, true) => self.degraded_pool_count = self.degraded_pool_count.saturating_add(1),
            (true, false) => self.degraded_pool_count = self.degraded_pool_count.saturating_sub(1),
            _ => {}
        }
        registration.status = status;
        let routing_registration = registration.clone();
        debug_assert!(
            self.routing.update_pool(routing_registration),
            "active pool status must exist in the routing registry"
        );
        if let Some(instance) = self.ownership.active_pool(key).cloned() {
            self.lifecycles
                .set_pool(instance, pool_runtime_state(status));
        }
        true
    }
}

fn sync_pool_changes(
    ownership: &AmmOwnershipIndex,
    report: &ReactiveBatchReport<Ethereum>,
    degraded: &[PoolKey],
    recovered: &[PoolKey],
    uncertain: &[PoolKey],
) -> Vec<AmmSyncPoolChange> {
    let mut changes: BTreeMap<PoolKey, AmmSyncPoolChange> = BTreeMap::new();

    for pool in uncertain {
        changes.insert(
            pool.clone(),
            AmmSyncPoolChange {
                pool: pool.clone(),
                kind: AmmSyncPoolChangeKind::UnknownImpact,
                impact: AmmChangeImpact::quoteability(),
                source: AmmSyncChangeSource::Unknown,
            },
        );
    }

    for applied in &report.applied {
        for signal in applied.hook_signals.iter().filter_map(|signal| {
            signal
                .payload
                .as_deref()?
                .downcast_ref::<AmmReactiveSignal>()
        }) {
            match signal {
                AmmReactiveSignal::Event(event)
                    if event_is_unresolved(event, applied.resyncs.is_empty()) =>
                {
                    changes.insert(
                        event.pool.clone(),
                        AmmSyncPoolChange {
                            pool: event.pool.clone(),
                            kind: AmmSyncPoolChangeKind::UnknownImpact,
                            impact: AmmChangeImpact::new(true, true, false),
                            source: AmmSyncChangeSource::Unknown,
                        },
                    );
                }
                AmmReactiveSignal::PoolEvent { instance, event }
                    if event_is_unresolved(event, applied.resyncs.is_empty()) =>
                {
                    let pool = instance.key();
                    changes.insert(
                        pool.clone(),
                        AmmSyncPoolChange {
                            pool: pool.clone(),
                            kind: AmmSyncPoolChangeKind::UnknownImpact,
                            impact: AmmChangeImpact::new(true, true, false),
                            source: AmmSyncChangeSource::Unknown,
                        },
                    );
                }
                AmmReactiveSignal::DecodeError { pool, .. } => {
                    changes.insert(
                        pool.clone(),
                        AmmSyncPoolChange {
                            pool: pool.clone(),
                            kind: AmmSyncPoolChangeKind::UnknownImpact,
                            impact: AmmChangeImpact::quoteability(),
                            source: AmmSyncChangeSource::Unknown,
                        },
                    );
                }
                AmmReactiveSignal::PoolDecodeError { instance, .. } => {
                    let pool = instance.key();
                    changes.insert(
                        pool.clone(),
                        AmmSyncPoolChange {
                            pool: pool.clone(),
                            kind: AmmSyncPoolChangeKind::UnknownImpact,
                            impact: AmmChangeImpact::quoteability(),
                            source: AmmSyncChangeSource::Unknown,
                        },
                    );
                }
                AmmReactiveSignal::Event(_)
                | AmmReactiveSignal::PoolEvent { .. }
                | AmmReactiveSignal::PoolRepair { .. } => {}
            }
        }
    }

    for applied in &report.applied {
        for changed in &applied.diff.slots {
            for pool in ownership.pools_for_slot(StateSlot::new(changed.address, changed.slot)) {
                record_state_change(
                    &mut changes,
                    pool.key().clone(),
                    AmmSyncChangeSource::Direct,
                );
            }
        }
        for changed in &applied.diff.accounts {
            for pool in ownership.pools_for_address(changed.address) {
                record_state_change(
                    &mut changes,
                    pool.key().clone(),
                    AmmSyncChangeSource::Direct,
                );
            }
        }
        for purged in &applied.diff.purged {
            record_purge_change(&mut changes, ownership, purged, AmmSyncChangeSource::Direct);
        }
    }

    for resync in resync_reports(report) {
        for changed in &resync.diff.slots {
            for pool in ownership.pools_for_slot(StateSlot::new(changed.address, changed.slot)) {
                record_state_change(
                    &mut changes,
                    pool.key().clone(),
                    AmmSyncChangeSource::AuthoritativeResync,
                );
            }
        }
        for changed in &resync.diff.accounts {
            for pool in ownership.pools_for_address(changed.address) {
                record_state_change(
                    &mut changes,
                    pool.key().clone(),
                    AmmSyncChangeSource::AuthoritativeResync,
                );
            }
        }
        for purged in &resync.diff.purged {
            record_purge_change(
                &mut changes,
                ownership,
                purged,
                AmmSyncChangeSource::AuthoritativeResync,
            );
        }
    }

    for pool in degraded {
        changes
            .entry(pool.clone())
            .and_modify(|change| {
                change.kind = AmmSyncPoolChangeKind::Degraded;
                change.impact = change.impact.union(AmmChangeImpact::quoteability());
            })
            .or_insert_with(|| AmmSyncPoolChange {
                pool: pool.clone(),
                kind: AmmSyncPoolChangeKind::Degraded,
                impact: AmmChangeImpact::quoteability(),
                source: AmmSyncChangeSource::LifecycleOnly,
            });
    }
    for pool in recovered {
        changes
            .entry(pool.clone())
            .and_modify(|change| {
                change.kind = AmmSyncPoolChangeKind::Recovered;
                change.impact = change.impact.union(AmmChangeImpact::quoteability());
                change.source = change.source.with_authoritative_resync();
            })
            .or_insert_with(|| AmmSyncPoolChange {
                pool: pool.clone(),
                kind: AmmSyncPoolChangeKind::Recovered,
                impact: AmmChangeImpact::quoteability(),
                source: AmmSyncChangeSource::AuthoritativeResync,
            });
    }

    changes.into_values().collect()
}

fn collect_pending_repairs(report: &ReactiveBatchReport<Ethereum>) -> Vec<AmmPendingRepair> {
    let mut repairs = BTreeMap::<PoolInstanceId, RepairAction>::new();
    for signal in report
        .applied
        .iter()
        .flat_map(|applied| &applied.hook_signals)
    {
        let Some(reactive) = signal
            .payload
            .as_deref()
            .and_then(|payload| payload.downcast_ref::<AmmReactiveSignal>())
        else {
            continue;
        };
        let AmmReactiveSignal::PoolRepair { instance, action } = reactive else {
            continue;
        };
        repairs
            .entry(instance.clone())
            .and_modify(|existing| {
                *existing = std::mem::take(existing).combine(action.clone());
            })
            .or_insert_with(|| action.clone());
    }
    repairs
        .into_iter()
        .map(|(instance, action)| AmmPendingRepair::new(instance, action))
        .collect()
}

fn record_state_change(
    changes: &mut BTreeMap<PoolKey, AmmSyncPoolChange>,
    pool: PoolKey,
    source: AmmSyncChangeSource,
) {
    changes
        .entry(pool.clone())
        .and_modify(|change| {
            change.kind = AmmSyncPoolChangeKind::Updated;
            change.impact = change.impact.union(AmmChangeImpact::state_only());
            change.source = match source {
                AmmSyncChangeSource::Direct => change.source.with_direct(),
                AmmSyncChangeSource::AuthoritativeResync => {
                    change.source.with_authoritative_resync()
                }
                AmmSyncChangeSource::DirectAndAuthoritativeResync => {
                    change.source.with_direct().with_authoritative_resync()
                }
                AmmSyncChangeSource::LifecycleOnly => change.source,
                AmmSyncChangeSource::Unknown => AmmSyncChangeSource::Unknown,
            };
        })
        .or_insert(AmmSyncPoolChange {
            pool,
            kind: AmmSyncPoolChangeKind::Updated,
            impact: AmmChangeImpact::state_only(),
            source,
        });
}

fn record_purge_change(
    changes: &mut BTreeMap<PoolKey, AmmSyncPoolChange>,
    ownership: &AmmOwnershipIndex,
    purged: &evm_fork_cache::PurgeRecord,
    source: AmmSyncChangeSource,
) {
    if purged.slots_removed == 0 && !purged.account_removed {
        return;
    }
    let (pools, exact) = match &purged.scope {
        evm_fork_cache::PurgeScope::Slots(slots) => {
            let mut unique = slots.clone();
            unique.sort_unstable();
            unique.dedup();
            (
                unique
                    .iter()
                    .flat_map(|slot| {
                        ownership.pools_for_slot(StateSlot::new(purged.address, *slot))
                    })
                    .collect(),
                purged.slots_removed == unique.len(),
            )
        }
        evm_fork_cache::PurgeScope::Account | evm_fork_cache::PurgeScope::AllStorage => {
            (ownership.pools_for_address(purged.address), true)
        }
        _ => (ownership.pools_for_address(purged.address), false),
    };
    let mut keys: Vec<_> = pools.into_iter().map(|pool| pool.key().clone()).collect();
    keys.sort();
    keys.dedup();
    for pool in keys {
        if exact {
            record_state_change(changes, pool, source);
        } else {
            changes.insert(
                pool.clone(),
                AmmSyncPoolChange {
                    pool,
                    kind: AmmSyncPoolChangeKind::UnknownImpact,
                    impact: AmmChangeImpact::new(true, true, false),
                    source: AmmSyncChangeSource::Unknown,
                },
            );
        }
    }
}

fn event_is_unresolved(event: &AdapterEvent, has_no_resync_requests: bool) -> bool {
    has_no_resync_requests
        && matches!(
            event.quality,
            UpdateQuality::RequiresRepair | UpdateQuality::ConservativeInvalidation
        )
}

fn sync_incidents(report: &ReactiveBatchReport<Ethereum>) -> Vec<AmmSyncIncident> {
    let mut incidents = Vec::new();
    for report in &report.reports {
        let incident = match report.as_ref() {
            ReactiveReport::Reorg(reorg) => AmmSyncIncident::Reorg {
                dropped_blocks: reorg.dropped_blocks.clone(),
            },
            ReactiveReport::MissedBlockRange(gap) => AmmSyncIncident::Gap {
                from: gap.from,
                to: gap.to,
            },
            ReactiveReport::CoverageGap(gap) => AmmSyncIncident::CoverageGap {
                address: gap.address,
                block: gap.block,
            },
            _ => continue,
        };
        if !incidents.contains(&incident) {
            incidents.push(incident);
        }
    }
    incidents.sort_by(compare_incidents);
    incidents
}

fn compare_incidents(left: &AmmSyncIncident, right: &AmmSyncIncident) -> Ordering {
    fn rank(incident: &AmmSyncIncident) -> u8 {
        match incident {
            AmmSyncIncident::Reorg { .. } => 0,
            AmmSyncIncident::Gap { .. } => 1,
            AmmSyncIncident::CoverageGap { .. } => 2,
        }
    }

    rank(left)
        .cmp(&rank(right))
        .then_with(|| match (left, right) {
            (
                AmmSyncIncident::Reorg {
                    dropped_blocks: left,
                },
                AmmSyncIncident::Reorg {
                    dropped_blocks: right,
                },
            ) => {
                left.iter()
                    .map(|block| (block.number, block.hash, block.parent_hash, block.timestamp))
                    .cmp(right.iter().map(|block| {
                        (block.number, block.hash, block.parent_hash, block.timestamp)
                    }))
            }
            (
                AmmSyncIncident::Gap {
                    from: left_from,
                    to: left_to,
                },
                AmmSyncIncident::Gap {
                    from: right_from,
                    to: right_to,
                },
            ) => (*left_from, *left_to).cmp(&(*right_from, *right_to)),
            (
                AmmSyncIncident::CoverageGap {
                    address: left_address,
                    block: left_block,
                },
                AmmSyncIncident::CoverageGap {
                    address: right_address,
                    block: right_block,
                },
            ) => (*left_block, *left_address).cmp(&(*right_block, *right_address)),
            _ => Ordering::Equal,
        })
}

fn runtime_for(
    ownership: &AmmOwnershipIndex,
    routing: AmmReactiveRoutingContext,
    config: ReactiveConfig,
) -> Result<ReactiveRuntime<Ethereum>, AmmSyncError> {
    let mut runtime = ReactiveRuntime::<Ethereum>::new(config);
    for instance in ownership.pools() {
        runtime.register_handler(Arc::new(AmmPoolReactiveHandler::with_routing_context(
            routing.clone(),
            instance.clone(),
        )?))?;
    }
    Ok(runtime)
}

fn ownership_for_registry(
    registry: &AdapterRegistry,
    previous: &RuntimeGenerationLedger,
) -> Result<(AmmOwnershipIndex, RuntimeGenerationLedger), AmmSyncError> {
    let mut ownership = AmmOwnershipIndex::default();
    let mut generations = previous.clone();
    let mut families: BTreeMap<AdapterKey, Arc<dyn AmmAdapter>> = BTreeMap::new();
    for adapter in registry.adapters() {
        let key = AdapterKey::new(adapter.protocol(), adapter.protocols());
        families.entry(key).or_insert_with(|| adapter.clone());
    }

    for key in families.keys() {
        let generation = previous.next_adapter(key)?;
        ownership.insert_adapter(AdapterInstanceId::new(key.clone(), generation))?;
        generations.adapters.insert(key.clone(), generation);
    }

    let mut pools: Vec<_> = registry.pools().collect();
    pools.sort_by(|left, right| left.key.cmp(&right.key));
    for pool in pools {
        let adapter = registry
            .adapter(pool.protocol())
            .cloned()
            .ok_or_else(|| AmmOwnershipError::MissingAdapter(pool.key.clone()))?;
        let adapter_key = AdapterKey::new(adapter.protocol(), adapter.protocols());
        let adapter_instance = ownership
            .active_adapter(&adapter_key)
            .cloned()
            .ok_or_else(|| AmmOwnershipError::MissingAdapter(pool.key.clone()))?;
        let generation = previous.next_pool(&pool.key)?;
        let instance = PoolInstanceId::new(pool.key.clone(), generation);
        let sources = registry.event_sources_for(pool);
        ownership.insert_pool(PoolOwnership::new(
            instance.clone(),
            adapter_instance,
            adapter.state_dependencies(pool),
            sources.iter().map(|source| source.emitter),
        )?)?;
        generations.pools.insert(pool.key.clone(), generation);
    }
    Ok((ownership, generations))
}

fn lifecycle_map_for(
    registry: &AdapterRegistry,
    ownership: &AmmOwnershipIndex,
) -> RuntimeLifecycleMap {
    let mut lifecycles = RuntimeLifecycleMap::default();
    for adapter in ownership.adapters() {
        lifecycles.set_adapter(adapter.clone(), OwnerRuntimeState::Active);
    }
    for instance in ownership.pools() {
        let state = registry
            .pool(instance.key())
            .map(|pool| pool_runtime_state(pool.status))
            .unwrap_or(PoolRuntimeState::Failed);
        lifecycles.set_pool(instance.clone(), state);
    }
    lifecycles
}

fn pool_runtime_state(status: PoolStatus) -> PoolRuntimeState {
    match status {
        PoolStatus::Pending => PoolRuntimeState::Discovered,
        PoolStatus::Cold => PoolRuntimeState::Hydrating,
        PoolStatus::Ready => PoolRuntimeState::Searchable,
        PoolStatus::Degraded => PoolRuntimeState::Degraded,
        PoolStatus::Disabled | PoolStatus::Unsupported => PoolRuntimeState::Failed,
    }
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

fn resync_reports(
    report: &ReactiveBatchReport<Ethereum>,
) -> impl Iterator<Item = &evm_fork_cache::reactive::ResyncReport> {
    report
        .reports
        .iter()
        .filter_map(|report| match report.as_ref() {
            ReactiveReport::Resynced(report) => Some(report),
            _ => None,
        })
}

fn resync_failures(report: &ReactiveBatchReport<Ethereum>) -> impl Iterator<Item = &ResyncFailure> {
    resync_reports(report).flat_map(|report| report.failed.iter())
}

/// State targets authoritatively refreshed by this batch's executed resyncs,
/// including no-op writes that still verify the current value.
fn resync_refreshes(report: &ReactiveBatchReport<Ethereum>) -> ResyncRefresh {
    let mut refreshed = ResyncRefresh::default();
    for update in resync_reports(report).flat_map(|report| &report.state_updates) {
        if let evm_fork_cache::StateUpdate::Slot { address, slot, .. } = update {
            refreshed.slots.push((*address, *slot));
        }
    }
    refreshed.slots.sort_unstable();
    refreshed.slots.dedup();
    refreshed
}

/// Purge cache state owned exclusively by `removed` pools, sparing anything a
/// `remaining` pool still references.
fn evict_exclusive_state(
    cache: &mut dyn AdapterCache,
    removed: &[PoolOwnership],
    remaining: &AmmOwnershipIndex,
) -> AmmEvictionReport {
    let mut purged_accounts = Vec::new();
    let mut purged_slots = Vec::new();
    for pool in removed {
        for address in pool.dependencies().associated_addresses() {
            if remaining.pools_for_address(*address).is_empty() {
                cache.purge_storage(*address);
                purged_accounts.push(*address);
                continue;
            }
            let exclusive: Vec<U256> = pool
                .dependencies()
                .slots()
                .iter()
                .filter(|slot| slot.address() == *address)
                .map(|slot| slot.slot())
                .filter(|slot| {
                    remaining
                        .pools_for_slot(StateSlot::new(*address, *slot))
                        .is_empty()
                })
                .collect();
            if !exclusive.is_empty() {
                cache.purge_slots(*address, &exclusive);
                purged_slots.extend(
                    exclusive
                        .into_iter()
                        .map(|slot| StateSlot::new(*address, slot)),
                );
            }
        }
    }
    purged_accounts.sort_unstable();
    purged_accounts.dedup();
    purged_slots.sort_unstable();
    purged_slots.dedup();
    AmmEvictionReport {
        policy: AmmEvictionPolicy::Exclusive,
        purged_accounts,
        purged_slots,
    }
}

fn pools_for_failure(
    ownership: &AmmOwnershipIndex,
    failure: &ResyncFailure,
) -> Vec<PoolInstanceId> {
    if let Some(pool) = ownership.resync_owner(&failure.request_id) {
        return vec![pool.clone()];
    }
    let mut pools = match &failure.target {
        ResyncTarget::StorageSlot { address, slot } => {
            ownership.pools_for_slot(StateSlot::new(*address, *slot))
        }
        ResyncTarget::StorageSlots { address, slots } => slots
            .iter()
            .flat_map(|slot| ownership.pools_for_slot(StateSlot::new(*address, *slot)))
            .collect(),
        ResyncTarget::Account { address, .. } => ownership.pools_for_address(*address),
    };
    pools.sort();
    pools.dedup();
    pools
}

/// The concrete repair targets a `Degraded` pool is waiting on, recorded when a
/// resync fails so recovery can require exactly them — not any write at the
/// pool's address. See `AmmSyncEngine::recover_resynced_pools`.
#[derive(Clone, Debug, Default)]
struct PendingTargets {
    /// `(address, slot)` repairs that failed; each must be refreshed to recover.
    slots: Vec<(Address, U256)>,
    /// Trust was lost without a complete attributable repair. Only an explicit
    /// authoritative registry replacement/cold-start may clear this fence.
    require_explicit_refresh: bool,
}

impl PendingTargets {
    /// The subset of `target` that `pool` actually covers according to the
    /// generation-scoped ownership index.
    fn covered(
        target: &ResyncTarget,
        pool: &PoolInstanceId,
        ownership: &AmmOwnershipIndex,
    ) -> Self {
        let mut pending = Self::default();
        match target {
            ResyncTarget::StorageSlot { address, slot } => {
                if ownership
                    .pools_for_slot(StateSlot::new(*address, *slot))
                    .contains(pool)
                {
                    pending.slots.push((*address, *slot));
                }
            }
            ResyncTarget::StorageSlots { address, slots } => {
                for slot in slots {
                    if ownership
                        .pools_for_slot(StateSlot::new(*address, *slot))
                        .contains(pool)
                    {
                        pending.slots.push((*address, *slot));
                    }
                }
            }
            ResyncTarget::Account { address, .. } => {
                if ownership.pools_for_address(*address).contains(pool) {
                    // The compatibility layer does not retain AccountFieldMask
                    // provenance, so no later partial account write can prove
                    // that the originally requested fields were refreshed.
                    pending.require_explicit_refresh = true;
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
        self.require_explicit_refresh |= other.require_explicit_refresh;
    }

    fn is_empty(&self) -> bool {
        self.slots.is_empty() && !self.require_explicit_refresh
    }

    /// Whether this batch's authoritative slot writes cover every outstanding
    /// target. Explicit-refresh barriers are never satisfied here.
    fn satisfied_by(&self, refreshed: &ResyncRefresh) -> bool {
        if self.require_explicit_refresh {
            return false;
        }
        self.slots
            .iter()
            .all(|target| refreshed.slots.contains(target))
    }
}

#[derive(Clone, Debug, Default)]
struct ResyncRefresh {
    slots: Vec<(Address, U256)>,
}

impl ResyncRefresh {
    fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }
}

/// Whether a `Degraded` `pool` should flip back to `Ready` given this batch's
/// `refreshed` slot writes and its recorded failed `targets` (if any).
///
/// With a non-empty tracked target set, recovery requires exactly those to
/// refresh. Otherwise, every slot in an enumerable complete read set must have
/// refreshed; address-wide coverage is deliberately insufficient.
fn should_recover(
    dependencies: &PoolStateDependencies,
    targets: Option<&PendingTargets>,
    refreshed: &ResyncRefresh,
) -> bool {
    match targets {
        Some(pending) if !pending.is_empty() => pending.satisfied_by(refreshed),
        _ => {
            let required: Vec<_> = dependencies
                .slots()
                .iter()
                .map(|slot| (slot.address(), slot.slot()))
                .collect();
            !required.is_empty()
                && required
                    .iter()
                    .all(|target| refreshed.slots.contains(target))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use alloy_network::Ethereum;
    use alloy_primitives::{B256, Bytes, Log as PrimitiveLog};
    use alloy_provider::{RootProvider, network::AnyNetwork};
    use alloy_rpc_client::RpcClient;
    use alloy_rpc_types_eth::{Filter, Log as RpcLog};
    use alloy_transport::mock::Asserter;
    use evm_fork_cache::reactive::{
        BlockRef, ChainStatus, HandlerError, HandlerOutcome, InputSource, LogInterest,
        ReactiveContext, ReactiveEffect, ReactiveError, ReactiveHandler, ReactiveInput,
        ReactiveInputBatch, ReactiveInputRecord, ReactiveInterest, StateEffectQuality,
    };

    use super::super::{
        AdapterEvent, AdapterEventKind, AdapterEventResult, AdapterGeneration, AdapterInstanceId,
        AdapterKey, AdapterRegistry, AmmAdapter, CustomPoolKey, EventSource, PoolStatus,
        ProtocolId, RepairAction, StateUpdate, StateView, UpdateQuality,
    };
    use super::*;

    fn address_wide_dependencies(address: Address) -> PoolStateDependencies {
        PoolStateDependencies::default().with_whole_accounts([address])
    }

    fn indexed_pool(
        key: PoolKey,
        dependencies: PoolStateDependencies,
    ) -> (AmmOwnershipIndex, PoolInstanceId) {
        let protocol = key.protocol();
        let adapter =
            AdapterInstanceId::new(AdapterKey::new(protocol, []), AdapterGeneration::new(0));
        let instance = PoolInstanceId::new(key, PoolGeneration::new(0));
        let mut ownership = AmmOwnershipIndex::default();
        ownership.insert_adapter(adapter.clone()).unwrap();
        ownership
            .insert_pool(PoolOwnership::new(instance.clone(), adapter, dependencies, []).unwrap())
            .unwrap();
        (ownership, instance)
    }

    #[test]
    fn ownership_refresh_preserves_generation_work_and_resyncs() {
        let address = Address::repeat_byte(0x71);
        let old_slot = StateSlot::new(address, U256::from(1));
        let new_slot = StateSlot::new(address, U256::from(2));
        let new_emitter = Address::repeat_byte(0x73);
        let key = PoolKey::Custom(CustomPoolKey::Address {
            protocol: "refresh-owned-work",
            address,
        });
        let (mut ownership, instance) =
            indexed_pool(key, PoolStateDependencies::default().with_slots([old_slot]));
        let adapter = ownership
            .adapter_for_pool(&instance)
            .expect("pool has an adapter")
            .clone();
        let work = RuntimeWorkId::new(
            RuntimeOwnerId::Pool(instance.clone()),
            super::super::WorkId::new(7),
        );
        let resync = ResyncId::new("refresh-owned-resync");
        ownership.track_work(work.clone()).unwrap();
        ownership
            .track_resync(instance.clone(), resync.clone())
            .unwrap();

        ownership
            .replace_pool(
                PoolOwnership::new(
                    instance.clone(),
                    adapter,
                    PoolStateDependencies::default().with_slots([new_slot]),
                    [new_emitter],
                )
                .unwrap(),
            )
            .unwrap();

        assert!(ownership.pools_for_slot(old_slot).is_empty());
        assert_eq!(ownership.pools_for_slot(new_slot), vec![instance.clone()]);
        assert_eq!(
            ownership.pools_for_emitter(new_emitter),
            vec![instance.clone()]
        );
        assert_eq!(
            ownership.work_for_owner(&RuntimeOwnerId::Pool(instance.clone())),
            vec![work]
        );
        assert_eq!(ownership.resync_owner(&resync), Some(&instance));
    }

    #[test]
    fn committed_refresh_clears_explicit_recovery_fence() {
        let address = Address::repeat_byte(0x74);
        let topic = B256::repeat_byte(0x75);
        let slot = U256::from(3);
        let key = PoolKey::Custom(CustomPoolKey::Address {
            protocol: FENCE_PROTOCOL,
            address,
        });
        let mut registry = AdapterRegistry::new();
        registry
            .register_adapter(Arc::new(FenceAdapter { topic, slot }))
            .unwrap();
        registry
            .register_pool(
                PoolRegistration::new(key.clone())
                    .with_state_address(address)
                    .with_status(PoolStatus::Ready),
            )
            .unwrap();
        let mut engine = AmmSyncEngine::new(registry).unwrap();
        let instance = engine
            .ownership
            .active_pool(&key)
            .expect("pool is active")
            .clone();
        engine.degraded_targets.insert(
            key.clone(),
            PendingTargets {
                require_explicit_refresh: true,
                ..PendingTargets::default()
            },
        );
        engine.set_pool_status(&key, PoolStatus::Degraded);

        let prepared = engine
            .prepare_pool_refresh(
                instance,
                PoolRegistration::new(key.clone())
                    .with_state_address(address)
                    .with_status(PoolStatus::Ready),
            )
            .unwrap();
        engine.commit_pool_refresh(prepared).unwrap();

        assert!(!engine.degraded_targets.contains_key(&key));
        assert_eq!(
            engine
                .registry
                .pool(&key)
                .expect("pool remains active")
                .status,
            PoolStatus::Ready
        );
    }

    fn refreshed(slots: impl IntoIterator<Item = (Address, U256)>) -> ResyncRefresh {
        ResyncRefresh {
            slots: slots.into_iter().collect(),
        }
    }

    #[test]
    fn tracked_target_recovers_only_on_that_slot() {
        // The reviewer's case: a pool degraded by a failed repair on `target`
        // must NOT recover when an unrelated slot at the same address refreshes —
        // only when `target` itself does.
        let address = Address::repeat_byte(0xaa);
        let target = U256::from(1);
        let unrelated = U256::from(2);
        let dependencies = address_wide_dependencies(address);
        let pending = PendingTargets {
            slots: vec![(address, target)],
            require_explicit_refresh: false,
        };

        assert!(!should_recover(
            &dependencies,
            Some(&pending),
            &refreshed([(address, unrelated)])
        ));
        assert!(should_recover(
            &dependencies,
            Some(&pending),
            &refreshed([(address, target)])
        ));
        // A superset (target plus others) also recovers.
        assert!(should_recover(
            &dependencies,
            Some(&pending),
            &refreshed([(address, unrelated), (address, target)]),
        ));
    }

    #[test]
    fn untracked_address_wide_pool_does_not_recover_from_one_arbitrary_slot() {
        let address = Address::repeat_byte(0xbb);
        let dependencies = address_wide_dependencies(address);
        assert!(!should_recover(
            &dependencies,
            None,
            &refreshed([(address, U256::from(9))]),
        ));
    }

    #[test]
    fn explicit_refresh_barrier_cannot_be_cleared_by_ordinary_resync() {
        let address = Address::repeat_byte(0xdd);
        let dependencies = address_wide_dependencies(address);
        let pending = PendingTargets {
            require_explicit_refresh: true,
            ..PendingTargets::default()
        };
        assert!(!should_recover(
            &dependencies,
            Some(&pending),
            &refreshed([(address, U256::from(3))])
        ));
    }

    #[test]
    fn untracked_enumerable_read_set_requires_every_slot() {
        let address = Address::repeat_byte(0xde);
        let slot_a = U256::from(3);
        let slot_b = U256::from(4);
        let dependencies = PoolStateDependencies::default().with_slots([
            StateSlot::new(address, slot_a),
            StateSlot::new(address, slot_b),
        ]);
        assert!(!should_recover(
            &dependencies,
            None,
            &refreshed([(address, slot_a)])
        ));
        assert!(should_recover(
            &dependencies,
            None,
            &refreshed([(address, slot_b), (address, slot_a)])
        ));
    }

    #[test]
    fn covered_records_only_slots_the_pool_covers() {
        let address = Address::repeat_byte(0xee);
        let elsewhere = Address::repeat_byte(0x01);
        let (ownership, pool) = indexed_pool(
            PoolKey::UniswapV3(address),
            address_wide_dependencies(address),
        );

        let here = ResyncTarget::StorageSlots {
            address,
            slots: vec![U256::from(1), U256::from(2)],
        };
        let recorded = PendingTargets::covered(&here, &pool, &ownership);
        assert_eq!(
            recorded.slots,
            vec![(address, U256::from(1)), (address, U256::from(2))],
        );

        // A slot on a different address is not this pool's to recover on.
        let other = ResyncTarget::StorageSlot {
            address: elsewhere,
            slot: U256::from(5),
        };
        assert!(PendingTargets::covered(&other, &pool, &ownership).is_empty());
    }

    #[test]
    fn partially_effective_multi_slot_purge_is_reported_as_ambiguous() {
        let address = Address::repeat_byte(0xef);
        let slot_a = StateSlot::new(address, U256::from(1));
        let slot_b = StateSlot::new(address, U256::from(2));
        let adapter = AdapterInstanceId::new(
            AdapterKey::new(ProtocolId::Curve, []),
            AdapterGeneration::new(0),
        );
        let pool_a = PoolInstanceId::new(
            PoolKey::Curve(Address::repeat_byte(0xa1)),
            PoolGeneration::new(0),
        );
        let pool_b = PoolInstanceId::new(
            PoolKey::Curve(Address::repeat_byte(0xb1)),
            PoolGeneration::new(0),
        );
        let mut ownership = AmmOwnershipIndex::default();
        ownership.insert_adapter(adapter.clone()).unwrap();
        ownership
            .insert_pool(
                PoolOwnership::new(
                    pool_a,
                    adapter.clone(),
                    PoolStateDependencies::default().with_slots([slot_a]),
                    [],
                )
                .unwrap(),
            )
            .unwrap();
        ownership
            .insert_pool(
                PoolOwnership::new(
                    pool_b,
                    adapter,
                    PoolStateDependencies::default().with_slots([slot_b]),
                    [],
                )
                .unwrap(),
            )
            .unwrap();

        let purge = evm_fork_cache::PurgeRecord {
            address,
            scope: evm_fork_cache::PurgeScope::Slots(vec![slot_a.slot(), slot_b.slot()]),
            slots_removed: 1,
            account_removed: false,
        };
        let mut changes = BTreeMap::new();
        record_purge_change(
            &mut changes,
            &ownership,
            &purge,
            AmmSyncChangeSource::Direct,
        );

        assert_eq!(changes.len(), 2);
        assert!(changes.values().all(|change| {
            change.kind == AmmSyncPoolChangeKind::UnknownImpact
                && change.source == AmmSyncChangeSource::Unknown
        }));
    }

    const PENDING_PROTOCOL: &str = "stage3-pending-continuity";
    const FENCE_PROTOCOL: &str = "stage3-eviction-fence";

    struct SharedPendingAdapter {
        vault: Address,
        topic: B256,
    }

    struct FenceAdapter {
        topic: B256,
        slot: U256,
    }

    impl AmmAdapter for FenceAdapter {
        fn protocol(&self) -> ProtocolId {
            ProtocolId::Custom(FENCE_PROTOCOL)
        }

        fn event_sources(&self, pool: &PoolRegistration) -> Vec<EventSource> {
            pool.key
                .address()
                .map(|address| EventSource::direct(address, vec![self.topic]))
                .into_iter()
                .collect()
        }

        fn state_dependencies(&self, pool: &PoolRegistration) -> PoolStateDependencies {
            let address = pool.key.address().expect("test pool is address keyed");
            PoolStateDependencies::default()
                .with_associated_addresses([address])
                .with_slots([StateSlot::new(address, self.slot)])
        }

        fn decode_event(
            &self,
            pool: &PoolRegistration,
            log: &PrimitiveLog,
            _view: &dyn StateView,
        ) -> AdapterEventResult {
            AdapterEventResult::event(
                AdapterEvent::new(
                    pool.key.clone(),
                    log.address,
                    self.topic,
                    AdapterEventKind::Unknown,
                    UpdateQuality::Exact,
                )
                .with_updates([StateUpdate::slot(
                    log.address,
                    self.slot,
                    U256::from(100),
                )]),
            )
        }
    }

    struct ConflictWriter {
        id: &'static str,
        emitter: Address,
        slot: U256,
        value: U256,
    }

    impl ReactiveHandler<Ethereum> for ConflictWriter {
        fn id(&self) -> HandlerId {
            HandlerId::new(self.id)
        }

        fn interests(&self) -> Vec<ReactiveInterest<Ethereum>> {
            vec![ReactiveInterest::Logs(LogInterest {
                provider_filter: Filter::new().address(self.emitter),
                local_matcher: None,
                route_key: None,
            })]
        }

        fn handle(
            &self,
            _ctx: &ReactiveContext,
            _input: &ReactiveInput<Ethereum>,
            _state: &dyn evm_fork_cache::StateView,
        ) -> Result<HandlerOutcome, HandlerError> {
            Ok(HandlerOutcome {
                effects: vec![ReactiveEffect::StateUpdate(
                    evm_fork_cache::StateUpdate::slot(self.emitter, self.slot, self.value),
                )],
                quality: StateEffectQuality::ExactFromInput,
                tags: Vec::new(),
            })
        }
    }

    impl SharedPendingAdapter {
        fn slot(pool: &PoolRegistration) -> U256 {
            U256::from(
                pool.key
                    .address()
                    .expect("test pool is address keyed")
                    .as_slice()[19],
            )
        }
    }

    impl AmmAdapter for SharedPendingAdapter {
        fn protocol(&self) -> ProtocolId {
            ProtocolId::Custom(PENDING_PROTOCOL)
        }

        fn state_dependencies(&self, pool: &PoolRegistration) -> PoolStateDependencies {
            PoolStateDependencies::default()
                .with_associated_addresses([self.vault])
                .with_slots([StateSlot::new(self.vault, Self::slot(pool))])
        }

        fn decode_event(
            &self,
            pool: &PoolRegistration,
            log: &PrimitiveLog,
            _view: &dyn StateView,
        ) -> AdapterEventResult {
            AdapterEventResult::event(
                AdapterEvent::new(
                    pool.key.clone(),
                    log.address,
                    self.topic,
                    AdapterEventKind::Unknown,
                    UpdateQuality::RequiresRepair,
                )
                .with_repair(RepairAction::VerifySlots(vec![(
                    self.vault,
                    Self::slot(pool),
                )])),
            )
        }
    }

    fn pending_key(address: Address) -> PoolKey {
        PoolKey::Custom(CustomPoolKey::Address {
            protocol: PENDING_PROTOCOL,
            address,
        })
    }

    fn pending_registration(key: PoolKey, emitter: Address, topic: B256) -> PoolRegistration {
        PoolRegistration::new(key.clone())
            .with_status(PoolStatus::Ready)
            .with_event_source(EventSource::indexed_address(emitter, vec![topic], 1))
    }

    fn pending_batch(
        emitter: Address,
        topic: B256,
        pool: Address,
        block_number: u64,
    ) -> ReactiveInputBatch<Ethereum> {
        let mut indexed_pool = [0_u8; 32];
        indexed_pool[12..].copy_from_slice(pool.as_slice());
        let block = BlockRef {
            number: block_number,
            hash: B256::repeat_byte(block_number as u8),
            parent_hash: Some(B256::repeat_byte(block_number.saturating_sub(1) as u8)),
            timestamp: Some(block_number),
        };
        ReactiveInputBatch::new(vec![ReactiveInputRecord::new(
            ReactiveInput::Log(RpcLog {
                inner: PrimitiveLog::new_unchecked(
                    emitter,
                    vec![topic, B256::from(indexed_pool)],
                    Bytes::new(),
                ),
                block_hash: Some(block.hash),
                block_number: Some(block.number),
                transaction_hash: Some(B256::repeat_byte((block_number + 10) as u8)),
                transaction_index: Some(0),
                log_index: Some(0),
                ..RpcLog::default()
            }),
            ReactiveContext {
                chain_id: Some(1),
                source: InputSource::Synthetic,
                chain_status: ChainStatus::Included {
                    block: block.clone(),
                    confirmations: 0,
                },
                block: Some(block),
                transaction_index: Some(0),
                log_index: Some(0),
            },
        )])
    }

    #[tokio::test]
    async fn pool_removal_cancels_only_its_exact_pending_resync_ids() -> anyhow::Result<()> {
        let vault = Address::repeat_byte(0xf0);
        let emitter = Address::repeat_byte(0xf1);
        let topic = B256::repeat_byte(0xf2);
        let address_a = Address::repeat_byte(0xa1);
        let address_b = Address::repeat_byte(0xb1);
        let key_a = pending_key(address_a);
        let key_b = pending_key(address_b);
        let adapter = Arc::new(SharedPendingAdapter { vault, topic });
        let mut registry = AdapterRegistry::new();
        registry.register_adapter(adapter)?;
        registry.register_pool(pending_registration(key_a.clone(), emitter, topic))?;
        registry.register_pool(pending_registration(key_b.clone(), emitter, topic))?;
        let mut engine = AmmSyncEngine::new(registry)?;
        let provider = RootProvider::<AnyNetwork>::new(RpcClient::mocked(Asserter::new()));
        let mut cache = EvmCache::new(Arc::new(provider)).await;

        let report_a = engine
            .runtime
            .ingest_batch(&mut cache, pending_batch(emitter, topic, address_a, 1))?;
        let request_a = report_a.applied[0].resyncs[0].id.clone();
        let instance_a = engine
            .ownership
            .active_pool(&key_a)
            .cloned()
            .expect("pool A instance");
        engine
            .ownership
            .track_resync(instance_a, request_a.clone())?;

        let report_b = engine
            .runtime
            .ingest_batch(&mut cache, pending_batch(emitter, topic, address_b, 2))?;
        let request_b = report_b.applied[0].resyncs[0].id.clone();
        let instance_b = engine
            .ownership
            .active_pool(&key_b)
            .cloned()
            .expect("pool B instance");
        engine
            .ownership
            .track_resync(instance_b.clone(), request_b.clone())?;
        assert_eq!(engine.runtime.pending_resyncs().len(), 2);

        let removed = engine.remove_pools(std::slice::from_ref(&key_a))?;

        assert_eq!(
            removed.removed_pools()[0].detached_resyncs(),
            std::slice::from_ref(&request_a)
        );
        assert_eq!(removed.removed_pools()[0].cancelled_resyncs().len(), 1);
        assert_eq!(
            removed.removed_pools()[0].cancelled_resyncs()[0].id,
            request_a
        );
        assert_eq!(engine.runtime.pending_resyncs().len(), 1);
        assert_eq!(engine.runtime.pending_resyncs()[0].id, request_b);
        assert_eq!(
            engine
                .ownership
                .resync_owner(&engine.runtime.pending_resyncs()[0].id),
            Some(&instance_b)
        );
        Ok(())
    }

    #[tokio::test]
    async fn ingest_error_after_reorg_reapplies_eviction_fence() -> anyhow::Result<()> {
        let pool = Address::repeat_byte(0xf3);
        let topic = B256::repeat_byte(0xf4);
        let slot = U256::from(9);
        let key = PoolKey::Custom(CustomPoolKey::Address {
            protocol: FENCE_PROTOCOL,
            address: pool,
        });
        let mut registry = AdapterRegistry::new();
        registry.register_adapter(Arc::new(FenceAdapter { topic, slot }))?;
        registry.register_pool(
            PoolRegistration::new(key.clone())
                .with_state_address(pool)
                .with_status(PoolStatus::Ready),
        )?;
        let mut engine = AmmSyncEngine::new(registry)?;
        let provider = RootProvider::<AnyNetwork>::new(RpcClient::mocked(Asserter::new()));
        let mut cache = EvmCache::new(Arc::new(provider)).await;
        let canonical_block = BlockRef {
            number: 1,
            hash: B256::repeat_byte(1),
            parent_hash: Some(B256::ZERO),
            timestamp: Some(1),
        };
        let canonical_log = RpcLog {
            inner: PrimitiveLog::new_unchecked(pool, vec![topic], Bytes::new()),
            block_hash: Some(canonical_block.hash),
            block_number: Some(canonical_block.number),
            transaction_hash: Some(B256::repeat_byte(0xf5)),
            transaction_index: Some(0),
            log_index: Some(0),
            ..RpcLog::default()
        };
        engine.ingest_batch(
            &mut cache,
            ReactiveInputBatch::new(vec![ReactiveInputRecord::new(
                ReactiveInput::Log(canonical_log.clone()),
                ReactiveContext {
                    chain_id: Some(1),
                    source: InputSource::Synthetic,
                    chain_status: ChainStatus::Included {
                        block: canonical_block.clone(),
                        confirmations: 0,
                    },
                    block: Some(canonical_block.clone()),
                    transaction_index: Some(0),
                    log_index: Some(0),
                },
            )]),
        )?;
        assert_eq!(
            cache.cached_storage_value(pool, slot),
            Some(U256::from(100))
        );
        engine.remove_pools_evicting(std::slice::from_ref(&key), &mut cache)?;
        assert_eq!(cache.cached_storage_value(pool, slot), None);

        let conflict_emitter = Address::repeat_byte(0xf6);
        for (id, value) in [("conflict-a", 1_u64), ("conflict-b", 2_u64)] {
            engine.runtime.register_handler(Arc::new(ConflictWriter {
                id,
                emitter: conflict_emitter,
                slot: U256::from(10),
                value: U256::from(value),
            }))?;
        }
        let mut removed_log = canonical_log;
        removed_log.removed = true;
        let conflict_block = BlockRef {
            number: 2,
            hash: B256::repeat_byte(2),
            parent_hash: Some(B256::ZERO),
            timestamp: Some(2),
        };
        let conflict_log = RpcLog {
            inner: PrimitiveLog::new_unchecked(
                conflict_emitter,
                vec![B256::repeat_byte(0xf7)],
                Bytes::new(),
            ),
            block_hash: Some(conflict_block.hash),
            block_number: Some(conflict_block.number),
            transaction_hash: Some(B256::repeat_byte(0xf8)),
            transaction_index: Some(0),
            log_index: Some(0),
            ..RpcLog::default()
        };
        let error = engine
            .ingest_batch(
                &mut cache,
                ReactiveInputBatch::new(vec![
                    ReactiveInputRecord::new(
                        ReactiveInput::Log(removed_log),
                        ReactiveContext {
                            chain_id: Some(1),
                            source: InputSource::Synthetic,
                            chain_status: ChainStatus::Reorged {
                                dropped_from: canonical_block.clone(),
                            },
                            block: Some(canonical_block),
                            transaction_index: Some(0),
                            log_index: Some(0),
                        },
                    ),
                    ReactiveInputRecord::new(
                        ReactiveInput::Log(conflict_log),
                        ReactiveContext {
                            chain_id: Some(1),
                            source: InputSource::Synthetic,
                            chain_status: ChainStatus::Included {
                                block: conflict_block.clone(),
                                confirmations: 0,
                            },
                            block: Some(conflict_block),
                            transaction_index: Some(0),
                            log_index: Some(0),
                        },
                    ),
                ]),
            )
            .expect_err("the later input must fail after reorg recovery mutates the cache");

        assert!(matches!(
            error,
            AmmSyncError::Reactive(ReactiveError::ConflictingEffects { .. })
        ));
        assert_eq!(
            cache.cached_storage_value(pool, slot),
            None,
            "the error path must fence rollback writes before propagating"
        );
        Ok(())
    }
}
