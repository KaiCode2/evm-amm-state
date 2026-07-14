//! Protocol-neutral AMM live-runtime domain vocabulary.
//!
//! These types identify asynchronous work and published state independently of
//! the actor/runtime implementation that will consume them in later stages.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
#[cfg(feature = "live-runtime")]
use std::sync::atomic::{AtomicU64, Ordering};

use alloy_primitives::{Address, B256};

use super::{PoolKey, PoolStatus, ProtocolId};

#[cfg(feature = "live-runtime")]
static NEXT_AMM_RUNTIME_ID: AtomicU64 = AtomicU64::new(1);

/// Process-unique lineage of one cache-owning AMM runtime.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AmmRuntimeId(u64);

impl AmmRuntimeId {
    #[cfg(feature = "live-runtime")]
    pub(crate) fn allocate() -> Self {
        let value = NEXT_AMM_RUNTIME_ID
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                value.checked_add(1)
            })
            .expect("AMM runtime identity exhausted");
        Self(value)
    }

    /// Numeric process-local runtime identity.
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Error returned when a monotonic runtime sequence would overflow.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RuntimeSequenceOverflow {
    sequence: &'static str,
}

impl RuntimeSequenceOverflow {
    /// Construct an overflow error for `sequence`.
    pub const fn new(sequence: &'static str) -> Self {
        Self { sequence }
    }

    /// Name of the exhausted sequence type.
    pub const fn sequence(self) -> &'static str {
        self.sequence
    }
}

impl fmt::Display for RuntimeSequenceOverflow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} sequence exhausted", self.sequence)
    }
}

impl std::error::Error for RuntimeSequenceOverflow {}

/// Monotonic generation assigned to one accepted [`PoolKey`] registration.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PoolGeneration(u64);

impl PoolGeneration {
    /// Construct a generation from its persisted/runtime counter value.
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Return the numeric generation.
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Return the next generation, rejecting exhaustion rather than wrapping.
    pub fn checked_next(self) -> Result<Self, RuntimeSequenceOverflow> {
        self.0
            .checked_add(1)
            .map(Self)
            .ok_or_else(|| RuntimeSequenceOverflow::new("PoolGeneration"))
    }
}

/// Runtime identity of one concrete pool registration instance.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PoolInstanceId {
    key: PoolKey,
    generation: PoolGeneration,
}

impl PoolInstanceId {
    /// Construct a pool instance identity.
    pub const fn new(key: PoolKey, generation: PoolGeneration) -> Self {
        Self { key, generation }
    }

    /// Logical pool identity shared by replacement generations.
    pub const fn key(&self) -> &PoolKey {
        &self.key
    }

    /// Generation of this accepted registration.
    pub const fn generation(&self) -> PoolGeneration {
        self.generation
    }

    /// Consume the identity into its logical key and generation.
    pub fn into_parts(self) -> (PoolKey, PoolGeneration) {
        (self.key, self.generation)
    }
}

/// Stable logical key for one registered adapter family.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AdapterKey {
    protocols: Vec<ProtocolId>,
}

impl AdapterKey {
    /// Construct a canonical non-empty adapter-family key.
    ///
    /// `primary` guarantees the family is non-empty. All protocol ids are
    /// sorted and de-duplicated, so caller ordering cannot split one adapter
    /// family into several generation domains.
    pub fn new(primary: ProtocolId, additional: impl IntoIterator<Item = ProtocolId>) -> Self {
        let mut protocols = vec![primary];
        protocols.extend(additional);
        protocols.sort_unstable();
        protocols.dedup();
        Self { protocols }
    }

    /// Canonical sorted protocol ids served by the adapter family.
    pub fn protocols(&self) -> &[ProtocolId] {
        &self.protocols
    }
}

/// Monotonic generation assigned to one accepted adapter-family registration.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AdapterGeneration(u64);

impl AdapterGeneration {
    /// Construct an adapter generation.
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Return the numeric generation.
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Return the next generation, rejecting exhaustion rather than wrapping.
    pub fn checked_next(self) -> Result<Self, RuntimeSequenceOverflow> {
        self.0
            .checked_add(1)
            .map(Self)
            .ok_or_else(|| RuntimeSequenceOverflow::new("AdapterGeneration"))
    }
}

/// Runtime identity of one concrete adapter-family registration.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AdapterInstanceId {
    key: AdapterKey,
    generation: AdapterGeneration,
}

impl AdapterInstanceId {
    /// Construct an adapter instance identity.
    pub const fn new(key: AdapterKey, generation: AdapterGeneration) -> Self {
        Self { key, generation }
    }

    /// Logical adapter-family key.
    pub const fn key(&self) -> &AdapterKey {
        &self.key
    }

    /// Generation of this accepted adapter registration.
    pub const fn generation(&self) -> AdapterGeneration {
        self.generation
    }
}

/// Stable logical key for one discovery source or factory watcher.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DiscoveryOwnerKey(String);

impl DiscoveryOwnerKey {
    /// Construct a discovery-owner key.
    pub fn new(key: impl Into<String>) -> Self {
        Self(key.into())
    }

    /// Borrow the stable key.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Monotonic generation assigned to one accepted discovery-owner registration.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DiscoveryGeneration(u64);

impl DiscoveryGeneration {
    /// Construct a discovery generation.
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Return the numeric generation.
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Return the next generation, rejecting exhaustion rather than wrapping.
    pub fn checked_next(self) -> Result<Self, RuntimeSequenceOverflow> {
        self.0
            .checked_add(1)
            .map(Self)
            .ok_or_else(|| RuntimeSequenceOverflow::new("DiscoveryGeneration"))
    }
}

/// Runtime identity of one concrete discovery source or watcher.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DiscoveryOwnerId {
    key: DiscoveryOwnerKey,
    generation: DiscoveryGeneration,
}

impl DiscoveryOwnerId {
    /// Construct a discovery-owner identity.
    pub const fn new(key: DiscoveryOwnerKey, generation: DiscoveryGeneration) -> Self {
        Self { key, generation }
    }

    /// Logical discovery-owner key.
    pub const fn key(&self) -> &DiscoveryOwnerKey {
        &self.key
    }

    /// Generation of this accepted discovery-owner registration.
    pub const fn generation(&self) -> DiscoveryGeneration {
        self.generation
    }
}

/// Generation-scoped owner of asynchronous runtime work.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum RuntimeOwnerId {
    /// Work owned by one concrete pool registration.
    Pool(PoolInstanceId),
    /// Work owned by one concrete adapter-family registration.
    Adapter(AdapterInstanceId),
    /// Work owned by one concrete discovery source or watcher.
    Discovery(DiscoveryOwnerId),
}

/// Monotonic identifier for one scheduled attempt under a runtime owner.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WorkId(u64);

impl WorkId {
    /// Construct a work identifier.
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Return the numeric identifier.
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Return the next work identifier, rejecting exhaustion rather than wrapping.
    pub fn checked_next(self) -> Result<Self, RuntimeSequenceOverflow> {
        self.0
            .checked_add(1)
            .map(Self)
            .ok_or_else(|| RuntimeSequenceOverflow::new("WorkId"))
    }
}

/// Position represented by a canonical AMM state point.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum StatePosition {
    /// State after every transaction and log in the identified block.
    PostBlock,
}

/// Chain provenance for one coherent AMM state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AmmStatePoint {
    chain_id: u64,
    block_number: u64,
    block_hash: B256,
    position: StatePosition,
}

impl AmmStatePoint {
    /// Construct a post-block state point.
    pub const fn post_block(chain_id: u64, block_number: u64, block_hash: B256) -> Self {
        Self {
            chain_id,
            block_number,
            block_hash,
            position: StatePosition::PostBlock,
        }
    }

    /// Chain identifier.
    pub const fn chain_id(self) -> u64 {
        self.chain_id
    }

    /// Block number.
    pub const fn block_number(self) -> u64 {
        self.block_number
    }

    /// Canonical block hash.
    pub const fn block_hash(self) -> B256 {
        self.block_hash
    }

    /// State position within the block.
    pub const fn position(self) -> StatePosition {
        self.position
    }

    /// First block whose events are absent from this state point.
    pub fn first_unapplied_block(self) -> Result<u64, RuntimeSequenceOverflow> {
        match self.position {
            StatePosition::PostBlock => self
                .block_number
                .checked_add(1)
                .ok_or_else(|| RuntimeSequenceOverflow::new("AmmStatePoint.block_number")),
        }
    }
}

/// Monotonic version of a coherent published AMM state snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AmmStateVersion(u64);

impl AmmStateVersion {
    /// Initial snapshot version.
    pub const fn initial() -> Self {
        Self(0)
    }

    /// Construct a state version.
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Return the numeric version.
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Return the next version, rejecting exhaustion rather than wrapping.
    pub fn checked_next(self) -> Result<Self, RuntimeSequenceOverflow> {
        self.0
            .checked_add(1)
            .map(Self)
            .ok_or_else(|| RuntimeSequenceOverflow::new("AmmStateVersion"))
    }
}

/// Monotonic revision of one pool's quote-relevant state and eligibility.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PoolStateRevision(u64);

impl PoolStateRevision {
    /// Construct a pool-state revision.
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Return the numeric revision.
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Return the next revision, rejecting exhaustion rather than wrapping.
    pub fn checked_next(self) -> Result<Self, RuntimeSequenceOverflow> {
        self.0
            .checked_add(1)
            .map(Self)
            .ok_or_else(|| RuntimeSequenceOverflow::new("PoolStateRevision"))
    }
}

/// Complete provenance key for one pool's quote-relevant state.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PoolStateRef {
    pool: PoolInstanceId,
    revision: PoolStateRevision,
    point: AmmStatePoint,
}

impl PoolStateRef {
    /// Construct a pool-state reference.
    pub const fn new(
        pool: PoolInstanceId,
        revision: PoolStateRevision,
        point: AmmStatePoint,
    ) -> Self {
        Self {
            pool,
            revision,
            point,
        }
    }

    /// Pool registration instance.
    pub const fn pool(&self) -> &PoolInstanceId {
        &self.pool
    }

    /// Quote-relevant revision.
    pub const fn revision(&self) -> PoolStateRevision {
        self.revision
    }

    /// Chain point at which this state is valid.
    pub const fn point(&self) -> AmmStatePoint {
        self.point
    }
}

/// Operational scheduler/transport state of one pool registration.
///
/// This is a sidecar to [`PoolStatus`], which continues to express adapter and
/// quote health.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum PoolRuntimeState {
    /// Metadata was discovered but no work has been accepted.
    Discovered,
    /// A generation-scoped hydration job is queued.
    Queued,
    /// Cold-start provider work is in flight or ready to commit.
    Hydrating,
    /// A verified staging state is catching up to a canonical commit fence.
    CatchingUp,
    /// A coherent published snapshot can quote the pool.
    Searchable,
    /// Catch-up and subsequent live delivery are continuous.
    Live,
    /// Current state quality is insufficient for normal search policy.
    Degraded,
    /// Teardown is rejecting new work and removing ownership.
    Removing,
    /// No runtime ownership remains for this generation.
    Removed,
    /// The current pre-searchable attempt failed; retry is explicit.
    Failed,
}

impl PoolRuntimeState {
    /// Required/normal adapter-facing status for this operational state.
    ///
    /// `None` means teardown/failure preserves the last adapter classification.
    pub const fn required_pool_status(self) -> Option<PoolStatus> {
        match self {
            Self::Discovered | Self::Queued => Some(PoolStatus::Pending),
            Self::Hydrating | Self::CatchingUp => Some(PoolStatus::Cold),
            Self::Searchable | Self::Live => Some(PoolStatus::Ready),
            Self::Degraded => Some(PoolStatus::Degraded),
            Self::Removing | Self::Removed | Self::Failed => None,
        }
    }

    /// Whether `next` is a legal lifecycle transition.
    pub const fn can_transition_to(self, next: Self) -> bool {
        if matches!(next, Self::Removing) {
            return !matches!(self, Self::Removing | Self::Removed);
        }

        matches!(
            (self, next),
            (Self::Discovered, Self::Queued)
                | (Self::Queued, Self::Hydrating | Self::Failed)
                | (Self::Hydrating, Self::CatchingUp | Self::Failed)
                | (Self::CatchingUp, Self::Searchable | Self::Failed)
                | (
                    Self::Searchable,
                    Self::Live | Self::CatchingUp | Self::Degraded
                )
                | (Self::Live, Self::CatchingUp | Self::Degraded)
                | (Self::Degraded, Self::CatchingUp | Self::Live)
                | (Self::Failed, Self::Queued)
                | (Self::Removing, Self::Removed)
        )
    }
}

/// Rejected transition between operational pool lifecycle states.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InvalidPoolRuntimeTransition {
    from: PoolRuntimeState,
    to: PoolRuntimeState,
}

impl InvalidPoolRuntimeTransition {
    /// Construct a transition error.
    pub const fn new(from: PoolRuntimeState, to: PoolRuntimeState) -> Self {
        Self { from, to }
    }

    /// Current state.
    pub const fn from(self) -> PoolRuntimeState {
        self.from
    }

    /// Rejected target state.
    pub const fn to(self) -> PoolRuntimeState {
        self.to
    }
}

impl fmt::Display for InvalidPoolRuntimeTransition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid pool runtime transition: {:?} -> {:?}",
            self.from, self.to
        )
    }
}

impl std::error::Error for InvalidPoolRuntimeTransition {}

/// Checked operational lifecycle for one concrete pool registration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PoolLifecycle {
    pool: PoolInstanceId,
    state: PoolRuntimeState,
}

impl PoolLifecycle {
    /// Construct a lifecycle at `state`.
    pub const fn new(pool: PoolInstanceId, state: PoolRuntimeState) -> Self {
        Self { pool, state }
    }

    /// Pool registration instance governed by this lifecycle.
    pub const fn pool(&self) -> &PoolInstanceId {
        &self.pool
    }

    /// Current operational state.
    pub const fn state(&self) -> PoolRuntimeState {
        self.state
    }

    /// Apply one checked lifecycle transition.
    pub fn transition_to(
        &mut self,
        next: PoolRuntimeState,
    ) -> Result<(), InvalidPoolRuntimeTransition> {
        if !self.state.can_transition_to(next) {
            return Err(InvalidPoolRuntimeTransition::new(self.state, next));
        }
        self.state = next;
        Ok(())
    }
}

/// Stable identifier for manual/configuration registration evidence.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RegistrationSourceKey(String);

impl RegistrationSourceKey {
    /// Construct a stable source key.
    pub fn new(key: impl Into<String>) -> Self {
        Self(key.into())
    }

    /// Borrow the stable source key.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Reorg policy for pool evidence obtained from a state query.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum QueryEvidencePolicy {
    /// The query was accepted at a finalized point and remains stable evidence.
    Finalized,
    /// An orphaned query point must be checked again on the canonical chain.
    RevalidateOnReorg,
}

/// One independent piece of evidence supporting a pool registration.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum RegistrationProvenance {
    /// Stable manual or configuration evidence.
    Stable {
        /// Stable configuration/source identity.
        source: RegistrationSourceKey,
    },
    /// Evidence obtained from a state query at one block.
    StateQuery {
        /// Generation-scoped discovery source.
        owner: DiscoveryOwnerId,
        /// Chain identifier.
        chain_id: u64,
        /// Queried block number.
        block_number: u64,
        /// Queried block hash.
        block_hash: B256,
        /// Finality/revalidation policy.
        policy: QueryEvidencePolicy,
    },
    /// Evidence obtained from one factory creation log.
    FactoryLog {
        /// Generation-scoped factory watcher.
        owner: DiscoveryOwnerId,
        /// Factory that emitted the log.
        factory: Address,
        /// Chain identifier.
        chain_id: u64,
        /// Block number containing the log.
        block_number: u64,
        /// Block hash containing the log.
        block_hash: B256,
        /// Transaction hash that emitted the log.
        transaction_hash: B256,
        /// Log index within the block.
        log_index: u64,
    },
}

impl RegistrationProvenance {
    /// Construct stable manual/configuration evidence.
    pub const fn stable(source: RegistrationSourceKey) -> Self {
        Self::Stable { source }
    }

    /// Construct state-query evidence.
    pub const fn state_query(
        owner: DiscoveryOwnerId,
        chain_id: u64,
        block_number: u64,
        block_hash: B256,
        policy: QueryEvidencePolicy,
    ) -> Self {
        Self::StateQuery {
            owner,
            chain_id,
            block_number,
            block_hash,
            policy,
        }
    }

    /// Construct factory-log evidence.
    #[allow(clippy::too_many_arguments)]
    pub const fn factory_log(
        owner: DiscoveryOwnerId,
        factory: Address,
        chain_id: u64,
        block_number: u64,
        block_hash: B256,
        transaction_hash: B256,
        log_index: u64,
    ) -> Self {
        Self::FactoryLog {
            owner,
            factory,
            chain_id,
            block_number,
            block_hash,
            transaction_hash,
            log_index,
        }
    }
}

/// Required registration response after canonical hashes are dropped.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RegistrationReorgAction {
    /// At least one unaffected stable/canonical evidence item remains.
    Keep,
    /// No stable evidence remains, but a state query can be re-run.
    Revalidate,
    /// Every supporting evidence item was orphaned.
    Remove,
}

/// All known independent evidence supporting one logical pool registration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegistrationEvidenceSet {
    evidence: Vec<RegistrationProvenance>,
}

impl RegistrationEvidenceSet {
    /// Construct a non-empty evidence set in canonical order.
    ///
    /// Requiring a primary item makes the invariant that every accepted
    /// registration has provenance explicit in the type construction path.
    pub fn new(
        primary: RegistrationProvenance,
        additional: impl IntoIterator<Item = RegistrationProvenance>,
    ) -> Self {
        let mut evidence = vec![primary];
        evidence.extend(additional);
        evidence.sort_unstable();
        evidence.dedup();
        Self { evidence }
    }

    /// Borrow every supporting evidence item.
    pub fn evidence(&self) -> &[RegistrationProvenance] {
        &self.evidence
    }

    /// Determine the conservative response to the supplied dropped hashes.
    pub fn reorg_action(&self, dropped_hashes: &[B256]) -> RegistrationReorgAction {
        let mut requires_revalidation = false;

        for evidence in &self.evidence {
            match evidence {
                RegistrationProvenance::Stable { .. }
                | RegistrationProvenance::StateQuery {
                    policy: QueryEvidencePolicy::Finalized,
                    ..
                } => return RegistrationReorgAction::Keep,
                RegistrationProvenance::StateQuery {
                    block_hash,
                    policy: QueryEvidencePolicy::RevalidateOnReorg,
                    ..
                } => {
                    if dropped_hashes.contains(block_hash) {
                        requires_revalidation = true;
                    } else {
                        return RegistrationReorgAction::Keep;
                    }
                }
                RegistrationProvenance::FactoryLog { block_hash, .. } => {
                    if !dropped_hashes.contains(block_hash) {
                        return RegistrationReorgAction::Keep;
                    }
                }
            }
        }

        if requires_revalidation {
            RegistrationReorgAction::Revalidate
        } else {
            RegistrationReorgAction::Remove
        }
    }
}

/// Quality of the complete AMM state represented by a committed change set.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum AmmStateQuality {
    /// State is coherent at the advertised point.
    Coherent,
    /// State is coherent, but one or more pools are unavailable/degraded.
    Degraded,
    /// Canonical trust was lost and consumers must await recovery.
    Untrusted,
}

/// Final kind of one pool change within a committed AMM state version.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum AmmPoolChangeKind {
    /// A pool instance became visible.
    Added,
    /// Quote-relevant state or metadata changed.
    Updated,
    /// The pool became unavailable for normal search policy.
    Degraded,
    /// The pool recovered normal search eligibility.
    Recovered,
    /// The pool instance was removed.
    Removed,
}

/// Search-facing consequences of one pool change.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct AmmChangeImpact {
    state: bool,
    quoteability: bool,
    topology: bool,
}

impl AmmChangeImpact {
    /// Construct explicit impact flags.
    pub const fn new(state: bool, quoteability: bool, topology: bool) -> Self {
        Self {
            state,
            quoteability,
            topology,
        }
    }

    /// A quote-state update with unchanged eligibility/topology.
    pub const fn state_only() -> Self {
        Self::new(true, false, false)
    }

    /// A search-eligibility change with unchanged topology metadata.
    pub const fn quoteability() -> Self {
        Self::new(false, true, false)
    }

    /// A graph-topology metadata change with unchanged state/eligibility.
    pub const fn topology() -> Self {
        Self::new(false, false, true)
    }

    /// State, search eligibility, and topology all changed.
    pub const fn all() -> Self {
        Self::new(true, true, true)
    }

    /// Whether quote-relevant state changed.
    pub const fn state_changed(self) -> bool {
        self.state
    }

    /// Whether search eligibility changed.
    pub const fn quoteability_changed(self) -> bool {
        self.quoteability
    }

    /// Whether graph topology metadata changed.
    pub const fn topology_changed(self) -> bool {
        self.topology
    }

    /// Union two independently observed impact sets.
    pub const fn union(self, other: Self) -> Self {
        Self::new(
            self.state || other.state,
            self.quoteability || other.quoteability,
            self.topology || other.topology,
        )
    }
}

/// One pool's final change within a committed AMM state version.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AmmPoolChange {
    pool: PoolInstanceId,
    revision: PoolStateRevision,
    kind: AmmPoolChangeKind,
    impact: AmmChangeImpact,
}

impl AmmPoolChange {
    /// Construct a pool change.
    pub const fn new(
        pool: PoolInstanceId,
        revision: PoolStateRevision,
        kind: AmmPoolChangeKind,
        impact: AmmChangeImpact,
    ) -> Self {
        Self {
            pool,
            revision,
            kind,
            impact,
        }
    }

    /// Affected pool registration instance.
    pub const fn pool(&self) -> &PoolInstanceId {
        &self.pool
    }

    /// Resulting quote-relevant revision.
    pub const fn revision(&self) -> PoolStateRevision {
        self.revision
    }

    /// Final change kind.
    pub const fn kind(&self) -> AmmPoolChangeKind {
        self.kind
    }

    /// Search-facing consequences.
    pub const fn impact(&self) -> AmmChangeImpact {
        self.impact
    }
}

/// Canonical incident represented by a committed AMM state version.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[non_exhaustive]
pub enum AmmStateIncident {
    /// Previously canonical block points were dropped.
    Reorg {
        /// Dropped post-block points in ascending chain order.
        dropped: Vec<AmmStatePoint>,
    },
    /// A forward canonical block range was not observed.
    Gap {
        /// First missed block, inclusive.
        from: u64,
        /// Last missed block, inclusive.
        to: u64,
    },
    /// A tracked account root changed without a covering decoder.
    CoverageGap {
        /// Account with unknown changed storage.
        address: Address,
        /// Block at which the gap was detected.
        block: u64,
    },
}

/// Invalid construction of a committed AMM change set.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum AmmChangeSetError {
    /// More than one final change was supplied for a pool instance.
    DuplicatePool {
        /// Duplicated pool registration instance.
        pool: PoolInstanceId,
    },
}

impl AmmChangeSetError {
    /// Construct a duplicate-pool error.
    pub const fn duplicate_pool(pool: PoolInstanceId) -> Self {
        Self::DuplicatePool { pool }
    }
}

impl fmt::Display for AmmChangeSetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicatePool { pool } => {
                write!(f, "duplicate AMM change for pool instance {pool:?}")
            }
        }
    }
}

impl std::error::Error for AmmChangeSetError {}

/// Deterministically ordered changes for one coherent published AMM state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AmmChangeSet {
    version: AmmStateVersion,
    point: AmmStatePoint,
    quality: AmmStateQuality,
    pool_changes: Vec<AmmPoolChange>,
    incidents: Vec<AmmStateIncident>,
    requires_full_refresh: bool,
}

impl AmmChangeSet {
    /// Construct a committed change set and canonicalize pool-change ordering.
    pub fn new(
        version: AmmStateVersion,
        point: AmmStatePoint,
        quality: AmmStateQuality,
        pool_changes: impl IntoIterator<Item = AmmPoolChange>,
        incidents: impl IntoIterator<Item = AmmStateIncident>,
        requires_full_refresh: bool,
    ) -> Result<Self, AmmChangeSetError> {
        let mut pool_changes: Vec<_> = pool_changes.into_iter().collect();
        pool_changes.sort_by(|left, right| left.pool.cmp(&right.pool));
        for duplicate in pool_changes.windows(2) {
            if duplicate[0].pool == duplicate[1].pool {
                return Err(AmmChangeSetError::duplicate_pool(duplicate[0].pool.clone()));
            }
        }

        let mut incidents: Vec<_> = incidents
            .into_iter()
            .map(|incident| match incident {
                AmmStateIncident::Reorg { mut dropped } => {
                    dropped.sort_unstable();
                    dropped.dedup();
                    AmmStateIncident::Reorg { dropped }
                }
                incident => incident,
            })
            .collect();
        incidents.sort_unstable();
        incidents.dedup();

        Ok(Self {
            version,
            point,
            quality,
            pool_changes,
            incidents,
            requires_full_refresh,
        })
    }

    /// Published state version.
    pub const fn version(&self) -> AmmStateVersion {
        self.version
    }

    /// Coherent post-block point.
    pub const fn point(&self) -> AmmStatePoint {
        self.point
    }

    /// Complete state quality.
    pub const fn quality(&self) -> AmmStateQuality {
        self.quality
    }

    /// Canonically ordered one-per-pool changes.
    pub fn pool_changes(&self) -> &[AmmPoolChange] {
        &self.pool_changes
    }

    /// Canonical incidents carried by this version.
    pub fn incidents(&self) -> &[AmmStateIncident] {
        &self.incidents
    }

    /// Whether consumers must conservatively refresh outside the known pool set.
    pub const fn requires_full_refresh(&self) -> bool {
        self.requires_full_refresh
    }
}

/// Composite identity of one scheduled attempt under a generation-scoped owner.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RuntimeWorkId {
    owner: RuntimeOwnerId,
    work: WorkId,
}

impl RuntimeWorkId {
    /// Construct a runtime work identity.
    pub const fn new(owner: RuntimeOwnerId, work: WorkId) -> Self {
        Self { owner, work }
    }

    /// Generation-scoped owner.
    pub const fn owner(&self) -> &RuntimeOwnerId {
        &self.owner
    }

    /// Attempt identifier.
    pub const fn work(&self) -> WorkId {
        self.work
    }
}

/// Operational state shared by adapter and discovery owners.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum OwnerRuntimeState {
    /// Owner accepts and processes work.
    Active,
    /// Owner rejects new work while teardown runs.
    Removing,
    /// No ownership remains for this generation.
    Removed,
    /// Owner cannot currently make progress.
    Failed,
}

/// Recoverable latest lifecycle state for every runtime owner category.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RuntimeLifecycleMap {
    pools: BTreeMap<PoolInstanceId, PoolRuntimeState>,
    adapters: BTreeMap<AdapterInstanceId, OwnerRuntimeState>,
    discovery: BTreeMap<DiscoveryOwnerId, OwnerRuntimeState>,
}

impl RuntimeLifecycleMap {
    /// Set the latest state of a pool instance.
    pub fn set_pool(&mut self, pool: PoolInstanceId, state: PoolRuntimeState) {
        self.pools.insert(pool, state);
    }

    /// Set the latest state of an adapter instance.
    pub fn set_adapter(&mut self, adapter: AdapterInstanceId, state: OwnerRuntimeState) {
        self.adapters.insert(adapter, state);
    }

    /// Set the latest state of a discovery owner.
    pub fn set_discovery(&mut self, owner: DiscoveryOwnerId, state: OwnerRuntimeState) {
        self.discovery.insert(owner, state);
    }

    /// Latest state of a pool instance.
    pub fn pool(&self, pool: &PoolInstanceId) -> Option<PoolRuntimeState> {
        self.pools.get(pool).copied()
    }

    /// Latest state of an adapter instance.
    pub fn adapter(&self, adapter: &AdapterInstanceId) -> Option<OwnerRuntimeState> {
        self.adapters.get(adapter).copied()
    }

    /// Latest state of a discovery owner.
    pub fn discovery(&self, owner: &DiscoveryOwnerId) -> Option<OwnerRuntimeState> {
        self.discovery.get(owner).copied()
    }

    /// Canonically ordered pool lifecycle entries.
    pub fn pools(&self) -> impl Iterator<Item = (&PoolInstanceId, PoolRuntimeState)> {
        self.pools.iter().map(|(owner, state)| (owner, *state))
    }

    /// Canonically ordered adapter lifecycle entries.
    pub fn adapters(&self) -> impl Iterator<Item = (&AdapterInstanceId, OwnerRuntimeState)> {
        self.adapters.iter().map(|(owner, state)| (owner, *state))
    }

    /// Canonically ordered discovery-owner lifecycle entries.
    pub fn discovery_owners(&self) -> impl Iterator<Item = (&DiscoveryOwnerId, OwnerRuntimeState)> {
        self.discovery.iter().map(|(owner, state)| (owner, *state))
    }
}

/// Scheduler priority/service class.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum AmmWorkClass {
    /// Reorg and canonical live input.
    Canonical,
    /// Registration catch-up.
    CatchUp,
    /// Required repair for tracked pools.
    Repair,
    /// Focused/user-requested work.
    Focused,
    /// Normal background bootstrap.
    Bootstrap,
    /// Optional deferred warming.
    Deferred,
}

/// Kind of one scheduled runtime job.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum AmmWorkKind {
    /// Pool/factory discovery.
    Discovery,
    /// Initial state hydration.
    ColdStart,
    /// Replay toward a canonical commit fence.
    CatchUp,
    /// Required state repair.
    Repair,
    /// Optional post-searchability warming.
    DeferredWarmup,
}

/// Latest progress of one scheduled runtime job.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AmmWorkProgress {
    kind: AmmWorkKind,
    completed: u64,
    total: Option<u64>,
}

/// Invalid completed/total relationship for one runtime job.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InvalidWorkProgress {
    completed: u64,
    total: u64,
}

impl InvalidWorkProgress {
    /// Construct an invalid-progress error.
    pub const fn new(completed: u64, total: u64) -> Self {
        Self { completed, total }
    }

    /// Reported completed units.
    pub const fn completed(self) -> u64 {
        self.completed
    }

    /// Reported total units.
    pub const fn total(self) -> u64 {
        self.total
    }
}

impl fmt::Display for InvalidWorkProgress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "completed work {} exceeds known total {}",
            self.completed, self.total
        )
    }
}

impl std::error::Error for InvalidWorkProgress {}

impl AmmWorkProgress {
    /// Construct progress, retaining `None` when total work is unknown.
    pub const fn new(
        kind: AmmWorkKind,
        completed: u64,
        total: Option<u64>,
    ) -> Result<Self, InvalidWorkProgress> {
        if let Some(total) = total
            && completed > total
        {
            return Err(InvalidWorkProgress::new(completed, total));
        }
        Ok(Self {
            kind,
            completed,
            total,
        })
    }

    /// Work kind.
    pub const fn kind(&self) -> AmmWorkKind {
        self.kind
    }

    /// Completed units.
    pub const fn completed(&self) -> u64 {
        self.completed
    }

    /// Known total units, or `None` when discovery/planning is incomplete.
    pub const fn total(&self) -> Option<u64> {
        self.total
    }
}

/// Recoverable queued work counts by scheduler class.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct QueueDepths {
    depths: BTreeMap<AmmWorkClass, usize>,
}

impl QueueDepths {
    /// Set the latest queue depth for a class.
    pub fn set(&mut self, class: AmmWorkClass, depth: usize) {
        self.depths.insert(class, depth);
    }

    /// Latest queue depth for a class.
    pub fn get(&self, class: AmmWorkClass) -> usize {
        self.depths.get(&class).copied().unwrap_or_default()
    }

    /// Canonically ordered non-default queue depths.
    pub fn iter(&self) -> impl Iterator<Item = (AmmWorkClass, usize)> + '_ {
        self.depths.iter().map(|(class, depth)| (*class, *depth))
    }
}

/// High-level health of the AMM runtime.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum AmmRuntimeHealth {
    /// Canonical state and control paths are healthy.
    Healthy,
    /// Some pools/work are degraded but coherent publication continues.
    Degraded,
    /// Canonical trust was lost; state publication is paused.
    Untrusted,
    /// Shutdown is in progress.
    ShuttingDown,
}

/// Queryable latest runtime lifecycle/progress state for lag recovery.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AmmRuntimeStatusSnapshot {
    sequence: u64,
    state_version: AmmStateVersion,
    lifecycles: Arc<RuntimeLifecycleMap>,
    active_work: Arc<BTreeMap<RuntimeWorkId, AmmWorkProgress>>,
    queues: Arc<QueueDepths>,
    health: AmmRuntimeHealth,
}

impl AmmRuntimeStatusSnapshot {
    /// Construct a recoverable status snapshot.
    pub fn new(
        sequence: u64,
        state_version: AmmStateVersion,
        lifecycles: RuntimeLifecycleMap,
        active_work: impl IntoIterator<Item = (RuntimeWorkId, AmmWorkProgress)>,
        queues: QueueDepths,
        health: AmmRuntimeHealth,
    ) -> Self {
        Self {
            sequence,
            state_version,
            lifecycles: Arc::new(lifecycles),
            active_work: Arc::new(active_work.into_iter().collect()),
            queues: Arc::new(queues),
            health,
        }
    }

    /// Last observer sequence reflected by this snapshot.
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }

    /// Latest coherent published state version.
    pub const fn state_version(&self) -> AmmStateVersion {
        self.state_version
    }

    /// Latest pool operational state.
    pub fn pool_state(&self, pool: &PoolInstanceId) -> Option<PoolRuntimeState> {
        self.lifecycles.pool(pool)
    }

    /// Latest adapter operational state.
    pub fn adapter_state(&self, adapter: &AdapterInstanceId) -> Option<OwnerRuntimeState> {
        self.lifecycles.adapter(adapter)
    }

    /// Latest discovery-owner operational state.
    pub fn discovery_state(&self, owner: &DiscoveryOwnerId) -> Option<OwnerRuntimeState> {
        self.lifecycles.discovery(owner)
    }

    /// Complete canonically ordered lifecycle map.
    pub fn lifecycles(&self) -> &RuntimeLifecycleMap {
        &self.lifecycles
    }

    /// Latest progress for one work attempt.
    pub fn active_work(&self, work: &RuntimeWorkId) -> Option<&AmmWorkProgress> {
        self.active_work.get(work)
    }

    /// Canonically ordered active-work entries.
    pub fn active_work_items(&self) -> impl Iterator<Item = (&RuntimeWorkId, &AmmWorkProgress)> {
        self.active_work.iter()
    }

    /// Latest queue depth for one scheduler class.
    pub fn queue_depth(&self, class: AmmWorkClass) -> usize {
        self.queues.get(class)
    }

    /// Complete queue-depth snapshot.
    pub fn queue_depths(&self) -> &QueueDepths {
        &self.queues
    }

    /// Runtime health.
    pub const fn health(&self) -> AmmRuntimeHealth {
        self.health
    }
}

/// Observer-facing runtime event payload.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum AmmRuntimeEventKind {
    /// A pool operational state changed.
    PoolLifecycleTransition {
        /// Pool registration instance.
        pool: PoolInstanceId,
        /// Previous state.
        from: PoolRuntimeState,
        /// New state.
        to: PoolRuntimeState,
    },
    /// An adapter-family operational state changed.
    AdapterLifecycleTransition {
        /// Adapter-family generation.
        adapter: AdapterInstanceId,
        /// Previous state.
        from: OwnerRuntimeState,
        /// New state.
        to: OwnerRuntimeState,
    },
    /// A discovery-source generation changed operational state.
    DiscoveryLifecycleTransition {
        /// Discovery-source generation.
        owner: DiscoveryOwnerId,
        /// Previous operational state.
        from: OwnerRuntimeState,
        /// New operational state.
        to: OwnerRuntimeState,
    },
    /// Work was classified and queued.
    WorkQueued {
        /// Work attempt.
        work: RuntimeWorkId,
        /// Scheduler class.
        class: AmmWorkClass,
        /// Work kind.
        kind: AmmWorkKind,
    },
    /// Work made progress.
    WorkProgress {
        /// Work attempt.
        work: RuntimeWorkId,
        /// Latest progress.
        progress: AmmWorkProgress,
    },
    /// Work completed successfully.
    WorkCompleted {
        /// Completed work attempt.
        work: RuntimeWorkId,
    },
    /// Work was cancelled or superseded.
    WorkCancelled {
        /// Cancelled work attempt.
        work: RuntimeWorkId,
    },
    /// Work failed.
    WorkFailed {
        /// Failed work attempt.
        work: RuntimeWorkId,
        /// Human-readable diagnostic message.
        message: String,
    },
    /// One cold-start round began.
    ColdStartRoundStarted {
        /// Cold-start work attempt.
        work: RuntimeWorkId,
        /// Zero-based round number.
        round: u64,
        /// Known total rounds, when planning is complete.
        total_rounds: Option<u64>,
    },
    /// One cold-start round completed.
    ColdStartRoundCompleted {
        /// Cold-start work attempt.
        work: RuntimeWorkId,
        /// Zero-based round number.
        round: u64,
    },
    /// A pool registration instance became canonically visible.
    RegistrationAccepted {
        /// Accepted pool instance.
        pool: PoolInstanceId,
    },
    /// A pool registration instance was canonically removed.
    RegistrationRemoved {
        /// Removed pool instance.
        pool: PoolInstanceId,
    },
    /// An adapter-family generation became available for pool dispatch.
    AdapterRegistrationAccepted {
        /// Accepted adapter generation.
        adapter: AdapterInstanceId,
    },
    /// An adapter-family generation was removed.
    AdapterRegistrationRemoved {
        /// Removed adapter generation.
        adapter: AdapterInstanceId,
    },
    /// A discovery-source generation became available for discovery work.
    DiscoveryRegistrationAccepted {
        /// Accepted discovery-source generation.
        owner: DiscoveryOwnerId,
    },
    /// A discovery-source generation was removed.
    DiscoveryRegistrationRemoved {
        /// Removed discovery-source generation.
        owner: DiscoveryOwnerId,
    },
    /// A coherent AMM state version was published.
    StateCommitted {
        /// Published state version.
        version: AmmStateVersion,
        /// Published post-block point.
        point: AmmStatePoint,
    },
    /// Previously canonical state points were dropped.
    Reorg {
        /// Dropped post-block points in ascending chain order.
        dropped: Vec<AmmStatePoint>,
    },
    /// A forward canonical block range was not observed.
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
    /// Runtime health changed.
    HealthChanged {
        /// Previous health.
        from: AmmRuntimeHealth,
        /// New health.
        to: AmmRuntimeHealth,
    },
}

/// Monotonically sequenced observer event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AmmRuntimeEvent {
    sequence: u64,
    kind: AmmRuntimeEventKind,
}

impl AmmRuntimeEvent {
    /// Construct an event at an explicit sequence.
    pub const fn new(sequence: u64, kind: AmmRuntimeEventKind) -> Self {
        Self { sequence, kind }
    }

    /// Construct the immediately following event, rejecting sequence overflow.
    pub fn checked_next(&self, kind: AmmRuntimeEventKind) -> Result<Self, RuntimeSequenceOverflow> {
        self.sequence
            .checked_add(1)
            .map(|sequence| Self { sequence, kind })
            .ok_or_else(|| RuntimeSequenceOverflow::new("AmmRuntimeEvent.sequence"))
    }

    /// Observer sequence.
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }

    /// Typed event payload.
    pub const fn kind(&self) -> &AmmRuntimeEventKind {
        &self.kind
    }
}
