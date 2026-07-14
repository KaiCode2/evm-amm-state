//! Feature-gated asynchronous owner of canonical AMM cache state.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use alloy_eips::BlockId;
use alloy_network::Ethereum;
use alloy_rpc_types_eth::Header as RpcHeader;
use evm_fork_cache::BlockContextError;
use evm_fork_cache::cache::EvmCache;
use evm_fork_cache::reactive::{
    BlockRef, ChainStatus, HandlerId, ReactiveInput, ReactiveInputBatch, ReactiveReport,
};
use tokio::sync::{broadcast, mpsc, oneshot, watch};

use super::cold_start_scheduler::{
    AmmColdStartJob, AmmColdStartOptions, AmmColdStartTarget, AmmColdStartWorkerConfig,
    AmmColdStartWorkerControl, AmmColdStartWorkerError, AmmColdStartWorkerHandle, AmmDiscoveryJob,
    AmmScheduledPool, AmmSlotPatchJob, spawn_cold_start_worker,
};
use super::subscriber_driver::{AmmSubscriberControl, AmmSubscriberOwnerPlan};
use super::{
    AdapterRegistry, AdapterRegistrySnapshot, AdapterRegistrySnapshotError, AmmChangeSet,
    AmmChangeSetError, AmmPoolChange, AmmPoolChangeKind, AmmPoolGenerationReservation,
    AmmPreparedPoolState, AmmPreparedStorage, AmmRuntimeEvent, AmmRuntimeEventKind,
    AmmRuntimeHealth, AmmRuntimeStatusSnapshot, AmmStateCommit, AmmStateIncident, AmmStatePoint,
    AmmStateQuality, AmmStateSnapshot, AmmStateVersion, AmmSyncEngine, AmmSyncError,
    AmmSyncIncident, AmmSyncPoolChangeKind, AmmWorkClass, AmmWorkKind, AmmWorkProgress,
    DeferredWork, DiscoveryGeneration, DiscoveryOwnerId, DiscoveryOwnerKey, DiscoveryOwnership,
    PoolDiscovery, PoolKey, PoolRegistration, PoolRevisionMap, PoolRuntimeState, PoolStateRevision,
    QueryEvidencePolicy, QueueDepths, RegistrationEvidenceSet, RegistrationProvenance,
    RegistrationReorgAction, RegistrationSourceKey, RepairAction, RuntimeLifecycleMap,
    RuntimeOwnerId, RuntimeSequenceOverflow, RuntimeWorkId, TokenEdgeDiscoveryReport,
    TokenEdgeDiscoveryRequest, WorkId,
};

/// Hash-pinned, source-reconciled complete canonical block delivery.
///
/// Construction validates that every record belongs to the same canonical
/// block and chain. The caller/driver is responsible for the external proof of
/// completeness: live implementations build this only after hash-pinned log
/// reconciliation for the advertised interest revision.
pub struct AmmCanonicalBatch {
    chain_id: u64,
    header: RpcHeader,
    block: BlockRef,
    interest_revision: u64,
    records: ReactiveInputBatch<Ethereum>,
}

impl AmmCanonicalBatch {
    /// Construct a complete canonical block after source-level reconciliation.
    pub fn from_verified_block(
        chain_id: u64,
        header: RpcHeader,
        interest_revision: u64,
        records: ReactiveInputBatch<Ethereum>,
    ) -> Result<Self, AmmCanonicalBatchError> {
        let computed_hash = header.inner.hash_slow();
        if header.hash != computed_hash {
            return Err(AmmCanonicalBatchError::HeaderHashMismatch {
                advertised: header.hash,
                computed: computed_hash,
            });
        }
        let block = BlockRef {
            number: header.inner.number,
            hash: header.hash,
            parent_hash: Some(header.inner.parent_hash),
            timestamp: Some(header.inner.timestamp),
        };
        let mut identities = BTreeSet::new();
        let mut positions = BTreeSet::new();
        for (index, record) in records.records().iter().enumerate() {
            let ReactiveInput::Log(log) = &record.input else {
                return Err(AmmCanonicalBatchError::UnsupportedRecord { index });
            };
            if log.removed {
                return Err(AmmCanonicalBatchError::NonCanonicalRecord { index });
            }
            let Some(record_chain_id) = record.context.chain_id else {
                return Err(AmmCanonicalBatchError::MissingChainId { index });
            };
            if record_chain_id != chain_id {
                return Err(AmmCanonicalBatchError::RecordChainMismatch {
                    index,
                    expected: chain_id,
                    actual: record_chain_id,
                });
            }
            let record_block = match &record.context.chain_status {
                ChainStatus::Included {
                    block: record_block,
                    ..
                }
                | ChainStatus::Safe {
                    block: record_block,
                }
                | ChainStatus::Finalized {
                    block: record_block,
                } => record_block,
                ChainStatus::Pending | ChainStatus::Reorged { .. } => {
                    return Err(AmmCanonicalBatchError::NonCanonicalRecord { index });
                }
            };
            if record_block != &block {
                return Err(AmmCanonicalBatchError::RecordBlockMismatch {
                    index,
                    expected: Box::new(block.clone()),
                    actual: Box::new(record_block.clone()),
                });
            }
            if log.block_number != Some(block.number) || log.block_hash != Some(block.hash) {
                return Err(AmmCanonicalBatchError::MalformedRecord {
                    index,
                    reason: "log block identity does not match its canonical context",
                });
            }
            let (Some(transaction_hash), Some(transaction_index), Some(log_index)) =
                (log.transaction_hash, log.transaction_index, log.log_index)
            else {
                return Err(AmmCanonicalBatchError::MalformedRecord {
                    index,
                    reason: "canonical log identity is incomplete",
                });
            };
            if record.context.transaction_index != Some(transaction_index)
                || record.context.log_index != Some(log_index)
            {
                return Err(AmmCanonicalBatchError::MalformedRecord {
                    index,
                    reason: "log position does not match its canonical context",
                });
            }
            if !identities.insert((transaction_hash, log_index)) || !positions.insert(log_index) {
                return Err(AmmCanonicalBatchError::DuplicateRecord { index });
            }
        }
        Ok(Self {
            chain_id,
            header,
            block,
            interest_revision,
            records,
        })
    }

    /// Canonical chain identity.
    pub const fn chain_id(&self) -> u64 {
        self.chain_id
    }

    /// Complete block identity.
    pub const fn block(&self) -> &BlockRef {
        &self.block
    }

    /// Subscriber interest-set revision used for reconciliation.
    pub const fn interest_revision(&self) -> u64 {
        self.interest_revision
    }

    fn into_parts(self) -> (RpcHeader, ReactiveInputBatch<Ethereum>) {
        (self.header, self.records)
    }
}

/// Invalid complete-canonical-block envelope.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum AmmCanonicalBatchError {
    /// The RPC envelope's advertised hash did not seal its consensus header.
    HeaderHashMismatch {
        /// Hash advertised by the RPC response.
        advertised: alloy_primitives::B256,
        /// Hash computed from the consensus header.
        computed: alloy_primitives::B256,
    },
    /// The reconciled payload contained something other than a canonical log.
    UnsupportedRecord {
        /// Zero-based record index.
        index: usize,
    },
    /// A log's intrinsic identity disagreed with its canonical context.
    MalformedRecord {
        /// Zero-based record index.
        index: usize,
        /// Stable validation diagnostic.
        reason: &'static str,
    },
    /// A canonical log identity appeared more than once.
    DuplicateRecord {
        /// Zero-based duplicate record index.
        index: usize,
    },
    /// A record lacked a chain identity.
    MissingChainId {
        /// Zero-based record index.
        index: usize,
    },
    /// A record belonged to a different chain.
    RecordChainMismatch {
        /// Zero-based record index.
        index: usize,
        /// Envelope chain.
        expected: u64,
        /// Record chain.
        actual: u64,
    },
    /// A pending or explicitly reorged record appeared in a canonical block envelope.
    NonCanonicalRecord {
        /// Zero-based record index.
        index: usize,
    },
    /// A record belonged to a different block.
    RecordBlockMismatch {
        /// Zero-based record index.
        index: usize,
        /// Envelope block.
        expected: Box<BlockRef>,
        /// Record block.
        actual: Box<BlockRef>,
    },
}

impl fmt::Display for AmmCanonicalBatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HeaderHashMismatch {
                advertised,
                computed,
            } => write!(
                f,
                "canonical header hash mismatch: advertised {advertised}, computed {computed}"
            ),
            Self::UnsupportedRecord { index } => write!(
                f,
                "canonical AMM envelope contains unsupported record {index}"
            ),
            Self::MalformedRecord { index, reason } => {
                write!(f, "canonical record {index} is malformed: {reason}")
            }
            Self::DuplicateRecord { index } => {
                write!(f, "canonical record {index} duplicates an earlier log")
            }
            Self::MissingChainId { index } => {
                write!(f, "canonical record {index} has no chain id")
            }
            Self::RecordChainMismatch {
                index,
                expected,
                actual,
            } => write!(
                f,
                "canonical record {index} chain mismatch: expected {expected}, received {actual}"
            ),
            Self::NonCanonicalRecord { index } => {
                write!(f, "canonical envelope contains noncanonical record {index}")
            }
            Self::RecordBlockMismatch {
                index,
                expected,
                actual,
            } => write!(
                f,
                "canonical record {index} block mismatch: expected {expected:?}, received {actual:?}"
            ),
        }
    }
}

impl std::error::Error for AmmCanonicalBatchError {}

/// Hash-sealed full block context anchoring the actor's initial post-block state.
pub struct AmmRuntimeBaseline {
    header: RpcHeader,
    point: AmmStatePoint,
}

impl AmmRuntimeBaseline {
    /// Validate an RPC header seal and construct its explicit post-block point.
    pub fn from_verified_header(
        chain_id: u64,
        header: RpcHeader,
    ) -> Result<Self, AmmCanonicalBatchError> {
        let computed_hash = header.inner.hash_slow();
        if header.hash != computed_hash {
            return Err(AmmCanonicalBatchError::HeaderHashMismatch {
                advertised: header.hash,
                computed: computed_hash,
            });
        }
        let point = AmmStatePoint::post_block(chain_id, header.inner.number, header.hash);
        Ok(Self { header, point })
    }

    /// Explicit point represented by the full header context.
    pub const fn point(&self) -> AmmStatePoint {
        self.point
    }
}

/// Bounded-channel configuration for the asynchronous AMM runtime.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AmmRuntimeConfig {
    command_capacity: usize,
    canonical_input_capacity: usize,
    critical_change_capacity: usize,
    observer_capacity: usize,
}

impl Default for AmmRuntimeConfig {
    fn default() -> Self {
        Self {
            command_capacity: 64,
            canonical_input_capacity: 64,
            critical_change_capacity: 64,
            observer_capacity: 256,
        }
    }
}

impl AmmRuntimeConfig {
    /// Set the bounded control-command capacity.
    pub const fn with_command_capacity(mut self, capacity: usize) -> Self {
        self.command_capacity = capacity;
        self
    }

    /// Bounded control-command capacity.
    pub const fn command_capacity(&self) -> usize {
        self.command_capacity
    }

    /// Set the bounded canonical-input capacity independently of control work.
    pub const fn with_canonical_input_capacity(mut self, capacity: usize) -> Self {
        self.canonical_input_capacity = capacity;
        self
    }

    /// Bounded canonical-input capacity.
    pub const fn canonical_input_capacity(&self) -> usize {
        self.canonical_input_capacity
    }

    /// Set the bounded reliable commit capacity for the canonical consumer.
    pub const fn with_critical_change_capacity(mut self, capacity: usize) -> Self {
        self.critical_change_capacity = capacity;
        self
    }

    /// Bounded reliable commit capacity.
    pub const fn critical_change_capacity(&self) -> usize {
        self.critical_change_capacity
    }

    /// Set the lossy observer broadcast capacity.
    pub const fn with_observer_capacity(mut self, capacity: usize) -> Self {
        self.observer_capacity = capacity;
        self
    }

    /// Lossy observer broadcast capacity.
    pub const fn observer_capacity(&self) -> usize {
        self.observer_capacity
    }
}

/// Error starting the asynchronous runtime.
#[derive(Debug)]
#[non_exhaustive]
pub enum AmmRuntimeSpawnError {
    /// A required bounded channel capacity was zero.
    ZeroChannelCapacity,
    /// No Tokio runtime was active on the calling thread.
    MissingTokioRuntime,
    /// The caller was not executing inside a Tokio `LocalSet`.
    MissingLocalExecutor,
    /// The synchronous lifecycle engine could not be initialized.
    Sync(AmmSyncError),
    /// The initial registry and generation-ownership snapshot diverged.
    RegistrySnapshot(Box<AdapterRegistrySnapshotError>),
    /// The cache and advertised baseline use different chain identities.
    BaselineChainMismatch {
        /// Advertised state-point chain.
        expected: u64,
        /// Cache EVM chain context.
        actual: u64,
    },
    /// The cache block context does not match the advertised post-block point.
    BaselineBlockMismatch {
        /// Advertised post-block number.
        expected: u64,
        /// Cache EVM block context.
        actual: Option<u64>,
    },
    /// The cache's full EVM context does not match the verified baseline header.
    BaselineContextMismatch(&'static str),
}

impl fmt::Display for AmmRuntimeSpawnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroChannelCapacity => {
                write!(f, "AMM runtime channel capacities must be non-zero")
            }
            Self::MissingTokioRuntime => write!(f, "AMM runtime requires an active Tokio runtime"),
            Self::MissingLocalExecutor => {
                write!(f, "AMM runtime requires an active Tokio LocalSet")
            }
            Self::Sync(error) => write!(f, "failed to initialize AMM runtime: {error}"),
            Self::RegistrySnapshot(error) => write!(f, "failed to snapshot AMM topology: {error}"),
            Self::BaselineChainMismatch { expected, actual } => write!(
                f,
                "AMM runtime baseline chain mismatch: expected {expected}, cache uses {actual}"
            ),
            Self::BaselineBlockMismatch { expected, actual } => write!(
                f,
                "AMM runtime baseline block mismatch: expected {expected}, cache uses {actual:?}"
            ),
            Self::BaselineContextMismatch(field) => write!(
                f,
                "AMM runtime cache does not represent baseline header field {field}"
            ),
        }
    }
}

impl std::error::Error for AmmRuntimeSpawnError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Sync(error) => Some(error),
            Self::RegistrySnapshot(error) => Some(error),
            Self::ZeroChannelCapacity
            | Self::MissingTokioRuntime
            | Self::MissingLocalExecutor
            | Self::BaselineChainMismatch { .. }
            | Self::BaselineBlockMismatch { .. }
            | Self::BaselineContextMismatch(_) => None,
        }
    }
}

impl From<AmmSyncError> for AmmRuntimeSpawnError {
    fn from(error: AmmSyncError) -> Self {
        Self::Sync(error)
    }
}

impl From<AdapterRegistrySnapshotError> for AmmRuntimeSpawnError {
    fn from(error: AdapterRegistrySnapshotError) -> Self {
        Self::RegistrySnapshot(Box::new(error))
    }
}

/// Error submitting, executing, or acknowledging an asynchronous command.
#[derive(Debug)]
#[non_exhaustive]
pub enum AmmRuntimeCommandError {
    /// The runtime task has already stopped accepting commands.
    Closed,
    /// Canonical state is untrusted and must be authoritatively recovered.
    Untrusted,
    /// The synchronous AMM engine rejected or failed the batch.
    Sync(AmmSyncError),
    /// A registry/ownership divergence prevented coherent publication.
    RegistrySnapshot(Box<AdapterRegistrySnapshotError>),
    /// The verified header could not satisfy the cache's block-context contract.
    BlockContext(BlockContextError),
    /// A checked runtime sequence was exhausted.
    Sequence(RuntimeSequenceOverflow),
    /// The committed change set violated a domain invariant.
    ChangeSet(AmmChangeSetError),
    /// A second correctness-critical consumer was requested while one is active.
    CriticalSubscriberExists,
    /// An input carried a different chain identity than the runtime baseline.
    ChainMismatch {
        /// Runtime chain identity.
        expected: u64,
        /// Input chain identity.
        actual: u64,
    },
    /// A reorged current block did not carry the parent hash needed for a new point.
    MissingReorgParent,
    /// Upstream surfaced a non-fatal error report after batch processing began.
    UntrustedBatch(String),
    /// Prepared state was built against a point other than the actor's current point.
    StaleBaseline {
        /// Actor's current point.
        expected: AmmStatePoint,
        /// Submitted prepared-state point.
        actual: AmmStatePoint,
    },
    /// Prepared installation was requested for a pool not marked ready.
    PoolNotReady(super::PoolKey),
    /// A ready registration's declared exact state was absent from the cache.
    MissingPreparedState {
        /// Registration that cannot yet quote from the actor cache.
        pool: Box<super::PoolKey>,
        /// Declared slots still absent from the coherent baseline.
        missing: Box<[super::StateSlot]>,
    },
    /// Whole-account ownership cannot be certified by the Stage 4 slot seam.
    UnverifiablePreparedState {
        /// Registration requiring a stronger prepared-state proof.
        pool: Box<super::PoolKey>,
        /// Number of whole-account dependencies.
        whole_accounts: usize,
    },
    /// A prepared artifact carried state outside the adapter's declared read set.
    UnexpectedPreparedState {
        /// Storage-owning contract.
        address: alloy_primitives::Address,
        /// Unexpected storage key.
        slot: alloy_primitives::U256,
    },
    /// A scheduled artifact omitted one adapter-declared code-seed proof.
    MissingPreparedAccount {
        /// Pool whose code claim was omitted.
        pool: Box<super::PoolKey>,
        /// Missing account/code identity.
        address: alloy_primitives::Address,
    },
    /// A prepared account/code proof was not declared by the adapter.
    UnexpectedPreparedAccount {
        /// Unexpected account identity.
        address: alloy_primitives::Address,
    },
    /// Prepared runtime bytes/proof do not match the adapter's code claim.
    PreparedAccountClaimMismatch {
        /// Contradicted account identity.
        address: alloy_primitives::Address,
    },
    /// Exact-generation removal did not match the active pool instance.
    StalePoolInstance {
        /// Requested generation.
        requested: Box<super::PoolInstanceId>,
        /// Currently active generation, when the logical key still exists.
        active: Option<Box<super::PoolInstanceId>>,
    },
    /// Exact-generation removal did not match the active adapter instance.
    StaleAdapterInstance {
        /// Requested adapter generation.
        requested: Box<super::AdapterInstanceId>,
        /// Currently active generation, when the logical family still exists.
        active: Option<Box<super::AdapterInstanceId>>,
    },
    /// A watcher with the same logical key is already active.
    DiscoveryAlreadyRegistered(DiscoveryOwnerId),
    /// Exact-generation removal did not match the active discovery owner.
    StaleDiscoveryOwner {
        /// Requested watcher generation.
        requested: Box<DiscoveryOwnerId>,
        /// Currently active generation, when the logical watcher still exists.
        active: Option<Box<DiscoveryOwnerId>>,
    },
    /// Removing a watcher would orphan registrations that have no independent evidence.
    DiscoveryOwnerInUse {
        /// Watcher generation that still exclusively supports active pools.
        owner: Box<DiscoveryOwnerId>,
        /// Exact pool generations that would lose their final evidence item.
        pools: Box<[super::PoolInstanceId]>,
    },
    /// A factory watcher declared no creation-log subscription sources.
    DiscoveryHasNoCreationSources(DiscoveryOwnerKey),
    /// Repair/deferred work could not be normalized into a supported scheduler job.
    UnsupportedFollowUp(String),
    /// Canonical block did not extend the actor's current point exactly.
    CanonicalDiscontinuity {
        /// Actor's current point.
        current: AmmStatePoint,
        /// Submitted block.
        next: Box<BlockRef>,
    },
    /// Canonical delivery was reconciled against a stale subscriber interest set.
    InterestRevisionMismatch {
        /// Actor's current interest revision.
        expected: u64,
        /// Delivery interest revision.
        actual: u64,
    },
    /// Subscriber interest revisions exhausted their monotonic sequence.
    InterestRevisionExhausted,
    /// Subscriber ownership/delivery coordination failed.
    Subscriber(String),
    /// A second live subscriber was attached to the same actor.
    SubscriberAlreadyAttached,
    /// A topology mutation attempted to overtake accepted canonical delivery.
    CanonicalBacklog,
    /// Direct canonical submission is disabled while a subscriber owns delivery.
    AttachedSubscriberOwnsCanonicalInput,
    /// No background cold-start provider worker is attached.
    ColdStartWorkerUnavailable,
    /// A background cold-start provider worker is already attached.
    ColdStartWorkerAlreadyAttached,
    /// The logical pool already has an accepted pending cold-start attempt.
    PoolAlreadyScheduled(PoolKey),
    /// A worker result or control request no longer owns the active work attempt.
    StaleWork(RuntimeWorkId),
    /// The background cold-start worker rejected a bounded submission.
    ColdStartWorker(String),
    /// Successful ingest did not align the cache's EVM block context.
    CacheContextMismatch {
        /// Canonical block expected after ingest.
        expected: u64,
        /// Cache EVM block number.
        actual: Option<u64>,
    },
}

impl fmt::Display for AmmRuntimeCommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closed => write!(f, "AMM runtime command channel is closed"),
            Self::Untrusted => write!(f, "AMM runtime canonical state is untrusted"),
            Self::Sync(error) => write!(f, "AMM runtime ingest failed: {error}"),
            Self::RegistrySnapshot(error) => write!(f, "AMM topology snapshot failed: {error}"),
            Self::BlockContext(error) => write!(f, "canonical block context failed: {error}"),
            Self::Sequence(error) => write!(f, "AMM runtime sequence failed: {error}"),
            Self::ChangeSet(error) => write!(f, "AMM runtime change set failed: {error}"),
            Self::CriticalSubscriberExists => {
                write!(
                    f,
                    "a correctness-critical AMM consumer is already subscribed"
                )
            }
            Self::ChainMismatch { expected, actual } => write!(
                f,
                "AMM runtime chain mismatch: expected {expected}, received {actual}"
            ),
            Self::MissingReorgParent => {
                write!(f, "reorged current block did not carry a parent hash")
            }
            Self::UntrustedBatch(message) => {
                write!(f, "AMM runtime batch lost canonical trust: {message}")
            }
            Self::StaleBaseline { expected, actual } => write!(
                f,
                "prepared AMM baseline is stale: expected {expected:?}, received {actual:?}"
            ),
            Self::PoolNotReady(pool) => {
                write!(f, "prepared AMM pool is not ready: {pool:?}")
            }
            Self::MissingPreparedState { pool, missing } => write!(
                f,
                "prepared AMM pool {pool:?} is missing {} declared slots",
                missing.len()
            ),
            Self::UnverifiablePreparedState {
                pool,
                whole_accounts,
            } => write!(
                f,
                "prepared AMM pool {pool:?} requires proof for {whole_accounts} whole-account dependencies"
            ),
            Self::UnexpectedPreparedState { address, slot } => write!(
                f,
                "prepared AMM state contains undeclared slot ({address}, {slot})"
            ),
            Self::MissingPreparedAccount { pool, address } => write!(
                f,
                "prepared AMM pool {pool:?} is missing code proof for {address}"
            ),
            Self::UnexpectedPreparedAccount { address } => {
                write!(
                    f,
                    "prepared AMM state contains undeclared account {address}"
                )
            }
            Self::PreparedAccountClaimMismatch { address } => write!(
                f,
                "prepared AMM account proof contradicts adapter code claim for {address}"
            ),
            Self::StalePoolInstance { requested, active } => write!(
                f,
                "stale AMM pool removal for {requested:?}; active generation is {active:?}"
            ),
            Self::StaleAdapterInstance { requested, active } => write!(
                f,
                "stale AMM adapter removal for {requested:?}; active generation is {active:?}"
            ),
            Self::DiscoveryAlreadyRegistered(owner) => {
                write!(f, "AMM discovery watcher is already active: {owner:?}")
            }
            Self::StaleDiscoveryOwner { requested, active } => write!(
                f,
                "stale AMM discovery watcher removal for {requested:?}; active generation is {active:?}"
            ),
            Self::DiscoveryOwnerInUse { owner, pools } => write!(
                f,
                "AMM discovery watcher {owner:?} still exclusively supports {} active pool(s)",
                pools.len()
            ),
            Self::DiscoveryHasNoCreationSources(key) => {
                write!(f, "AMM discovery watcher {key:?} has no creation sources")
            }
            Self::UnsupportedFollowUp(message) => {
                write!(f, "unsupported AMM follow-up work: {message}")
            }
            Self::CanonicalDiscontinuity { current, next } => write!(
                f,
                "canonical block {next:?} does not extend AMM point {current:?}"
            ),
            Self::InterestRevisionMismatch { expected, actual } => write!(
                f,
                "canonical interest revision mismatch: expected {expected}, received {actual}"
            ),
            Self::InterestRevisionExhausted => {
                write!(f, "AMM subscriber interest revision is exhausted")
            }
            Self::Subscriber(message) => write!(f, "AMM subscriber transaction failed: {message}"),
            Self::SubscriberAlreadyAttached => {
                write!(f, "an AMM subscriber driver is already attached")
            }
            Self::CanonicalBacklog => write!(
                f,
                "AMM topology mutation requires an empty canonical-input fence"
            ),
            Self::AttachedSubscriberOwnsCanonicalInput => write!(
                f,
                "direct canonical input is disabled while an AMM subscriber is attached"
            ),
            Self::ColdStartWorkerUnavailable => write!(f, "no AMM cold-start worker is attached"),
            Self::ColdStartWorkerAlreadyAttached => {
                write!(f, "an AMM cold-start worker is already attached")
            }
            Self::PoolAlreadyScheduled(pool) => {
                write!(f, "AMM pool already has pending cold-start work: {pool:?}")
            }
            Self::StaleWork(work) => write!(f, "stale AMM runtime work result: {work:?}"),
            Self::ColdStartWorker(message) => write!(f, "AMM cold-start worker failed: {message}"),
            Self::CacheContextMismatch { expected, actual } => write!(
                f,
                "cache block context mismatch after ingest: expected {expected}, received {actual:?}"
            ),
        }
    }
}

impl std::error::Error for AmmRuntimeCommandError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Sync(error) => Some(error),
            Self::RegistrySnapshot(error) => Some(error),
            Self::BlockContext(error) => Some(error),
            Self::Sequence(error) => Some(error),
            Self::ChangeSet(error) => Some(error),
            _ => None,
        }
    }
}

impl From<AmmSyncError> for AmmRuntimeCommandError {
    fn from(error: AmmSyncError) -> Self {
        Self::Sync(error)
    }
}

impl From<RuntimeSequenceOverflow> for AmmRuntimeCommandError {
    fn from(error: RuntimeSequenceOverflow) -> Self {
        Self::Sequence(error)
    }
}

impl From<AmmChangeSetError> for AmmRuntimeCommandError {
    fn from(error: AmmChangeSetError) -> Self {
        Self::ChangeSet(error)
    }
}

impl From<AdapterRegistrySnapshotError> for AmmRuntimeCommandError {
    fn from(error: AdapterRegistrySnapshotError) -> Self {
        Self::RegistrySnapshot(Box::new(error))
    }
}

impl From<BlockContextError> for AmmRuntimeCommandError {
    fn from(error: BlockContextError) -> Self {
        Self::BlockContext(error)
    }
}

/// One generation-fenced factory watcher to install in the live runtime.
#[derive(Clone)]
pub struct AmmFactoryWatcherRegistration {
    key: DiscoveryOwnerKey,
    adapter: super::AdapterInstanceId,
    discovery: Arc<PoolDiscovery>,
}

impl AmmFactoryWatcherRegistration {
    /// Bind a stable watcher key and discovery decoder to one exact adapter generation.
    pub const fn new(
        key: DiscoveryOwnerKey,
        adapter: super::AdapterInstanceId,
        discovery: Arc<PoolDiscovery>,
    ) -> Self {
        Self {
            key,
            adapter,
            discovery,
        }
    }

    /// Stable logical watcher key.
    pub const fn key(&self) -> &DiscoveryOwnerKey {
        &self.key
    }

    /// Exact adapter generation targeted by this watcher.
    pub const fn adapter(&self) -> &super::AdapterInstanceId {
        &self.adapter
    }

    /// Factory query and creation-log decoder set.
    pub const fn discovery(&self) -> &Arc<PoolDiscovery> {
        &self.discovery
    }
}

impl fmt::Debug for AmmFactoryWatcherRegistration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AmmFactoryWatcherRegistration")
            .field("key", &self.key)
            .field("adapter", &self.adapter)
            .finish_non_exhaustive()
    }
}

/// Scheduling policy for one connector-focused discovery request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AmmDiscoveryOptions {
    class: AmmWorkClass,
    max_candidates: Option<usize>,
}

impl Default for AmmDiscoveryOptions {
    fn default() -> Self {
        Self {
            class: AmmWorkClass::Focused,
            max_candidates: None,
        }
    }
}

impl AmmDiscoveryOptions {
    /// Select the shared scheduler service class.
    pub const fn with_class(mut self, class: AmmWorkClass) -> Self {
        self.class = class;
        self
    }

    /// Shared scheduler class used by this request.
    pub const fn class(self) -> AmmWorkClass {
        self.class
    }

    /// Bound how many deterministically ordered pool candidates this request may admit.
    pub const fn with_max_candidates(mut self, max_candidates: usize) -> Self {
        self.max_candidates = Some(max_candidates);
        self
    }

    /// Maximum candidate registrations admitted by this request, when bounded.
    pub const fn max_candidates(self) -> Option<usize> {
        self.max_candidates
    }
}

/// One accepted connector-discovery attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AmmScheduledDiscovery {
    owner: DiscoveryOwnerId,
    work: RuntimeWorkId,
}

impl AmmScheduledDiscovery {
    const fn new(owner: DiscoveryOwnerId, work: RuntimeWorkId) -> Self {
        Self { owner, work }
    }

    /// Exact discovery owner generation.
    pub const fn owner(&self) -> &DiscoveryOwnerId {
        &self.owner
    }

    /// Exact scheduled attempt.
    pub const fn work(&self) -> &RuntimeWorkId {
        &self.work
    }
}

/// One accepted repair or deferred-warmup attempt for an active pool generation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AmmScheduledFollowUp {
    pool: super::PoolInstanceId,
    work: RuntimeWorkId,
    class: AmmWorkClass,
    kind: AmmWorkKind,
}

impl AmmScheduledFollowUp {
    const fn new(
        pool: super::PoolInstanceId,
        work: RuntimeWorkId,
        class: AmmWorkClass,
        kind: AmmWorkKind,
    ) -> Self {
        Self {
            pool,
            work,
            class,
            kind,
        }
    }

    /// Exact active pool generation.
    pub const fn pool(&self) -> &super::PoolInstanceId {
        &self.pool
    }

    /// Exact scheduled attempt.
    pub const fn work(&self) -> &RuntimeWorkId {
        &self.work
    }

    /// Shared scheduler class.
    pub const fn class(&self) -> AmmWorkClass {
        self.class
    }

    /// Repair or deferred-warmup work kind.
    pub const fn kind(&self) -> AmmWorkKind {
        self.kind
    }
}

/// Namespace for spawning the asynchronous cache-owner runtime.
pub struct AmmRuntime;

impl AmmRuntime {
    /// Spawn a standalone local cache actor around an explicit coherent post-block baseline.
    ///
    /// `EvmCache` is intentionally thread-local, so this method must be called
    /// from a Tokio [`LocalSet`](tokio::task::LocalSet). The returned handle and
    /// immutable snapshots can still be cloned into ordinary `Send` workers.
    /// Subscriber-backed spawning is layered onto the same actor by the live
    /// transport constructor; this form supports caller-owned complete batches.
    pub fn spawn(
        mut cache: EvmCache,
        registry: AdapterRegistry,
        baseline: AmmRuntimeBaseline,
        config: AmmRuntimeConfig,
    ) -> Result<AmmRuntimeHandle, AmmRuntimeSpawnError> {
        let point = baseline.point;
        if config.command_capacity == 0
            || config.canonical_input_capacity == 0
            || config.critical_change_capacity == 0
            || config.observer_capacity == 0
        {
            return Err(AmmRuntimeSpawnError::ZeroChannelCapacity);
        }
        tokio::runtime::Handle::try_current()
            .map_err(|_| AmmRuntimeSpawnError::MissingTokioRuntime)?;
        if cache.chain_id() != point.chain_id() {
            return Err(AmmRuntimeSpawnError::BaselineChainMismatch {
                expected: point.chain_id(),
                actual: cache.chain_id(),
            });
        }
        if cache.block_number() != Some(point.block_number()) {
            return Err(AmmRuntimeSpawnError::BaselineBlockMismatch {
                expected: point.block_number(),
                actual: cache.block_number(),
            });
        }
        if cache.basefee() != baseline.header.inner.base_fee_per_gas {
            return Err(AmmRuntimeSpawnError::BaselineContextMismatch("basefee"));
        }
        if cache.coinbase() != Some(baseline.header.inner.beneficiary) {
            return Err(AmmRuntimeSpawnError::BaselineContextMismatch("coinbase"));
        }
        if cache.prevrandao() != Some(baseline.header.inner.mix_hash) {
            return Err(AmmRuntimeSpawnError::BaselineContextMismatch("prevrandao"));
        }
        if cache.block_gas_limit() != Some(baseline.header.inner.gas_limit) {
            return Err(AmmRuntimeSpawnError::BaselineContextMismatch("gas_limit"));
        }
        if cache.timestamp() != Some(baseline.header.inner.timestamp) {
            return Err(AmmRuntimeSpawnError::BaselineContextMismatch("timestamp"));
        }
        // The runtime point is hash-sealed. Keep provider reads and prepared
        // patch validation on that same EIP-1898 identity while restoring the
        // EVM NUMBER/BASEFEE context that a hash-only cache pin cannot infer.
        cache.set_block(BlockId::from((point.block_hash(), Some(true))));
        cache.set_block_context(
            Some(point.block_number()),
            baseline.header.inner.base_fee_per_gas,
        );

        let engine = AmmSyncEngine::new(registry)?;
        let registry_snapshot = Arc::new(AdapterRegistrySnapshot::try_new(
            engine.registry(),
            engine.ownership(),
        )?);
        let revisions: PoolRevisionMap = engine
            .ownership()
            .pools()
            .cloned()
            .map(|pool| (pool, PoolStateRevision::new(0)))
            .collect();
        let revisions = Arc::new(revisions);
        let runtime_id = super::AmmRuntimeId::allocate();
        let snapshot = Arc::new(AmmStateSnapshot::new(
            runtime_id,
            AmmStateVersion::initial(),
            point,
            0,
            cache.snapshot(),
            Arc::clone(&registry_snapshot),
            Arc::clone(&revisions),
        ));
        let (snapshot_tx, snapshot_rx) = watch::channel(snapshot);
        let initial_health = if engine
            .registry()
            .pools()
            .any(|pool| pool.status == super::PoolStatus::Degraded)
        {
            AmmRuntimeHealth::Degraded
        } else {
            AmmRuntimeHealth::Healthy
        };
        let initial_status = Arc::new(AmmRuntimeStatusSnapshot::new(
            0,
            AmmStateVersion::initial(),
            engine.lifecycles().clone(),
            std::iter::empty(),
            QueueDepths::default(),
            initial_health,
        ));
        let (status_tx, status_rx) = watch::channel(initial_status);
        let (interest_tx, interest_rx) = watch::channel(0u64);
        let (observer_tx, _) = broadcast::channel(config.observer_capacity);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (exit_tx, exit_rx) = watch::channel(false);
        let (command_tx, command_rx) = mpsc::channel(config.command_capacity);
        let (canonical_tx, canonical_rx) = mpsc::channel(config.canonical_input_capacity);
        let registration_evidence = engine
            .ownership()
            .pools()
            .cloned()
            .map(|pool| {
                (
                    pool,
                    RegistrationEvidenceSet::new(
                        RegistrationProvenance::stable(RegistrationSourceKey::new(
                            "runtime.initial",
                        )),
                        [],
                    ),
                )
            })
            .collect();
        let actor = AmmRuntimeActor {
            runtime_id,
            cache,
            engine,
            commands: command_rx,
            canonical: canonical_rx,
            snapshots: snapshot_tx,
            registry_snapshot,
            revisions,
            version: AmmStateVersion::initial(),
            point,
            trusted: true,
            critical: None,
            critical_capacity: config.critical_change_capacity,
            status: status_tx,
            observers: observer_tx.clone(),
            event_sequence: 0,
            health: initial_health,
            interest_revision: 0,
            interest_revisions: interest_tx,
            subscriber: None,
            cold_start_worker: None,
            scheduled_work: BTreeMap::new(),
            scheduled_discovery: BTreeMap::new(),
            scheduled_followups: BTreeMap::new(),
            pending_pool_followup: BTreeMap::new(),
            pending_followup_intents: BTreeMap::new(),
            pending_pool_work: BTreeMap::new(),
            registration_evidence,
            registration_revalidation: BTreeMap::new(),
            pending_revalidations: BTreeMap::new(),
            pending_lifecycles: BTreeMap::new(),
            discovery_generations: BTreeMap::new(),
            discovery_lifecycles: BTreeMap::new(),
            factory_watchers: BTreeMap::new(),
            factory_watcher_index: AmmFactoryWatcherIndex::default(),
            pending_factory_candidates: VecDeque::new(),
            active_work: BTreeMap::new(),
            queue_depths: QueueDepths::default(),
            next_work_id: WorkId::new(0),
            shutdown: shutdown_rx,
            exited: exit_tx,
        };
        let exited = actor.exited.clone();
        catch_unwind(AssertUnwindSafe(|| {
            tokio::task::spawn_local(async move {
                actor.run().await;
                // `run` owns the actor, so its cache and every other actor-owned
                // resource have been dropped before shutdown becomes observable.
                exited.send_replace(true);
            })
        }))
        .map_err(|_| AmmRuntimeSpawnError::MissingLocalExecutor)?;

        Ok(AmmRuntimeHandle {
            commands: command_tx,
            canonical: canonical_tx,
            snapshots: snapshot_rx,
            status: status_rx,
            interest_revision: interest_rx,
            observers: observer_tx,
            shutdown: shutdown_tx,
            exited: exit_rx,
            next_command_id: Arc::new(AtomicU64::new(0)),
        })
    }
}

/// Monotonic identity of one accepted or attempted runtime command.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AmmCommandId(u64);

impl AmmCommandId {
    /// Numeric command identity.
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Immediate failure from a non-blocking command submission.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum AmmRuntimeSubmitError {
    /// The bounded control queue has no available capacity.
    Full,
    /// The runtime no longer accepts commands.
    Closed,
    /// Command identity allocation was exhausted.
    SequenceExhausted,
}

impl fmt::Display for AmmRuntimeSubmitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Full => write!(f, "AMM runtime submission queue is full"),
            Self::Closed => write!(f, "AMM runtime submission queue is closed"),
            Self::SequenceExhausted => write!(f, "AMM runtime command ids are exhausted"),
        }
    }
}

impl std::error::Error for AmmRuntimeSubmitError {}

/// Accepted command whose completion can be awaited independently.
///
/// Dropping the ticket does not cancel the accepted command.
pub struct AmmCommandTicket<T> {
    id: AmmCommandId,
    response: oneshot::Receiver<Result<T, AmmRuntimeCommandError>>,
}

impl<T> AmmCommandTicket<T> {
    /// Command identity allocated before enqueue.
    pub const fn id(&self) -> AmmCommandId {
        self.id
    }

    /// Await command completion.
    pub async fn wait(self) -> Result<T, AmmRuntimeCommandError> {
        self.response
            .await
            .map_err(|_| AmmRuntimeCommandError::Closed)?
    }
}

/// Cheap cloneable control and publication handle for [`AmmRuntime`].
#[derive(Clone)]
pub struct AmmRuntimeHandle {
    commands: mpsc::Sender<AmmRuntimeCommand>,
    canonical: mpsc::Sender<AmmCanonicalCommand>,
    snapshots: watch::Receiver<Arc<AmmStateSnapshot>>,
    status: watch::Receiver<Arc<AmmRuntimeStatusSnapshot>>,
    interest_revision: watch::Receiver<u64>,
    observers: broadcast::Sender<AmmRuntimeEvent>,
    shutdown: watch::Sender<bool>,
    exited: watch::Receiver<bool>,
    next_command_id: Arc<AtomicU64>,
}

impl AmmRuntimeHandle {
    /// Latest coherent immutable snapshot without borrowing the runtime actor.
    pub fn latest_snapshot(&self) -> Arc<AmmStateSnapshot> {
        self.snapshots.borrow().clone()
    }

    /// Subscribe to recoverable latest-value coherent snapshot updates.
    pub fn subscribe_snapshots(&self) -> watch::Receiver<Arc<AmmStateSnapshot>> {
        self.snapshots.clone()
    }

    /// Latest recoverable lifecycle, work, version, and health status.
    pub fn latest_status(&self) -> Arc<AmmRuntimeStatusSnapshot> {
        self.status.borrow().clone()
    }

    /// Subscribe to recoverable latest-value runtime status updates.
    pub fn subscribe_status(&self) -> watch::Receiver<Arc<AmmRuntimeStatusSnapshot>> {
        self.status.clone()
    }

    /// Interest-set revision required by the next canonical block envelope.
    pub fn interest_revision(&self) -> u64 {
        *self.interest_revision.borrow()
    }

    /// Subscribe to lossy observer events; lag is reported explicitly.
    pub fn subscribe_events(&self) -> AmmObserver {
        AmmObserver {
            events: self.observers.subscribe(),
            exited: self.exited.clone(),
        }
    }

    /// Atomically capture a baseline snapshot and subscribe to strictly newer commits.
    pub async fn subscribe_changes(&self) -> Result<AmmChangeSubscription, AmmRuntimeCommandError> {
        let (response, result) = oneshot::channel();
        self.commands
            .send(AmmRuntimeCommand::SubscribeChanges { response })
            .await
            .map_err(|_| AmmRuntimeCommandError::Closed)?;
        result.await.map_err(|_| AmmRuntimeCommandError::Closed)?
    }

    /// Submit one ordered input batch for canonical actor ingestion.
    ///
    /// The caller-owned transport form is responsible for supplying a complete
    /// canonical boundary. The subscriber driver constructor provides that
    /// guarantee for live operation. Once a subscriber is attached, direct
    /// submission is rejected so external callers cannot bypass its lifecycle
    /// fence or interest revision.
    pub async fn ingest_batch(
        &self,
        batch: AmmCanonicalBatch,
    ) -> Result<Arc<AmmChangeSet>, AmmRuntimeCommandError> {
        let (response, result) = oneshot::channel();
        self.canonical
            .send(AmmCanonicalCommand {
                batch: Box::new(batch),
                origin: AmmCanonicalOrigin::Direct,
                response,
            })
            .await
            .map_err(|_| AmmRuntimeCommandError::Closed)?;
        result.await.map_err(|_| AmmRuntimeCommandError::Closed)?
    }

    /// Non-blockingly enqueue one input batch and return its independent ticket.
    pub fn try_ingest_batch(
        &self,
        batch: AmmCanonicalBatch,
    ) -> Result<AmmCommandTicket<Arc<AmmChangeSet>>, AmmRuntimeSubmitError> {
        let id = self.allocate_command_id()?;
        let (response, result) = oneshot::channel();
        self.canonical
            .try_send(AmmCanonicalCommand {
                batch: Box::new(batch),
                origin: AmmCanonicalOrigin::Direct,
                response,
            })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => AmmRuntimeSubmitError::Full,
                mpsc::error::TrySendError::Closed(_) => AmmRuntimeSubmitError::Closed,
            })?;
        Ok(AmmCommandTicket {
            id,
            response: result,
        })
    }

    fn allocate_command_id(&self) -> Result<AmmCommandId, AmmRuntimeSubmitError> {
        self.next_command_id
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                current.checked_add(1)
            })
            .map(AmmCommandId)
            .map_err(|_| AmmRuntimeSubmitError::SequenceExhausted)
    }

    /// Atomically install already-coherent pool registrations at `baseline`.
    ///
    /// Stage 5 feeds progressively hydrated results through this same commit
    /// path. This Stage 4 command rejects non-ready registrations and stale
    /// baselines rather than pretending raw metadata is immediately quoteable.
    pub async fn install_prepared_pools(
        &self,
        pools: Vec<super::PoolRegistration>,
        baseline: AmmStatePoint,
    ) -> Result<Arc<AmmChangeSet>, AmmRuntimeCommandError> {
        let (response, result) = oneshot::channel();
        self.commands
            .send(AmmRuntimeCommand::InstallPreparedPools {
                pools,
                baseline,
                response,
            })
            .await
            .map_err(|_| AmmRuntimeCommandError::Closed)?;
        result.await.map_err(|_| AmmRuntimeCommandError::Closed)?
    }

    /// Atomically apply one caller-attested hash-pinned result and publish its pool.
    ///
    /// Prefer [`Self::queue_cold_start`] for runtime-owned provider work. This
    /// compatibility seam trusts the caller's claim that the artifact was
    /// fetched at its baseline; it cannot independently authenticate an
    /// arbitrary caller's RPC response.
    pub async fn commit_prepared_pool(
        &self,
        prepared: AmmPreparedPoolState,
    ) -> Result<Arc<AmmChangeSet>, AmmRuntimeCommandError> {
        let (response, result) = oneshot::channel();
        self.commands
            .send(AmmRuntimeCommand::CommitPreparedPool { prepared, response })
            .await
            .map_err(|_| AmmRuntimeCommandError::Closed)?;
        result.await.map_err(|_| AmmRuntimeCommandError::Closed)?
    }

    /// Flush the actor-owned EVM cache while canonical input is quiescent.
    ///
    /// Callers can pair the successful flush with a hash-certified checkpoint;
    /// a warm restart must still verify that checkpoint against the provider
    /// before trusting the persisted state.
    pub async fn flush_persistent_cache(&self) -> Result<(), AmmRuntimeCommandError> {
        let (response, result) = oneshot::channel();
        self.commands
            .send(AmmRuntimeCommand::FlushPersistentCache { response })
            .await
            .map_err(|_| AmmRuntimeCommandError::Closed)?;
        result.await.map_err(|_| AmmRuntimeCommandError::Closed)?
    }

    /// Non-blockingly enqueue an atomic prepared-pool installation.
    pub fn try_install_prepared_pools(
        &self,
        pools: Vec<super::PoolRegistration>,
        baseline: AmmStatePoint,
    ) -> Result<AmmCommandTicket<Arc<AmmChangeSet>>, AmmRuntimeSubmitError> {
        let id = self.allocate_command_id()?;
        let (response, result) = oneshot::channel();
        self.commands
            .try_send(AmmRuntimeCommand::InstallPreparedPools {
                pools,
                baseline,
                response,
            })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => AmmRuntimeSubmitError::Full,
                mpsc::error::TrySendError::Closed(_) => AmmRuntimeSubmitError::Closed,
            })?;
        Ok(AmmCommandTicket {
            id,
            response: result,
        })
    }

    /// Remove one exact pool generation with explicit cache treatment.
    pub async fn remove_pool(
        &self,
        pool: super::PoolInstanceId,
        eviction: super::AmmEvictionPolicy,
    ) -> Result<Arc<AmmChangeSet>, AmmRuntimeCommandError> {
        let (response, result) = oneshot::channel();
        self.commands
            .send(AmmRuntimeCommand::RemovePool {
                pool,
                eviction,
                response,
            })
            .await
            .map_err(|_| AmmRuntimeCommandError::Closed)?;
        result.await.map_err(|_| AmmRuntimeCommandError::Closed)?
    }

    /// Non-blockingly enqueue exact-generation pool removal.
    pub fn try_remove_pool(
        &self,
        pool: super::PoolInstanceId,
        eviction: super::AmmEvictionPolicy,
    ) -> Result<AmmCommandTicket<Arc<AmmChangeSet>>, AmmRuntimeSubmitError> {
        let id = self.allocate_command_id()?;
        let (response, result) = oneshot::channel();
        self.commands
            .try_send(AmmRuntimeCommand::RemovePool {
                pool,
                eviction,
                response,
            })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => AmmRuntimeSubmitError::Full,
                mpsc::error::TrySendError::Closed(_) => AmmRuntimeSubmitError::Closed,
            })?;
        Ok(AmmCommandTicket {
            id,
            response: result,
        })
    }

    /// Attach a bounded provider worker for progressive cold-start jobs.
    pub async fn attach_cold_start_worker<P>(
        &self,
        provider: P,
        config: AmmColdStartWorkerConfig,
    ) -> Result<AmmColdStartWorkerHandle, AmmColdStartWorkerError>
    where
        P: alloy_provider::Provider<alloy_network::AnyNetwork> + Clone + Send + Sync + 'static,
    {
        let (control, handle) = spawn_cold_start_worker(self.clone(), provider, config)?;
        let (response, result) = oneshot::channel();
        self.commands
            .send(AmmRuntimeCommand::AttachColdStartWorker { control, response })
            .await
            .map_err(|_| AmmColdStartWorkerError::Closed)?;
        if let Err(error) = result.await.map_err(|_| AmmColdStartWorkerError::Closed)? {
            handle.shutdown();
            return Err(error.into());
        }
        Ok(handle)
    }

    /// Queue pool hydration and return generation-owned work immediately.
    pub async fn queue_cold_start(
        &self,
        pools: Vec<super::PoolRegistration>,
        options: AmmColdStartOptions,
    ) -> Result<Vec<AmmScheduledPool>, AmmRuntimeCommandError> {
        let (response, result) = oneshot::channel();
        self.commands
            .send(AmmRuntimeCommand::QueueColdStart {
                pools,
                options,
                response,
            })
            .await
            .map_err(|_| AmmRuntimeCommandError::Closed)?;
        result.await.map_err(|_| AmmRuntimeCommandError::Closed)?
    }

    /// Dynamically register one adapter family and publish a coherent topology snapshot.
    pub async fn add_adapter(
        &self,
        adapter: Arc<dyn super::AmmAdapter>,
    ) -> Result<super::AdapterInstanceId, AmmRuntimeCommandError> {
        let (response, result) = oneshot::channel();
        self.commands
            .send(AmmRuntimeCommand::AddAdapter { adapter, response })
            .await
            .map_err(|_| AmmRuntimeCommandError::Closed)?;
        result.await.map_err(|_| AmmRuntimeCommandError::Closed)?
    }

    /// Remove one exact unused adapter generation and publish its disappearance.
    pub async fn remove_adapter(
        &self,
        adapter: super::AdapterInstanceId,
    ) -> Result<(), AmmRuntimeCommandError> {
        let (response, result) = oneshot::channel();
        self.commands
            .send(AmmRuntimeCommand::RemoveAdapter { adapter, response })
            .await
            .map_err(|_| AmmRuntimeCommandError::Closed)?;
        result.await.map_err(|_| AmmRuntimeCommandError::Closed)?
    }

    /// Atomically remove one exact adapter generation and every dependent pool.
    pub async fn remove_adapter_cascade(
        &self,
        adapter: super::AdapterInstanceId,
        eviction: super::AmmEvictionPolicy,
    ) -> Result<Arc<AmmChangeSet>, AmmRuntimeCommandError> {
        let (response, result) = oneshot::channel();
        self.commands
            .send(AmmRuntimeCommand::RemoveAdapterCascade {
                adapter,
                eviction,
                response,
            })
            .await
            .map_err(|_| AmmRuntimeCommandError::Closed)?;
        result.await.map_err(|_| AmmRuntimeCommandError::Closed)?
    }

    /// Install one generation-owned factory creation watcher.
    pub async fn add_factory_watcher(
        &self,
        registration: AmmFactoryWatcherRegistration,
    ) -> Result<DiscoveryOwnerId, AmmRuntimeCommandError> {
        let (response, result) = oneshot::channel();
        self.commands
            .send(AmmRuntimeCommand::AddFactoryWatcher {
                registration,
                response,
            })
            .await
            .map_err(|_| AmmRuntimeCommandError::Closed)?;
        result.await.map_err(|_| AmmRuntimeCommandError::Closed)?
    }

    /// Remove one exact factory watcher generation and its subscriber ownership.
    pub async fn remove_factory_watcher(
        &self,
        owner: DiscoveryOwnerId,
    ) -> Result<(), AmmRuntimeCommandError> {
        let (response, result) = oneshot::channel();
        self.commands
            .send(AmmRuntimeCommand::RemoveFactoryWatcher { owner, response })
            .await
            .map_err(|_| AmmRuntimeCommandError::Closed)?;
        result.await.map_err(|_| AmmRuntimeCommandError::Closed)?
    }

    /// Queue one connector-focused discovery request on an exact watcher generation.
    pub async fn queue_token_discovery(
        &self,
        owner: DiscoveryOwnerId,
        request: TokenEdgeDiscoveryRequest,
        options: AmmDiscoveryOptions,
    ) -> Result<AmmScheduledDiscovery, AmmRuntimeCommandError> {
        let (response, result) = oneshot::channel();
        self.commands
            .send(AmmRuntimeCommand::QueueDiscovery {
                owner,
                request,
                options,
                response,
            })
            .await
            .map_err(|_| AmmRuntimeCommandError::Closed)?;
        result.await.map_err(|_| AmmRuntimeCommandError::Closed)?
    }

    /// Queue one required repair for an exact active pool generation.
    pub async fn queue_repair(
        &self,
        pool: super::PoolInstanceId,
        action: RepairAction,
    ) -> Result<AmmScheduledFollowUp, AmmRuntimeCommandError> {
        let (response, result) = oneshot::channel();
        self.commands
            .send(AmmRuntimeCommand::QueueRepair {
                pool,
                action,
                response,
            })
            .await
            .map_err(|_| AmmRuntimeCommandError::Closed)?;
        result.await.map_err(|_| AmmRuntimeCommandError::Closed)?
    }

    /// Queue normalized optional warming produced by a searchable cold-start.
    pub async fn queue_deferred(
        &self,
        pool: super::PoolInstanceId,
        deferred: Vec<DeferredWork>,
    ) -> Result<Option<AmmScheduledFollowUp>, AmmRuntimeCommandError> {
        let (response, result) = oneshot::channel();
        self.commands
            .send(AmmRuntimeCommand::QueueDeferred {
                pool,
                deferred,
                response,
            })
            .await
            .map_err(|_| AmmRuntimeCommandError::Closed)?;
        result.await.map_err(|_| AmmRuntimeCommandError::Closed)?
    }

    /// Cancel one exact scheduled work attempt.
    pub async fn cancel_work(&self, work: RuntimeWorkId) -> Result<(), AmmRuntimeCommandError> {
        self.cancel_scheduled_work(work).await
    }

    /// Promptly stop the actor and release tasks waiting on bounded capacity.
    ///
    /// Commands that were accepted but not committed complete with
    /// [`AmmRuntimeCommandError::Closed`]. Repeated shutdown calls are safe.
    pub async fn shutdown(&self) -> Result<(), AmmRuntimeCommandError> {
        self.shutdown.send_replace(true);
        let mut exited = self.exited.clone();
        while !*exited.borrow() {
            if exited.changed().await.is_err() {
                break;
            }
        }
        Ok(())
    }

    pub(crate) async fn attach_subscriber_control(
        &self,
        subscriber: AmmSubscriberControl,
    ) -> Result<(), AmmRuntimeCommandError> {
        let (response, result) = oneshot::channel();
        self.commands
            .send(AmmRuntimeCommand::AttachSubscriber {
                subscriber,
                response,
            })
            .await
            .map_err(|_| AmmRuntimeCommandError::Closed)?;
        result.await.map_err(|_| AmmRuntimeCommandError::Closed)?
    }

    pub(crate) async fn report_subscriber_failure(
        &self,
        message: String,
    ) -> Result<(), AmmRuntimeCommandError> {
        let (response, result) = oneshot::channel();
        self.commands
            .send(AmmRuntimeCommand::ReportSubscriberFailure { message, response })
            .await
            .map_err(|_| AmmRuntimeCommandError::Closed)?;
        result.await.map_err(|_| AmmRuntimeCommandError::Closed)?
    }

    pub(crate) async fn ingest_subscriber_batch(
        &self,
        batch: AmmCanonicalBatch,
    ) -> Result<Arc<AmmChangeSet>, AmmRuntimeCommandError> {
        let (response, result) = oneshot::channel();
        self.canonical
            .send(AmmCanonicalCommand {
                batch: Box::new(batch),
                origin: AmmCanonicalOrigin::Subscriber,
                response,
            })
            .await
            .map_err(|_| AmmRuntimeCommandError::Closed)?;
        result.await.map_err(|_| AmmRuntimeCommandError::Closed)?
    }

    pub(crate) async fn mark_scheduled_work_started(
        &self,
        work: RuntimeWorkId,
    ) -> Result<(), AmmRuntimeCommandError> {
        let (response, result) = oneshot::channel();
        self.commands
            .send(AmmRuntimeCommand::StartScheduledWork { work, response })
            .await
            .map_err(|_| AmmRuntimeCommandError::Closed)?;
        result.await.map_err(|_| AmmRuntimeCommandError::Closed)?
    }

    pub(crate) async fn report_scheduled_round(
        &self,
        work: RuntimeWorkId,
        round: u64,
        next_round: Option<u64>,
    ) -> Result<(), AmmRuntimeCommandError> {
        let (response, result) = oneshot::channel();
        self.commands
            .send(AmmRuntimeCommand::ReportScheduledRound {
                work,
                round,
                next_round,
                response,
            })
            .await
            .map_err(|_| AmmRuntimeCommandError::Closed)?;
        result.await.map_err(|_| AmmRuntimeCommandError::Closed)?
    }

    pub(crate) async fn begin_scheduled_round(
        &self,
        work: RuntimeWorkId,
        round: u64,
    ) -> Result<(), AmmRuntimeCommandError> {
        let (response, result) = oneshot::channel();
        self.commands
            .send(AmmRuntimeCommand::BeginScheduledRound {
                work,
                round,
                response,
            })
            .await
            .map_err(|_| AmmRuntimeCommandError::Closed)?;
        result.await.map_err(|_| AmmRuntimeCommandError::Closed)?
    }

    pub(crate) async fn commit_scheduled_pool(
        &self,
        prepared: AmmPreparedPoolState,
    ) -> Result<Arc<AmmChangeSet>, AmmRuntimeCommandError> {
        let (response, result) = oneshot::channel();
        self.commands
            .send(AmmRuntimeCommand::CommitScheduledPool { prepared, response })
            .await
            .map_err(|_| AmmRuntimeCommandError::Closed)?;
        result.await.map_err(|_| AmmRuntimeCommandError::Closed)?
    }

    pub(crate) async fn commit_scheduled_discovery(
        &self,
        work: RuntimeWorkId,
        owner: DiscoveryOwnerId,
        report: TokenEdgeDiscoveryReport,
    ) -> Result<(), AmmRuntimeCommandError> {
        let (response, result) = oneshot::channel();
        self.commands
            .send(AmmRuntimeCommand::CommitScheduledDiscovery {
                work,
                owner,
                report,
                response,
            })
            .await
            .map_err(|_| AmmRuntimeCommandError::Closed)?;
        result.await.map_err(|_| AmmRuntimeCommandError::Closed)?
    }

    pub(crate) async fn commit_scheduled_refresh(
        &self,
        prepared: AmmPreparedPoolState,
    ) -> Result<Arc<AmmChangeSet>, AmmRuntimeCommandError> {
        let (response, result) = oneshot::channel();
        self.commands
            .send(AmmRuntimeCommand::CommitScheduledRefresh { prepared, response })
            .await
            .map_err(|_| AmmRuntimeCommandError::Closed)?;
        result.await.map_err(|_| AmmRuntimeCommandError::Closed)?
    }

    pub(crate) async fn commit_scheduled_slot_patch(
        &self,
        work: RuntimeWorkId,
        pool: super::PoolInstanceId,
        baseline: AmmStatePoint,
        storage: Vec<AmmPreparedStorage>,
    ) -> Result<Arc<AmmChangeSet>, AmmRuntimeCommandError> {
        let (response, result) = oneshot::channel();
        self.commands
            .send(AmmRuntimeCommand::CommitScheduledSlotPatch {
                work,
                pool,
                baseline,
                storage,
                response,
            })
            .await
            .map_err(|_| AmmRuntimeCommandError::Closed)?;
        result.await.map_err(|_| AmmRuntimeCommandError::Closed)?
    }

    pub(crate) async fn fail_scheduled_work(
        &self,
        work: RuntimeWorkId,
        message: String,
    ) -> Result<(), AmmRuntimeCommandError> {
        let (response, result) = oneshot::channel();
        self.commands
            .send(AmmRuntimeCommand::FailScheduledWork {
                work,
                message,
                response,
            })
            .await
            .map_err(|_| AmmRuntimeCommandError::Closed)?;
        result.await.map_err(|_| AmmRuntimeCommandError::Closed)?
    }

    pub(crate) async fn cancel_scheduled_work(
        &self,
        work: RuntimeWorkId,
    ) -> Result<(), AmmRuntimeCommandError> {
        let (response, result) = oneshot::channel();
        self.commands
            .send(AmmRuntimeCommand::CancelScheduledWork { work, response })
            .await
            .map_err(|_| AmmRuntimeCommandError::Closed)?;
        result.await.map_err(|_| AmmRuntimeCommandError::Closed)?
    }

    pub(crate) fn shutdown_requested(&self) -> bool {
        *self.shutdown.borrow()
    }
}

/// Error receiving from the lossy observer stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum AmmObserverError {
    /// The observer skipped events because its bounded receiver lagged.
    Lagged {
        /// Number of skipped observer events.
        skipped: u64,
    },
    /// The runtime closed the observer channel.
    Closed,
}

impl fmt::Display for AmmObserverError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Lagged { skipped } => write!(f, "AMM observer lagged by {skipped} events"),
            Self::Closed => write!(f, "AMM observer channel is closed"),
        }
    }
}

impl std::error::Error for AmmObserverError {}

/// Lossy lifecycle and diagnostic observer receiver.
pub struct AmmObserver {
    events: broadcast::Receiver<AmmRuntimeEvent>,
    exited: watch::Receiver<bool>,
}

impl AmmObserver {
    /// Receive the next event or an explicit lag/closure error.
    pub async fn next_event(&mut self) -> Result<AmmRuntimeEvent, AmmObserverError> {
        loop {
            if *self.exited.borrow() && self.events.is_empty() {
                return Err(AmmObserverError::Closed);
            }
            tokio::select! {
                biased;
                event = self.events.recv() => return event.map_err(|error| match error {
                    broadcast::error::RecvError::Lagged(skipped) => AmmObserverError::Lagged { skipped },
                    broadcast::error::RecvError::Closed => AmmObserverError::Closed,
                }),
                changed = self.exited.changed() => {
                    if changed.is_err() || (*self.exited.borrow() && self.events.is_empty()) {
                        return Err(AmmObserverError::Closed);
                    }
                }
            }
        }
    }
}

/// Atomic baseline plus reliable contiguous commits for the canonical consumer.
pub struct AmmChangeSubscription {
    snapshot: Arc<AmmStateSnapshot>,
    commits: mpsc::Receiver<Arc<AmmStateCommit>>,
}

impl AmmChangeSubscription {
    /// Explicit baseline captured before this receiver was registered.
    pub fn snapshot(&self) -> &Arc<AmmStateSnapshot> {
        &self.snapshot
    }

    /// Receive the next commit, whose version is strictly newer than the baseline.
    pub async fn next_commit(&mut self) -> Option<Arc<AmmStateCommit>> {
        self.commits.recv().await
    }

    /// Take one immediately available commit without waiting.
    ///
    /// `None` means the reliable queue is currently empty or has closed; a
    /// later [`Self::next_commit`] distinguishes those states for consumers
    /// that need to keep waiting.
    pub fn try_next_commit(&mut self) -> Option<Arc<AmmStateCommit>> {
        self.commits.try_recv().ok()
    }
}

enum AmmRuntimeCommand {
    AddAdapter {
        adapter: Arc<dyn super::AmmAdapter>,
        response: oneshot::Sender<Result<super::AdapterInstanceId, AmmRuntimeCommandError>>,
    },
    RemoveAdapter {
        adapter: super::AdapterInstanceId,
        response: oneshot::Sender<Result<(), AmmRuntimeCommandError>>,
    },
    RemoveAdapterCascade {
        adapter: super::AdapterInstanceId,
        eviction: super::AmmEvictionPolicy,
        response: oneshot::Sender<Result<Arc<AmmChangeSet>, AmmRuntimeCommandError>>,
    },
    AddFactoryWatcher {
        registration: AmmFactoryWatcherRegistration,
        response: oneshot::Sender<Result<DiscoveryOwnerId, AmmRuntimeCommandError>>,
    },
    RemoveFactoryWatcher {
        owner: DiscoveryOwnerId,
        response: oneshot::Sender<Result<(), AmmRuntimeCommandError>>,
    },
    QueueDiscovery {
        owner: DiscoveryOwnerId,
        request: TokenEdgeDiscoveryRequest,
        options: AmmDiscoveryOptions,
        response: oneshot::Sender<Result<AmmScheduledDiscovery, AmmRuntimeCommandError>>,
    },
    QueueRepair {
        pool: super::PoolInstanceId,
        action: RepairAction,
        response: oneshot::Sender<Result<AmmScheduledFollowUp, AmmRuntimeCommandError>>,
    },
    QueueDeferred {
        pool: super::PoolInstanceId,
        deferred: Vec<DeferredWork>,
        response: oneshot::Sender<Result<Option<AmmScheduledFollowUp>, AmmRuntimeCommandError>>,
    },
    AttachColdStartWorker {
        control: AmmColdStartWorkerControl,
        response: oneshot::Sender<Result<(), AmmRuntimeCommandError>>,
    },
    QueueColdStart {
        pools: Vec<PoolRegistration>,
        options: AmmColdStartOptions,
        response: oneshot::Sender<Result<Vec<AmmScheduledPool>, AmmRuntimeCommandError>>,
    },
    StartScheduledWork {
        work: RuntimeWorkId,
        response: oneshot::Sender<Result<(), AmmRuntimeCommandError>>,
    },
    ReportScheduledRound {
        work: RuntimeWorkId,
        round: u64,
        next_round: Option<u64>,
        response: oneshot::Sender<Result<(), AmmRuntimeCommandError>>,
    },
    BeginScheduledRound {
        work: RuntimeWorkId,
        round: u64,
        response: oneshot::Sender<Result<(), AmmRuntimeCommandError>>,
    },
    CommitScheduledPool {
        prepared: AmmPreparedPoolState,
        response: oneshot::Sender<Result<Arc<AmmChangeSet>, AmmRuntimeCommandError>>,
    },
    CommitScheduledDiscovery {
        work: RuntimeWorkId,
        owner: DiscoveryOwnerId,
        report: TokenEdgeDiscoveryReport,
        response: oneshot::Sender<Result<(), AmmRuntimeCommandError>>,
    },
    CommitScheduledRefresh {
        prepared: AmmPreparedPoolState,
        response: oneshot::Sender<Result<Arc<AmmChangeSet>, AmmRuntimeCommandError>>,
    },
    CommitScheduledSlotPatch {
        work: RuntimeWorkId,
        pool: super::PoolInstanceId,
        baseline: AmmStatePoint,
        storage: Vec<AmmPreparedStorage>,
        response: oneshot::Sender<Result<Arc<AmmChangeSet>, AmmRuntimeCommandError>>,
    },
    FailScheduledWork {
        work: RuntimeWorkId,
        message: String,
        response: oneshot::Sender<Result<(), AmmRuntimeCommandError>>,
    },
    CancelScheduledWork {
        work: RuntimeWorkId,
        response: oneshot::Sender<Result<(), AmmRuntimeCommandError>>,
    },
    AttachSubscriber {
        subscriber: AmmSubscriberControl,
        response: oneshot::Sender<Result<(), AmmRuntimeCommandError>>,
    },
    ReportSubscriberFailure {
        message: String,
        response: oneshot::Sender<Result<(), AmmRuntimeCommandError>>,
    },
    SubscribeChanges {
        response: oneshot::Sender<Result<AmmChangeSubscription, AmmRuntimeCommandError>>,
    },
    InstallPreparedPools {
        pools: Vec<super::PoolRegistration>,
        baseline: AmmStatePoint,
        response: oneshot::Sender<Result<Arc<AmmChangeSet>, AmmRuntimeCommandError>>,
    },
    CommitPreparedPool {
        prepared: AmmPreparedPoolState,
        response: oneshot::Sender<Result<Arc<AmmChangeSet>, AmmRuntimeCommandError>>,
    },
    FlushPersistentCache {
        response: oneshot::Sender<Result<(), AmmRuntimeCommandError>>,
    },
    RemovePool {
        pool: super::PoolInstanceId,
        eviction: super::AmmEvictionPolicy,
        response: oneshot::Sender<Result<Arc<AmmChangeSet>, AmmRuntimeCommandError>>,
    },
}

#[derive(Clone)]
struct AmmScheduledPoolWork {
    reservation: AmmPoolGenerationReservation,
    registration: PoolRegistration,
    adapter: super::AdapterInstanceId,
    discovery_owner: Option<DiscoveryOwnerId>,
    supporting_discovery: BTreeSet<DiscoveryOwnerId>,
    evidence: RegistrationEvidenceSet,
    revalidations: BTreeMap<DiscoveryOwnerId, TokenEdgeDiscoveryRequest>,
    class: AmmWorkClass,
    queued: bool,
}

#[derive(Clone)]
struct AmmScheduledDiscoveryWork {
    owner: DiscoveryOwnerId,
    request: TokenEdgeDiscoveryRequest,
    baseline: AmmStatePoint,
    class: AmmWorkClass,
    max_candidates: Option<usize>,
    queued: bool,
    revalidate_pool: Option<super::PoolInstanceId>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum AmmFollowUpTask {
    Refresh {
        policy: super::ColdStartPolicy,
        after_slots: Vec<(alloy_primitives::Address, alloy_primitives::U256)>,
    },
    SlotPatch {
        slots: Vec<(alloy_primitives::Address, alloy_primitives::U256)>,
    },
}

#[derive(Clone)]
struct AmmScheduledFollowUpWork {
    pool: super::PoolInstanceId,
    class: AmmWorkClass,
    kind: AmmWorkKind,
    task: AmmFollowUpTask,
    queued: bool,
}

#[derive(Clone, Copy)]
struct AmmPreparedInstallPolicy<'a> {
    require_artifact: bool,
    allowed_missing_slots: &'a BTreeSet<(alloy_primitives::Address, alloy_primitives::U256)>,
}

impl<'a> AmmPreparedInstallPolicy<'a> {
    const fn new(
        require_artifact: bool,
        allowed_missing_slots: &'a BTreeSet<(alloy_primitives::Address, alloy_primitives::U256)>,
    ) -> Self {
        Self {
            require_artifact,
            allowed_missing_slots,
        }
    }
}

#[derive(Clone)]
struct AmmActiveFactoryWatcher {
    ownership: DiscoveryOwnership,
    discovery: Arc<PoolDiscovery>,
    handler: HandlerId,
    sources: Vec<super::EventSource>,
}

#[derive(Default)]
struct AmmFactoryWatcherIndex {
    exact:
        BTreeMap<(alloy_primitives::Address, alloy_primitives::B256), BTreeSet<DiscoveryOwnerId>>,
    wildcard_topics: BTreeMap<alloy_primitives::Address, BTreeSet<DiscoveryOwnerId>>,
}

impl AmmFactoryWatcherIndex {
    fn insert(&mut self, owner: &DiscoveryOwnerId, sources: &[super::EventSource]) {
        for source in sources {
            if source.topics.is_empty() {
                self.wildcard_topics
                    .entry(source.emitter)
                    .or_default()
                    .insert(owner.clone());
            } else {
                for topic in &source.topics {
                    self.exact
                        .entry((source.emitter, *topic))
                        .or_default()
                        .insert(owner.clone());
                }
            }
        }
    }

    fn remove(&mut self, owner: &DiscoveryOwnerId, sources: &[super::EventSource]) {
        for source in sources {
            if source.topics.is_empty() {
                remove_factory_index_owner(&mut self.wildcard_topics, &source.emitter, owner);
            } else {
                for topic in &source.topics {
                    remove_factory_index_owner(&mut self.exact, &(source.emitter, *topic), owner);
                }
            }
        }
    }

    fn owners_for(&self, log: &alloy_rpc_types_eth::Log) -> BTreeSet<DiscoveryOwnerId> {
        let wildcard = self
            .wildcard_topics
            .get(&log.inner.address)
            .into_iter()
            .flatten();
        let exact = log
            .inner
            .topics()
            .first()
            .and_then(|topic| self.exact.get(&(log.inner.address, *topic)))
            .into_iter()
            .flatten();
        wildcard.chain(exact).cloned().collect()
    }
}

fn remove_factory_index_owner<K: Ord>(
    index: &mut BTreeMap<K, BTreeSet<DiscoveryOwnerId>>,
    key: &K,
    owner: &DiscoveryOwnerId,
) {
    let remove_key = index.get_mut(key).is_some_and(|owners| {
        owners.remove(owner);
        owners.is_empty()
    });
    if remove_key {
        index.remove(key);
    }
}

struct AmmFactoryCandidate {
    owner: DiscoveryOwnerId,
    registration: PoolRegistration,
    evidence: RegistrationEvidenceSet,
    revalidate: Option<TokenEdgeDiscoveryRequest>,
}

#[derive(Clone)]
enum AmmPendingFollowUpIntent {
    Repair(RepairAction),
    Deferred(Vec<DeferredWork>),
}

struct AmmCanonicalCommand {
    batch: Box<AmmCanonicalBatch>,
    origin: AmmCanonicalOrigin,
    response: oneshot::Sender<Result<Arc<AmmChangeSet>, AmmRuntimeCommandError>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AmmCanonicalOrigin {
    Direct,
    Subscriber,
}

struct AmmRuntimeActor {
    runtime_id: super::AmmRuntimeId,
    cache: EvmCache,
    engine: AmmSyncEngine,
    commands: mpsc::Receiver<AmmRuntimeCommand>,
    canonical: mpsc::Receiver<AmmCanonicalCommand>,
    snapshots: watch::Sender<Arc<AmmStateSnapshot>>,
    registry_snapshot: Arc<AdapterRegistrySnapshot>,
    revisions: Arc<PoolRevisionMap>,
    version: AmmStateVersion,
    point: AmmStatePoint,
    trusted: bool,
    critical: Option<mpsc::Sender<Arc<AmmStateCommit>>>,
    critical_capacity: usize,
    status: watch::Sender<Arc<AmmRuntimeStatusSnapshot>>,
    observers: broadcast::Sender<AmmRuntimeEvent>,
    event_sequence: u64,
    health: AmmRuntimeHealth,
    interest_revision: u64,
    interest_revisions: watch::Sender<u64>,
    subscriber: Option<AmmSubscriberControl>,
    cold_start_worker: Option<AmmColdStartWorkerControl>,
    scheduled_work: BTreeMap<RuntimeWorkId, AmmScheduledPoolWork>,
    scheduled_discovery: BTreeMap<RuntimeWorkId, AmmScheduledDiscoveryWork>,
    scheduled_followups: BTreeMap<RuntimeWorkId, AmmScheduledFollowUpWork>,
    pending_pool_followup: BTreeMap<super::PoolInstanceId, RuntimeWorkId>,
    pending_followup_intents: BTreeMap<super::PoolInstanceId, AmmPendingFollowUpIntent>,
    pending_pool_work: BTreeMap<PoolKey, RuntimeWorkId>,
    registration_evidence: BTreeMap<super::PoolInstanceId, RegistrationEvidenceSet>,
    registration_revalidation:
        BTreeMap<super::PoolInstanceId, BTreeMap<DiscoveryOwnerId, TokenEdgeDiscoveryRequest>>,
    pending_revalidations:
        BTreeMap<(super::PoolInstanceId, DiscoveryOwnerId), TokenEdgeDiscoveryRequest>,
    pending_lifecycles: BTreeMap<super::PoolInstanceId, PoolRuntimeState>,
    discovery_generations: BTreeMap<DiscoveryOwnerKey, DiscoveryGeneration>,
    discovery_lifecycles: BTreeMap<DiscoveryOwnerId, super::OwnerRuntimeState>,
    factory_watchers: BTreeMap<DiscoveryOwnerId, AmmActiveFactoryWatcher>,
    factory_watcher_index: AmmFactoryWatcherIndex,
    pending_factory_candidates: VecDeque<AmmFactoryCandidate>,
    active_work: BTreeMap<RuntimeWorkId, AmmWorkProgress>,
    queue_depths: QueueDepths,
    next_work_id: WorkId,
    shutdown: watch::Receiver<bool>,
    exited: watch::Sender<bool>,
}

const MAX_CANONICAL_BURST: usize = 16;

enum AmmActorInput {
    Shutdown,
    Control(Option<Box<AmmRuntimeCommand>>),
    Canonical(Option<AmmCanonicalCommand>),
}

impl AmmRuntimeActor {
    async fn run(mut self) {
        let mut controls_open = true;
        let mut canonical_open = true;
        let mut canonical_streak = 0usize;
        while controls_open || canonical_open {
            if *self.shutdown.borrow() {
                break;
            }
            match self
                .next_input(
                    canonical_streak >= MAX_CANONICAL_BURST,
                    controls_open,
                    canonical_open,
                )
                .await
            {
                AmmActorInput::Shutdown => break,
                AmmActorInput::Control(Some(command)) => {
                    canonical_streak = 0;
                    self.handle_control(*command).await;
                }
                AmmActorInput::Control(None) => controls_open = false,
                AmmActorInput::Canonical(Some(command)) => {
                    canonical_streak = canonical_streak.saturating_add(1);
                    self.handle_canonical(command).await;
                }
                AmmActorInput::Canonical(None) => canonical_open = false,
            }
        }
        if let Some(worker) = self.cold_start_worker.take() {
            worker.shutdown_for_runtime();
        }
        if let Some(subscriber) = self.subscriber.take() {
            subscriber.shutdown_for_runtime();
        }
        self.publish_shutting_down();
    }

    async fn next_input(
        &mut self,
        prefer_control: bool,
        controls_open: bool,
        canonical_open: bool,
    ) -> AmmActorInput {
        if prefer_control {
            tokio::select! {
                biased;
                _ = self.shutdown.changed() => AmmActorInput::Shutdown,
                command = self.commands.recv(), if controls_open => AmmActorInput::Control(command.map(Box::new)),
                command = self.canonical.recv(), if canonical_open => AmmActorInput::Canonical(command),
            }
        } else {
            tokio::select! {
                biased;
                _ = self.shutdown.changed() => AmmActorInput::Shutdown,
                command = self.canonical.recv(), if canonical_open => AmmActorInput::Canonical(command),
                command = self.commands.recv(), if controls_open => AmmActorInput::Control(command.map(Box::new)),
            }
        }
    }

    async fn handle_control(&mut self, command: AmmRuntimeCommand) {
        match command {
            AmmRuntimeCommand::AddAdapter { adapter, response } => {
                let _ = response.send(self.add_adapter(adapter).await);
            }
            AmmRuntimeCommand::RemoveAdapter { adapter, response } => {
                let _ = response.send(self.remove_adapter(adapter).await);
            }
            AmmRuntimeCommand::RemoveAdapterCascade {
                adapter,
                eviction,
                response,
            } => {
                let _ = response.send(self.remove_adapter_cascade(adapter, eviction).await);
            }
            AmmRuntimeCommand::AddFactoryWatcher {
                registration,
                response,
            } => {
                let _ = response.send(self.add_factory_watcher(registration).await);
            }
            AmmRuntimeCommand::RemoveFactoryWatcher { owner, response } => {
                let _ = response.send(self.remove_factory_watcher(owner).await);
            }
            AmmRuntimeCommand::QueueDiscovery {
                owner,
                request,
                options,
                response,
            } => {
                let _ = response.send(self.queue_discovery(owner, request, options));
            }
            AmmRuntimeCommand::QueueRepair {
                pool,
                action,
                response,
            } => {
                let _ = response.send(self.queue_repair(pool, action));
            }
            AmmRuntimeCommand::QueueDeferred {
                pool,
                deferred,
                response,
            } => {
                let _ = response.send(self.queue_deferred(pool, deferred));
            }
            AmmRuntimeCommand::AttachColdStartWorker { control, response } => {
                let _ = response.send(self.attach_cold_start_worker(control));
            }
            AmmRuntimeCommand::QueueColdStart {
                pools,
                options,
                response,
            } => {
                let _ = response.send(self.queue_cold_start(pools, options));
            }
            AmmRuntimeCommand::StartScheduledWork { work, response } => {
                let _ = response.send(self.start_scheduled_work(&work));
            }
            AmmRuntimeCommand::ReportScheduledRound {
                work,
                round,
                next_round,
                response,
            } => {
                let _ = response.send(self.report_scheduled_round(&work, round, next_round));
            }
            AmmRuntimeCommand::BeginScheduledRound {
                work,
                round,
                response,
            } => {
                let _ = response.send(self.begin_scheduled_round(&work, round));
            }
            AmmRuntimeCommand::CommitScheduledPool { prepared, response } => {
                let _ = response.send(self.commit_scheduled_pool(prepared).await);
            }
            AmmRuntimeCommand::CommitScheduledDiscovery {
                work,
                owner,
                report,
                response,
            } => {
                let _ = response.send(self.commit_scheduled_discovery(work, owner, report).await);
            }
            AmmRuntimeCommand::CommitScheduledRefresh { prepared, response } => {
                let _ = response.send(self.commit_scheduled_refresh(prepared).await);
            }
            AmmRuntimeCommand::CommitScheduledSlotPatch {
                work,
                pool,
                baseline,
                storage,
                response,
            } => {
                let _ = response.send(
                    self.commit_scheduled_slot_patch(work, pool, baseline, storage)
                        .await,
                );
            }
            AmmRuntimeCommand::FailScheduledWork {
                work,
                message,
                response,
            } => {
                let _ = response.send(self.fail_scheduled_work(&work, message));
            }
            AmmRuntimeCommand::CancelScheduledWork { work, response } => {
                let _ = response.send(self.cancel_scheduled_work(&work));
            }
            AmmRuntimeCommand::AttachSubscriber {
                subscriber,
                response,
            } => {
                let _ = response.send(self.attach_subscriber(subscriber).await);
            }
            AmmRuntimeCommand::ReportSubscriberFailure { message, response } => {
                self.mark_untrusted();
                let _ = response.send(Err(AmmRuntimeCommandError::Subscriber(message)));
            }
            AmmRuntimeCommand::SubscribeChanges { response } => {
                let _ = response.send(self.subscribe_changes());
            }
            AmmRuntimeCommand::InstallPreparedPools {
                pools,
                baseline,
                response,
            } => {
                let _ = response.send(
                    self.install_prepared_pools(
                        pools,
                        baseline,
                        &[],
                        None,
                        None,
                        AmmPreparedInstallPolicy::new(false, &BTreeSet::new()),
                    )
                    .await,
                );
            }
            AmmRuntimeCommand::CommitPreparedPool { prepared, response } => {
                let (registration, baseline, storage, accounts, _deferred, _) =
                    prepared.into_parts();
                let _ = response.send(
                    self.install_prepared_pools(
                        vec![registration],
                        baseline,
                        &storage,
                        accounts.as_ref(),
                        None,
                        AmmPreparedInstallPolicy::new(false, &BTreeSet::new()),
                    )
                    .await,
                );
            }
            AmmRuntimeCommand::FlushPersistentCache { response } => {
                let result = self
                    .cache
                    .flush()
                    .map_err(|error| AmmRuntimeCommandError::ColdStartWorker(error.to_string()));
                let _ = response.send(result);
            }
            AmmRuntimeCommand::RemovePool {
                pool,
                eviction,
                response,
            } => {
                let _ = response.send(self.remove_pool(pool, eviction).await);
            }
        }
    }

    async fn handle_canonical(&mut self, command: AmmCanonicalCommand) {
        let result = self.ingest(*command.batch, command.origin).await;
        let _ = command.response.send(result);
    }

    async fn drain_ready_canonical(&mut self) {
        while let Ok(command) = self.canonical.try_recv() {
            self.handle_canonical(command).await;
        }
    }

    async fn await_subscriber_fence<T, F>(&mut self, future: F) -> Result<T, AmmRuntimeCommandError>
    where
        F: Future<Output = Result<T, super::subscriber_driver::AmmSubscriberDriverError>>,
    {
        tokio::pin!(future);
        loop {
            tokio::select! {
                biased;
                command = self.canonical.recv() => {
                    let Some(command) = command else {
                        return Err(AmmRuntimeCommandError::Closed);
                    };
                    self.handle_canonical(command).await;
                }
                result = &mut future => {
                    return result.map_err(|error| AmmRuntimeCommandError::Subscriber(error.to_string()));
                }
                _ = self.shutdown.changed() => return Err(AmmRuntimeCommandError::Closed),
            }
        }
    }

    async fn attach_subscriber(
        &mut self,
        subscriber: AmmSubscriberControl,
    ) -> Result<(), AmmRuntimeCommandError> {
        self.require_trusted()?;
        if self.subscriber.is_some() {
            return Err(AmmRuntimeCommandError::SubscriberAlreadyAttached);
        }
        let plans = self.engine.active_pool_subscriptions()?;
        let instances: Vec<_> = plans.iter().map(|plan| plan.instance().clone()).collect();
        let mut owner_plans: Vec<AmmSubscriberOwnerPlan> =
            plans.into_iter().map(Into::into).collect();
        owner_plans.extend(self.factory_watchers.values().map(|watcher| {
            AmmSubscriberOwnerPlan::new(
                watcher.handler.clone(),
                watcher
                    .sources
                    .iter()
                    .cloned()
                    .map(super::reactive::event_source_interest)
                    .collect(),
            )
        }));
        self.next_event_sequence(instances.len())?;
        if let Err(error) = subscriber
            .adopt_existing_owners(owner_plans, self.point, self.interest_revision)
            .await
        {
            subscriber.shutdown_for_runtime();
            return Err(AmmRuntimeCommandError::Subscriber(error.to_string()));
        }
        let transitions = match self.engine.acknowledge_live_delivery(&instances) {
            Ok(transitions) => transitions,
            Err(error) => {
                subscriber.shutdown_for_runtime();
                self.mark_untrusted();
                return Err(error.into());
            }
        };
        self.subscriber = Some(subscriber);
        self.publish_live_transitions(transitions)?;
        Ok(())
    }

    async fn add_adapter(
        &mut self,
        adapter: Arc<dyn super::AmmAdapter>,
    ) -> Result<super::AdapterInstanceId, AmmRuntimeCommandError> {
        if self.subscriber.is_none() {
            self.drain_ready_canonical().await;
        }
        self.require_trusted()?;
        let (version, first_sequence) = self.next_commit_identity(2)?;
        let permit = self.reserve_critical().await?;
        let lifecycle = self.engine.add_adapter(adapter)?;
        let instance = lifecycle
            .registered_adapters()
            .first()
            .cloned()
            .ok_or_else(|| {
                AmmRuntimeCommandError::UntrustedBatch(
                    "adapter lifecycle committed without a generation".to_owned(),
                )
            })?;
        let registry_snapshot =
            match AdapterRegistrySnapshot::try_new(self.engine.registry(), self.engine.ownership())
            {
                Ok(snapshot) => Arc::new(snapshot),
                Err(error) => {
                    self.mark_untrusted();
                    return Err(error.into());
                }
            };
        let quality = self.registry_quality();
        let changes = Arc::new(AmmChangeSet::new(
            version,
            self.point,
            quality,
            [],
            [],
            false,
        )?);
        self.publish_commit(
            changes,
            registry_snapshot,
            Arc::clone(&self.revisions),
            permit,
            first_sequence,
            vec![
                AmmRuntimeEventKind::AdapterRegistrationAccepted {
                    adapter: instance.clone(),
                },
                AmmRuntimeEventKind::StateCommitted {
                    version,
                    point: self.point,
                },
            ],
        );
        Ok(instance)
    }

    async fn remove_adapter(
        &mut self,
        adapter: super::AdapterInstanceId,
    ) -> Result<(), AmmRuntimeCommandError> {
        if self.subscriber.is_none() {
            self.drain_ready_canonical().await;
        }
        self.require_trusted()?;
        let active = self
            .engine
            .ownership()
            .active_adapter(adapter.key())
            .cloned();
        if active.as_ref() != Some(&adapter) {
            return Err(AmmRuntimeCommandError::StaleAdapterInstance {
                requested: Box::new(adapter),
                active: active.map(Box::new),
            });
        }
        let protocol = adapter.key().protocols().first().copied().ok_or_else(|| {
            AmmRuntimeCommandError::UntrustedBatch(
                "active adapter generation had an empty family key".to_owned(),
            )
        })?;
        let cancelled_work = self
            .scheduled_work
            .iter()
            .filter(|(_, scheduled)| scheduled.adapter == adapter)
            .map(|(work, _)| work.clone())
            .collect::<Vec<_>>();
        let event_count = 2usize
            .checked_add(cancelled_work.len())
            .ok_or_else(|| RuntimeSequenceOverflow::new("AmmRuntimeEvent.sequence"))?;
        let (version, first_sequence) = self.next_commit_identity(event_count)?;
        let permit = self.reserve_critical().await?;
        let lifecycle = self.engine.remove_adapter(protocol)?;
        if lifecycle.removed_adapters() != std::slice::from_ref(&adapter) {
            self.mark_untrusted();
            return Err(AmmRuntimeCommandError::UntrustedBatch(
                "adapter lifecycle removed a different generation".to_owned(),
            ));
        }
        for work in &cancelled_work {
            self.detach_scheduled_work(work);
        }
        let registry_snapshot =
            match AdapterRegistrySnapshot::try_new(self.engine.registry(), self.engine.ownership())
            {
                Ok(snapshot) => Arc::new(snapshot),
                Err(error) => {
                    self.mark_untrusted();
                    return Err(error.into());
                }
            };
        let quality = self.registry_quality();
        let changes = Arc::new(AmmChangeSet::new(
            version,
            self.point,
            quality,
            [],
            [],
            false,
        )?);
        let mut events = cancelled_work
            .into_iter()
            .map(|work| AmmRuntimeEventKind::WorkCancelled { work })
            .collect::<Vec<_>>();
        events.extend([
            AmmRuntimeEventKind::AdapterRegistrationRemoved {
                adapter: adapter.clone(),
            },
            AmmRuntimeEventKind::StateCommitted {
                version,
                point: self.point,
            },
        ]);
        self.publish_commit(
            changes,
            registry_snapshot,
            Arc::clone(&self.revisions),
            permit,
            first_sequence,
            events,
        );
        Ok(())
    }

    async fn remove_adapter_cascade(
        &mut self,
        adapter: super::AdapterInstanceId,
        eviction: super::AmmEvictionPolicy,
    ) -> Result<Arc<AmmChangeSet>, AmmRuntimeCommandError> {
        if self.subscriber.is_none() {
            self.drain_ready_canonical().await;
        }
        self.require_trusted()?;
        let active = self
            .engine
            .ownership()
            .active_adapter(adapter.key())
            .cloned();
        if active.as_ref() != Some(&adapter) {
            return Err(AmmRuntimeCommandError::StaleAdapterInstance {
                requested: Box::new(adapter),
                active: active.map(Box::new),
            });
        }
        let protocol = adapter.key().protocols().first().copied().ok_or_else(|| {
            AmmRuntimeCommandError::UntrustedBatch(
                "active adapter generation had an empty family key".to_owned(),
            )
        })?;
        let pools = self.engine.ownership().pools_for_adapter(&adapter);
        let discovery_owners = self.engine.ownership().discovery_for_adapter(&adapter);
        let cancelled_work = self
            .scheduled_work
            .iter()
            .filter(|(_, scheduled)| scheduled.adapter == adapter)
            .map(|(work, _)| work.clone())
            .chain(
                self.scheduled_discovery
                    .iter()
                    .filter(|(_, scheduled)| discovery_owners.contains(&scheduled.owner))
                    .map(|(work, _)| work.clone()),
            )
            .chain(
                self.scheduled_followups
                    .iter()
                    .filter(|(_, scheduled)| pools.contains(&scheduled.pool))
                    .map(|(work, _)| work.clone()),
            )
            .collect::<BTreeSet<_>>();
        let maximum_events = pools
            .len()
            .checked_add(discovery_owners.len())
            .and_then(|count| count.checked_add(cancelled_work.len()))
            .and_then(|count| count.checked_add(3))
            .ok_or_else(|| RuntimeSequenceOverflow::new("AmmRuntimeEvent.sequence"))?;
        let _preflight_identity = self.next_commit_identity(maximum_events)?;
        let _preflight_interest_revision = if pools.is_empty() && discovery_owners.is_empty() {
            self.interest_revision
        } else {
            self.interest_revision
                .checked_add(1)
                .ok_or(AmmRuntimeCommandError::InterestRevisionExhausted)?
        };
        for pool in &pools {
            self.revisions
                .get(pool)
                .copied()
                .unwrap_or_else(|| PoolStateRevision::new(0))
                .checked_next()?;
        }
        let subscriber = self.subscriber.clone();
        let subscriber_transaction = match &subscriber {
            Some(subscriber) if !pools.is_empty() || !discovery_owners.is_empty() => {
                let subscriber = subscriber.clone();
                let mut owners: Vec<_> = pools
                    .iter()
                    .map(super::AmmPoolReactiveHandler::handler_id)
                    .collect();
                owners.extend(discovery_owners.iter().filter_map(|owner| {
                    self.factory_watchers
                        .get(owner)
                        .map(|watcher| watcher.handler.clone())
                }));
                Some(
                    self.await_subscriber_fence(
                        async move { subscriber.begin_remove(owners).await },
                    )
                    .await?,
                )
            }
            _ => None,
        };
        if let Err(error) = self.require_trusted() {
            if let (Some(subscriber), Some(transaction)) = (&subscriber, subscriber_transaction) {
                let _ = subscriber.abort(transaction).await;
            }
            return Err(error);
        }
        let post_fence_adapter = self
            .engine
            .ownership()
            .active_adapter(adapter.key())
            .cloned();
        let post_fence_pools = self.engine.ownership().pools_for_adapter(&adapter);
        let post_fence_discovery = self.engine.ownership().discovery_for_adapter(&adapter);
        if post_fence_adapter.as_ref() != Some(&adapter)
            || post_fence_pools != pools
            || post_fence_discovery != discovery_owners
        {
            if let (Some(subscriber), Some(transaction)) = (&subscriber, subscriber_transaction) {
                let _ = subscriber.abort(transaction).await;
            }
            if post_fence_adapter.as_ref() != Some(&adapter) {
                return Err(AmmRuntimeCommandError::StaleAdapterInstance {
                    requested: Box::new(adapter),
                    active: post_fence_adapter.map(Box::new),
                });
            }
            return Err(AmmRuntimeCommandError::CanonicalBacklog);
        }
        let cancelled_work = self
            .scheduled_work
            .iter()
            .filter(|(_, scheduled)| scheduled.adapter == adapter)
            .map(|(work, _)| work.clone())
            .chain(
                self.scheduled_discovery
                    .iter()
                    .filter(|(_, scheduled)| discovery_owners.contains(&scheduled.owner))
                    .map(|(work, _)| work.clone()),
            )
            .chain(
                self.scheduled_followups
                    .iter()
                    .filter(|(_, scheduled)| pools.contains(&scheduled.pool))
                    .map(|(work, _)| work.clone()),
            )
            .collect::<BTreeSet<_>>();
        let maximum_events = pools
            .len()
            .checked_add(discovery_owners.len())
            .and_then(|count| count.checked_add(cancelled_work.len()))
            .and_then(|count| count.checked_add(3))
            .ok_or_else(|| RuntimeSequenceOverflow::new("AmmRuntimeEvent.sequence"))?;
        let removal_revisions = pools
            .iter()
            .map(|pool| {
                Ok((
                    pool.clone(),
                    self.revisions
                        .get(pool)
                        .copied()
                        .unwrap_or_else(|| PoolStateRevision::new(0))
                        .checked_next()?,
                ))
            })
            .collect::<Result<BTreeMap<_, _>, AmmRuntimeCommandError>>();
        let removal_revisions = match removal_revisions {
            Ok(revisions) => revisions,
            Err(error) => {
                if let (Some(subscriber), Some(transaction)) = (&subscriber, subscriber_transaction)
                {
                    let _ = subscriber.abort(transaction).await;
                }
                return Err(error);
            }
        };
        let next_interest_revision = if pools.is_empty() && discovery_owners.is_empty() {
            self.interest_revision
        } else {
            self.interest_revision
                .checked_add(1)
                .ok_or(AmmRuntimeCommandError::InterestRevisionExhausted)?
        };
        let (version, first_sequence) = self.next_commit_identity(maximum_events)?;
        let permit = match self.reserve_critical().await {
            Ok(permit) => permit,
            Err(error) => {
                if let (Some(subscriber), Some(transaction)) = (&subscriber, subscriber_transaction)
                {
                    let _ = subscriber.abort(transaction).await;
                }
                return Err(error);
            }
        };
        if let (Some(subscriber), Some(transaction)) = (&subscriber, subscriber_transaction)
            && let Err(error) = subscriber
                .commit(transaction, next_interest_revision, self.point)
                .await
        {
            return Err(AmmRuntimeCommandError::Subscriber(error.to_string()));
        }
        let mut removed_watchers = Vec::with_capacity(discovery_owners.len());
        for owner in &discovery_owners {
            let Some(watcher) = self.factory_watchers.remove(owner) else {
                self.mark_untrusted();
                return Err(AmmRuntimeCommandError::UntrustedBatch(
                    "adapter cascade could not find its factory watcher".to_owned(),
                ));
            };
            self.factory_watcher_index.remove(owner, &watcher.sources);
            let Some(_ownership) = self.engine.ownership_mut().remove_discovery(owner) else {
                self.mark_untrusted();
                return Err(AmmRuntimeCommandError::UntrustedBatch(
                    "adapter cascade could not detach discovery ownership".to_owned(),
                ));
            };
            removed_watchers.push((owner.clone(), watcher));
        }
        let lifecycle_result = match eviction {
            super::AmmEvictionPolicy::Retain => self.engine.remove_adapter_cascade(protocol),
            super::AmmEvictionPolicy::Exclusive => self
                .engine
                .remove_adapter_cascade_evicting(protocol, &mut self.cache),
        };
        let lifecycle = match lifecycle_result {
            Ok(lifecycle) => lifecycle,
            Err(error) => {
                for (owner, watcher) in removed_watchers {
                    let _ = self
                        .engine
                        .ownership_mut()
                        .insert_discovery(watcher.ownership.clone());
                    self.factory_watcher_index.insert(&owner, &watcher.sources);
                    self.factory_watchers.insert(owner, watcher);
                }
                self.mark_untrusted();
                return Err(error.into());
            }
        };
        if lifecycle.removed_adapters() != std::slice::from_ref(&adapter)
            || lifecycle.removed_pools().len() != pools.len()
            || lifecycle
                .removed_pools()
                .iter()
                .map(|removed| removed.instance())
                .ne(pools.iter())
        {
            self.mark_untrusted();
            return Err(AmmRuntimeCommandError::UntrustedBatch(
                "adapter cascade removed a different ownership set".to_owned(),
            ));
        }
        for work in &cancelled_work {
            self.detach_scheduled_work(work);
        }
        self.pending_followup_intents
            .retain(|pool, _| !pools.contains(pool));
        for pool in &pools {
            self.registration_evidence.remove(pool);
            self.registration_revalidation.remove(pool);
        }
        self.pending_revalidations
            .retain(|(pool, _), _| !pools.contains(pool));
        let registry_snapshot =
            match AdapterRegistrySnapshot::try_new(self.engine.registry(), self.engine.ownership())
            {
                Ok(snapshot) => Arc::new(snapshot),
                Err(error) => {
                    self.mark_untrusted();
                    return Err(error.into());
                }
            };
        let mut revisions = (*self.revisions).clone();
        let pool_changes = pools
            .iter()
            .cloned()
            .map(|pool| {
                revisions.remove(&pool);
                let revision = removal_revisions[&pool];
                Ok(AmmPoolChange::new(
                    pool,
                    revision,
                    AmmPoolChangeKind::Removed,
                    super::AmmChangeImpact::all(),
                ))
            })
            .collect::<Result<Vec<_>, AmmRuntimeCommandError>>()?;
        let quality = self.registry_quality();
        let changes = Arc::new(AmmChangeSet::new(
            version,
            self.point,
            quality,
            pool_changes,
            [],
            false,
        )?);
        self.interest_revision = next_interest_revision;
        self.interest_revisions.send_replace(next_interest_revision);
        for owner in &discovery_owners {
            self.discovery_lifecycles
                .insert(owner.clone(), super::OwnerRuntimeState::Removed);
        }
        self.pending_factory_candidates
            .retain(|candidate| !discovery_owners.contains(&candidate.owner));
        let mut observer_events = cancelled_work
            .into_iter()
            .map(|work| AmmRuntimeEventKind::WorkCancelled { work })
            .collect::<Vec<_>>();
        observer_events.extend(
            pools
                .iter()
                .cloned()
                .map(|pool| AmmRuntimeEventKind::RegistrationRemoved { pool }),
        );
        observer_events.extend(
            discovery_owners
                .iter()
                .cloned()
                .map(|owner| AmmRuntimeEventKind::DiscoveryRegistrationRemoved { owner }),
        );
        observer_events.push(AmmRuntimeEventKind::AdapterRegistrationRemoved {
            adapter: adapter.clone(),
        });
        if runtime_health(quality) != self.health {
            observer_events.push(AmmRuntimeEventKind::HealthChanged {
                from: self.health,
                to: runtime_health(quality),
            });
        }
        observer_events.push(AmmRuntimeEventKind::StateCommitted {
            version,
            point: self.point,
        });
        Ok(self.publish_commit(
            changes,
            registry_snapshot,
            Arc::new(revisions),
            permit,
            first_sequence,
            observer_events,
        ))
    }

    async fn add_factory_watcher(
        &mut self,
        registration: AmmFactoryWatcherRegistration,
    ) -> Result<DiscoveryOwnerId, AmmRuntimeCommandError> {
        if self.subscriber.is_none() {
            self.drain_ready_canonical().await;
        }
        self.require_trusted()?;
        if let Some(active) = self.engine.ownership().active_discovery(registration.key()) {
            return Err(AmmRuntimeCommandError::DiscoveryAlreadyRegistered(
                active.clone(),
            ));
        }
        if self
            .engine
            .ownership()
            .active_adapter(registration.adapter().key())
            != Some(registration.adapter())
        {
            return Err(AmmRuntimeCommandError::StaleAdapterInstance {
                requested: Box::new(registration.adapter().clone()),
                active: self
                    .engine
                    .ownership()
                    .active_adapter(registration.adapter().key())
                    .cloned()
                    .map(Box::new),
            });
        }
        let sources = registration.discovery().creation_sources();
        if sources.is_empty() {
            return Err(AmmRuntimeCommandError::DiscoveryHasNoCreationSources(
                registration.key().clone(),
            ));
        }
        let generation = self
            .discovery_generations
            .get(registration.key())
            .copied()
            .map_or(
                Ok(DiscoveryGeneration::new(0)),
                DiscoveryGeneration::checked_next,
            )?;
        let owner = DiscoveryOwnerId::new(registration.key().clone(), generation);
        let handler = HandlerId::new(format!(
            "evm-amm-state.discovery.{}.{}",
            owner.key().as_str(),
            owner.generation().get()
        ));
        let plan = AmmSubscriberOwnerPlan::new(
            handler.clone(),
            sources
                .iter()
                .cloned()
                .map(super::reactive::event_source_interest)
                .collect(),
        );
        let _preflight_interest_revision = self
            .interest_revision
            .checked_add(1)
            .ok_or(AmmRuntimeCommandError::InterestRevisionExhausted)?;
        let _preflight_identity = self.next_commit_identity(2)?;
        let subscriber = self.subscriber.clone();
        let subscriber_transaction = match &subscriber {
            Some(subscriber) => {
                let subscriber = subscriber.clone();
                let point = self.point;
                Some(
                    self.await_subscriber_fence(async move {
                        subscriber.begin_add_owners(vec![plan], point).await
                    })
                    .await?,
                )
            }
            None => None,
        };
        let next_interest_revision = self
            .interest_revision
            .checked_add(1)
            .ok_or(AmmRuntimeCommandError::InterestRevisionExhausted)?;
        let (version, first_sequence) = self.next_commit_identity(2)?;
        let permit = match self.reserve_critical().await {
            Ok(permit) => permit,
            Err(error) => {
                if let (Some(subscriber), Some(transaction)) = (&subscriber, subscriber_transaction)
                {
                    let _ = subscriber.abort(transaction).await;
                }
                return Err(error);
            }
        };
        let ownership = DiscoveryOwnership::new(owner.clone(), registration.adapter().clone());
        if let Err(error) = self
            .engine
            .ownership_mut()
            .insert_discovery(ownership.clone())
        {
            if let (Some(subscriber), Some(transaction)) = (&subscriber, subscriber_transaction) {
                let _ = subscriber.abort(transaction).await;
            }
            return Err(AmmSyncError::Ownership(error).into());
        }
        self.factory_watchers.insert(
            owner.clone(),
            AmmActiveFactoryWatcher {
                ownership,
                discovery: Arc::clone(registration.discovery()),
                handler,
                sources: sources.clone(),
            },
        );
        self.factory_watcher_index.insert(&owner, &sources);
        if let (Some(subscriber), Some(transaction)) = (&subscriber, subscriber_transaction)
            && let Err(error) = subscriber
                .commit(transaction, next_interest_revision, self.point)
                .await
        {
            if let Some(watcher) = self.factory_watchers.remove(&owner) {
                self.factory_watcher_index.remove(&owner, &watcher.sources);
            }
            self.engine.ownership_mut().remove_discovery(&owner);
            self.mark_untrusted();
            return Err(AmmRuntimeCommandError::Subscriber(error.to_string()));
        }
        self.discovery_generations
            .insert(owner.key().clone(), owner.generation());
        self.discovery_lifecycles
            .insert(owner.clone(), super::OwnerRuntimeState::Active);
        self.interest_revision = next_interest_revision;
        self.interest_revisions.send_replace(next_interest_revision);
        let changes = Arc::new(AmmChangeSet::new(
            version,
            self.point,
            self.registry_quality(),
            [],
            [],
            false,
        )?);
        self.publish_commit(
            changes,
            Arc::clone(&self.registry_snapshot),
            Arc::clone(&self.revisions),
            permit,
            first_sequence,
            vec![
                AmmRuntimeEventKind::DiscoveryRegistrationAccepted {
                    owner: owner.clone(),
                },
                AmmRuntimeEventKind::StateCommitted {
                    version,
                    point: self.point,
                },
            ],
        );
        Ok(owner)
    }

    async fn remove_factory_watcher(
        &mut self,
        owner: DiscoveryOwnerId,
    ) -> Result<(), AmmRuntimeCommandError> {
        if self.subscriber.is_none() {
            self.drain_ready_canonical().await;
        }
        self.require_trusted()?;
        let active = self
            .engine
            .ownership()
            .active_discovery(owner.key())
            .cloned();
        if active.as_ref() != Some(&owner) {
            return Err(AmmRuntimeCommandError::StaleDiscoveryOwner {
                requested: Box::new(owner.clone()),
                active: active.map(Box::new),
            });
        }
        let orphaned_pools = self
            .registration_evidence
            .iter()
            .filter_map(|(pool, evidence)| {
                evidence_contains_owner(evidence, &owner)
                    .then(|| {
                        evidence_without_owner(evidence, &owner)
                            .is_none()
                            .then(|| pool.clone())
                    })
                    .flatten()
            })
            .collect::<Vec<_>>();
        if !orphaned_pools.is_empty() {
            return Err(AmmRuntimeCommandError::DiscoveryOwnerInUse {
                owner: Box::new(owner),
                pools: orphaned_pools.into_boxed_slice(),
            });
        }
        let watcher = self.factory_watchers.get(&owner).ok_or_else(|| {
            AmmRuntimeCommandError::UntrustedBatch(
                "active discovery ownership had no factory watcher".to_owned(),
            )
        })?;
        let cancelled_work = self
            .scheduled_discovery
            .iter()
            .filter(|(_, scheduled)| scheduled.owner == owner)
            .map(|(work, _)| work.clone())
            .chain(
                self.scheduled_work
                    .iter()
                    .filter(|(_, scheduled)| {
                        scheduled.supporting_discovery.len() == 1
                            && scheduled.supporting_discovery.contains(&owner)
                    })
                    .map(|(work, _)| work.clone()),
            )
            .collect::<Vec<_>>();
        let handler = watcher.handler.clone();
        let _preflight_interest_revision = self
            .interest_revision
            .checked_add(1)
            .ok_or(AmmRuntimeCommandError::InterestRevisionExhausted)?;
        let event_count = 2usize
            .checked_add(cancelled_work.len())
            .ok_or_else(|| RuntimeSequenceOverflow::new("AmmRuntimeEvent.sequence"))?;
        let _preflight_identity = self.next_commit_identity(event_count)?;
        let subscriber = self.subscriber.clone();
        let subscriber_transaction = match &subscriber {
            Some(subscriber) => {
                let subscriber = subscriber.clone();
                Some(
                    self.await_subscriber_fence(async move {
                        subscriber.begin_remove(vec![handler]).await
                    })
                    .await?,
                )
            }
            None => None,
        };
        let post_fence_active = self
            .engine
            .ownership()
            .active_discovery(owner.key())
            .cloned();
        if let Err(error) = self.require_trusted() {
            if let (Some(subscriber), Some(transaction)) = (&subscriber, subscriber_transaction) {
                let _ = subscriber.abort(transaction).await;
            }
            return Err(error);
        }
        if post_fence_active.as_ref() != Some(&owner) {
            if let (Some(subscriber), Some(transaction)) = (&subscriber, subscriber_transaction) {
                let _ = subscriber.abort(transaction).await;
            }
            return Err(AmmRuntimeCommandError::StaleDiscoveryOwner {
                requested: Box::new(owner.clone()),
                active: post_fence_active.map(Box::new),
            });
        }
        let orphaned_pools = self
            .registration_evidence
            .iter()
            .filter_map(|(pool, evidence)| {
                evidence_contains_owner(evidence, &owner)
                    .then(|| {
                        evidence_without_owner(evidence, &owner)
                            .is_none()
                            .then(|| pool.clone())
                    })
                    .flatten()
            })
            .collect::<Vec<_>>();
        if !orphaned_pools.is_empty() {
            if let (Some(subscriber), Some(transaction)) = (&subscriber, subscriber_transaction) {
                let _ = subscriber.abort(transaction).await;
            }
            return Err(AmmRuntimeCommandError::DiscoveryOwnerInUse {
                owner: Box::new(owner.clone()),
                pools: orphaned_pools.into_boxed_slice(),
            });
        }
        let cancelled_work = self
            .scheduled_discovery
            .iter()
            .filter(|(_, scheduled)| scheduled.owner == owner)
            .map(|(work, _)| work.clone())
            .chain(
                self.scheduled_work
                    .iter()
                    .filter(|(_, scheduled)| {
                        scheduled.supporting_discovery.len() == 1
                            && scheduled.supporting_discovery.contains(&owner)
                    })
                    .map(|(work, _)| work.clone()),
            )
            .collect::<Vec<_>>();
        let event_count = 2usize
            .checked_add(cancelled_work.len())
            .ok_or_else(|| RuntimeSequenceOverflow::new("AmmRuntimeEvent.sequence"))?;
        let next_interest_revision = self
            .interest_revision
            .checked_add(1)
            .ok_or(AmmRuntimeCommandError::InterestRevisionExhausted)?;
        let (version, first_sequence) = self.next_commit_identity(event_count)?;
        let permit = match self.reserve_critical().await {
            Ok(permit) => permit,
            Err(error) => {
                if let (Some(subscriber), Some(transaction)) = (&subscriber, subscriber_transaction)
                {
                    let _ = subscriber.abort(transaction).await;
                }
                return Err(error);
            }
        };
        if let (Some(subscriber), Some(transaction)) = (&subscriber, subscriber_transaction)
            && let Err(error) = subscriber
                .commit(transaction, next_interest_revision, self.point)
                .await
        {
            return Err(AmmRuntimeCommandError::Subscriber(error.to_string()));
        }
        let removed = self.engine.ownership_mut().remove_discovery(&owner);
        let removed_watcher = self.factory_watchers.remove(&owner);
        if removed.is_none() || removed_watcher.is_none() {
            self.mark_untrusted();
            return Err(AmmRuntimeCommandError::UntrustedBatch(
                "factory watcher disappeared during serialized removal".to_owned(),
            ));
        }
        self.factory_watcher_index.remove(
            &owner,
            &removed_watcher.expect("checked as present").sources,
        );
        self.pending_factory_candidates
            .retain(|candidate| candidate.owner != owner);
        for work in &cancelled_work {
            self.detach_scheduled_work(work);
        }
        for scheduled in self.scheduled_work.values_mut() {
            if !scheduled.supporting_discovery.remove(&owner) {
                continue;
            }
            let mut evidence = scheduled
                .evidence
                .evidence()
                .iter()
                .filter(|evidence| match evidence {
                    RegistrationProvenance::StateQuery { owner: source, .. }
                    | RegistrationProvenance::FactoryLog { owner: source, .. } => source != &owner,
                    RegistrationProvenance::Stable { .. } => true,
                })
                .cloned()
                .collect::<Vec<_>>();
            if !evidence.is_empty() {
                let primary = evidence.remove(0);
                scheduled.evidence = RegistrationEvidenceSet::new(primary, evidence);
            }
            if scheduled.discovery_owner.as_ref() == Some(&owner) {
                scheduled.discovery_owner = scheduled.supporting_discovery.iter().next().cloned();
            }
            scheduled.revalidations.remove(&owner);
        }
        self.pending_revalidations
            .retain(|(_, source), _| source != &owner);
        self.registration_revalidation.retain(|_, requests| {
            requests.remove(&owner);
            !requests.is_empty()
        });
        self.registration_evidence.retain(|_, evidence| {
            if let Some(retained) = evidence_without_owner(evidence, &owner) {
                *evidence = retained;
            }
            true
        });
        self.discovery_lifecycles
            .insert(owner.clone(), super::OwnerRuntimeState::Removed);
        self.interest_revision = next_interest_revision;
        self.interest_revisions.send_replace(next_interest_revision);
        let changes = Arc::new(AmmChangeSet::new(
            version,
            self.point,
            self.registry_quality(),
            [],
            [],
            false,
        )?);
        let mut events = cancelled_work
            .into_iter()
            .map(|work| AmmRuntimeEventKind::WorkCancelled { work })
            .collect::<Vec<_>>();
        events.extend([
            AmmRuntimeEventKind::DiscoveryRegistrationRemoved {
                owner: owner.clone(),
            },
            AmmRuntimeEventKind::StateCommitted {
                version,
                point: self.point,
            },
        ]);
        self.publish_commit(
            changes,
            Arc::clone(&self.registry_snapshot),
            Arc::clone(&self.revisions),
            permit,
            first_sequence,
            events,
        );
        Ok(())
    }

    fn subscribe_changes(&mut self) -> Result<AmmChangeSubscription, AmmRuntimeCommandError> {
        if self
            .critical
            .as_ref()
            .is_some_and(|sender| !sender.is_closed())
        {
            return Err(AmmRuntimeCommandError::CriticalSubscriberExists);
        }
        let (sender, commits) = mpsc::channel(self.critical_capacity);
        self.critical = Some(sender);
        Ok(AmmChangeSubscription {
            snapshot: self.snapshots.borrow().clone(),
            commits,
        })
    }

    fn attach_cold_start_worker(
        &mut self,
        control: AmmColdStartWorkerControl,
    ) -> Result<(), AmmRuntimeCommandError> {
        if self.cold_start_worker.is_some() {
            return Err(AmmRuntimeCommandError::ColdStartWorkerAlreadyAttached);
        }
        self.cold_start_worker = Some(control);
        self.drain_factory_candidates();
        self.drain_followup_intents();
        self.drain_revalidations();
        Ok(())
    }

    fn retain_factory_candidate(&mut self, candidate: AmmFactoryCandidate) {
        const MAX_PENDING_FACTORY_CANDIDATES: usize = 4_096;
        if let Some(pool) = self
            .engine
            .ownership()
            .active_pool(&candidate.registration.key)
            .cloned()
        {
            let mut evidence = self
                .registration_evidence
                .get(&pool)
                .map(|evidence| evidence.evidence().to_vec())
                .unwrap_or_default();
            evidence.extend(candidate.evidence.evidence().iter().cloned());
            let primary = evidence.remove(0);
            self.registration_evidence.insert(
                pool.clone(),
                RegistrationEvidenceSet::new(primary, evidence),
            );
            if let Some(request) = candidate.revalidate {
                self.registration_revalidation
                    .entry(pool)
                    .or_default()
                    .insert(candidate.owner, request);
            }
            return;
        }
        if let Some(pending) = self.pending_factory_candidates.iter_mut().find(|pending| {
            pending.owner == candidate.owner
                && pending.registration.key == candidate.registration.key
        }) {
            let mut evidence = pending.evidence.evidence().to_vec();
            evidence.extend(candidate.evidence.evidence().iter().cloned());
            let primary = evidence.remove(0);
            pending.evidence = RegistrationEvidenceSet::new(primary, evidence);
            if pending.revalidate.is_none() {
                pending.revalidate = candidate.revalidate;
            }
            return;
        }
        if self.pending_factory_candidates.len() < MAX_PENDING_FACTORY_CANDIDATES {
            self.pending_factory_candidates.push_back(candidate);
        } else {
            self.mark_untrusted();
        }
    }

    fn drain_factory_candidates(&mut self) {
        let attempts = self.pending_factory_candidates.len();
        for _ in 0..attempts {
            let Some(candidate) = self.pending_factory_candidates.pop_front() else {
                break;
            };
            if let Some(work) = self
                .pending_pool_work
                .get(&candidate.registration.key)
                .cloned()
                && let Some(pending) = self.scheduled_work.get_mut(&work)
            {
                let mut evidence = pending.evidence.evidence().to_vec();
                evidence.extend(candidate.evidence.evidence().iter().cloned());
                let primary = evidence.remove(0);
                pending.evidence = RegistrationEvidenceSet::new(primary, evidence);
                pending.supporting_discovery.insert(candidate.owner.clone());
                if pending.discovery_owner.is_none() {
                    pending.discovery_owner = Some(candidate.owner.clone());
                }
                if let Some(request) = candidate.revalidate {
                    pending.revalidations.insert(candidate.owner, request);
                }
                continue;
            }
            if self.cold_start_worker.is_none() {
                self.pending_factory_candidates.push_front(candidate);
                break;
            }
            if self
                .engine
                .ownership()
                .active_discovery(candidate.owner.key())
                != Some(&candidate.owner)
                || self
                    .engine
                    .registry()
                    .pool(&candidate.registration.key)
                    .is_some()
            {
                continue;
            }
            let registration = candidate.registration.clone();
            match self.queue_cold_start(
                vec![registration],
                AmmColdStartOptions::default().with_class(AmmWorkClass::Focused),
            ) {
                Ok(scheduled) => {
                    if let Some(work) = scheduled.first().map(AmmScheduledPool::work)
                        && let Some(pending) = self.scheduled_work.get_mut(work)
                    {
                        pending.discovery_owner = Some(candidate.owner.clone());
                        pending.supporting_discovery.insert(candidate.owner.clone());
                        pending.evidence = candidate.evidence;
                        if let Some(request) = candidate.revalidate {
                            pending.revalidations.insert(candidate.owner, request);
                        }
                    }
                }
                Err(AmmRuntimeCommandError::ColdStartWorkerUnavailable)
                | Err(AmmRuntimeCommandError::ColdStartWorker(_)) => {
                    self.pending_factory_candidates.push_front(candidate);
                    break;
                }
                Err(_) => {}
            }
        }
    }

    fn reconcile_orphaned_pending_registrations(
        &mut self,
        dropped_hashes: &[alloy_primitives::B256],
    ) -> Result<(), AmmRuntimeCommandError> {
        if dropped_hashes.is_empty() {
            return Ok(());
        }
        // Unpublished candidates have no canonical pool generation to keep
        // available while a query is revalidated. Only independently supported
        // (`Keep`) evidence may cross the reorg fence; `Remove` and `Revalidate`
        // candidates/work are discarded or cancelled before they can publish.
        self.pending_factory_candidates.retain_mut(|candidate| {
            if candidate.evidence.reorg_action(dropped_hashes) != RegistrationReorgAction::Keep {
                return false;
            }
            let Some(retained) =
                evidence_without_dropped_hashes(&candidate.evidence, dropped_hashes)
            else {
                return false;
            };
            candidate.evidence = retained;
            if !evidence_contains_state_query_owner(&candidate.evidence, &candidate.owner) {
                candidate.revalidate = None;
            }
            true
        });

        let mut orphaned_work = Vec::new();
        for (work, scheduled) in &mut self.scheduled_work {
            if scheduled.evidence.reorg_action(dropped_hashes) != RegistrationReorgAction::Keep {
                orphaned_work.push(work.clone());
                continue;
            }
            let Some(retained) =
                evidence_without_dropped_hashes(&scheduled.evidence, dropped_hashes)
            else {
                orphaned_work.push(work.clone());
                continue;
            };
            scheduled.evidence = retained;
            scheduled
                .supporting_discovery
                .retain(|owner| evidence_contains_owner(&scheduled.evidence, owner));
            scheduled
                .revalidations
                .retain(|owner, _| evidence_contains_state_query_owner(&scheduled.evidence, owner));
            if scheduled
                .discovery_owner
                .as_ref()
                .is_some_and(|owner| !scheduled.supporting_discovery.contains(owner))
            {
                scheduled.discovery_owner = scheduled.supporting_discovery.iter().next().cloned();
            }
        }
        for work in orphaned_work {
            if let Some(worker) = &self.cold_start_worker {
                let _ = worker.cancel(work.clone());
            }
            self.cancel_scheduled_work(&work)?;
        }

        for (pool, evidence) in &mut self.registration_evidence {
            if evidence.reorg_action(dropped_hashes) != RegistrationReorgAction::Keep {
                continue;
            }
            let Some(retained) = evidence_without_dropped_hashes(evidence, dropped_hashes) else {
                continue;
            };
            *evidence = retained;
            if let Some(revalidations) = self.registration_revalidation.get_mut(pool) {
                revalidations
                    .retain(|owner, _| evidence_contains_state_query_owner(evidence, owner));
                if revalidations.is_empty() {
                    self.registration_revalidation.remove(pool);
                }
            }
        }
        self.pending_revalidations.retain(|(pool, owner), _| {
            self.registration_revalidation
                .get(pool)
                .is_some_and(|revalidations| revalidations.contains_key(owner))
        });
        Ok(())
    }

    fn queue_cold_start(
        &mut self,
        pools: Vec<PoolRegistration>,
        options: AmmColdStartOptions,
    ) -> Result<Vec<AmmScheduledPool>, AmmRuntimeCommandError> {
        self.require_trusted()?;
        let worker = self
            .cold_start_worker
            .clone()
            .ok_or(AmmRuntimeCommandError::ColdStartWorkerUnavailable)?;
        if pools.is_empty() {
            return Ok(Vec::new());
        }
        let queue_events = pools
            .len()
            .checked_mul(2)
            .ok_or_else(|| RuntimeSequenceOverflow::new("AmmRuntimeEvent.sequence"))?;
        self.next_event_sequence(queue_events)?;

        let mut keys = BTreeSet::new();
        for pool in &pools {
            if !keys.insert(pool.key.clone())
                || self.pending_pool_work.contains_key(&pool.key)
                || self.engine.registry().pool(&pool.key).is_some()
            {
                return Err(AmmRuntimeCommandError::PoolAlreadyScheduled(
                    pool.key.clone(),
                ));
            }
        }

        let mut next_work_id = self.next_work_id;
        let mut accepted = Vec::with_capacity(pools.len());
        for registration in pools {
            let adapter_impl = self
                .engine
                .registry()
                .adapter(registration.protocol())
                .ok_or({
                    AmmRuntimeCommandError::Sync(AmmSyncError::LifecycleInvariant(
                        "cold-start registration has no active adapter",
                    ))
                })?;
            let adapter_key =
                super::AdapterKey::new(adapter_impl.protocol(), adapter_impl.protocols());
            let adapter = self
                .engine
                .ownership()
                .active_adapter(&adapter_key)
                .cloned()
                .ok_or({
                    AmmRuntimeCommandError::Sync(AmmSyncError::LifecycleInvariant(
                        "cold-start registration has no active adapter generation",
                    ))
                })?;
            let reservation = match self
                .engine
                .reserve_pool_generation(registration.key.clone())
            {
                Ok(reservation) => reservation,
                Err(error) => {
                    for (reservation, _, _, _, _) in &accepted {
                        self.engine.cancel_pool_reservation(reservation);
                    }
                    return Err(error.into());
                }
            };
            let work = RuntimeWorkId::new(
                RuntimeOwnerId::Pool(reservation.instance().clone()),
                next_work_id,
            );
            next_work_id = match next_work_id.checked_next() {
                Ok(next) => next,
                Err(error) => {
                    self.engine.cancel_pool_reservation(&reservation);
                    for (reservation, _, _, _, _) in &accepted {
                        self.engine.cancel_pool_reservation(reservation);
                    }
                    return Err(error.into());
                }
            };
            accepted.push((reservation, registration, adapter, work, options.class()));
        }

        let jobs = accepted
            .iter()
            .map(|(reservation, registration, _, work, _)| AmmColdStartJob {
                work: work.clone(),
                pool: reservation.instance().clone(),
                registration: registration.clone(),
                baseline: self.point,
                registry: Arc::clone(&self.registry_snapshot),
                cache: self.cache.snapshot(),
                policy: options.policy(),
                class: options.class(),
                target: AmmColdStartTarget::PendingRegistration,
            })
            .collect();
        if let Err(error) = worker.submit(jobs) {
            for (reservation, _, _, _, _) in &accepted {
                self.engine.cancel_pool_reservation(reservation);
            }
            return Err(AmmRuntimeCommandError::ColdStartWorker(error.to_string()));
        }

        self.next_work_id = next_work_id;
        let progress = AmmWorkProgress::new(AmmWorkKind::ColdStart, 0, None)
            .expect("zero with an unknown total is valid progress");
        let mut scheduled = Vec::with_capacity(accepted.len());
        let mut events = Vec::with_capacity(accepted.len() * 2);
        for (reservation, registration, adapter, work, class) in accepted {
            let pool = reservation.instance().clone();
            self.pending_pool_work
                .insert(registration.key.clone(), work.clone());
            self.pending_lifecycles
                .insert(pool.clone(), PoolRuntimeState::Queued);
            self.active_work.insert(work.clone(), progress.clone());
            self.scheduled_work.insert(
                work.clone(),
                AmmScheduledPoolWork {
                    reservation,
                    registration,
                    adapter,
                    discovery_owner: None,
                    supporting_discovery: BTreeSet::new(),
                    evidence: RegistrationEvidenceSet::new(
                        RegistrationProvenance::stable(RegistrationSourceKey::new("runtime.queue")),
                        [],
                    ),
                    revalidations: BTreeMap::new(),
                    class,
                    queued: true,
                },
            );
            self.adjust_queue_depth(class, 1);
            events.push(AmmRuntimeEventKind::PoolLifecycleTransition {
                pool: pool.clone(),
                from: PoolRuntimeState::Discovered,
                to: PoolRuntimeState::Queued,
            });
            events.push(AmmRuntimeEventKind::WorkQueued {
                work: work.clone(),
                class,
                kind: AmmWorkKind::ColdStart,
            });
            scheduled.push(AmmScheduledPool::new(pool, work));
        }
        self.publish_runtime_events(events)?;
        Ok(scheduled)
    }

    fn queue_discovery(
        &mut self,
        owner: DiscoveryOwnerId,
        request: TokenEdgeDiscoveryRequest,
        options: AmmDiscoveryOptions,
    ) -> Result<AmmScheduledDiscovery, AmmRuntimeCommandError> {
        self.require_trusted()?;
        let worker = self
            .cold_start_worker
            .clone()
            .ok_or(AmmRuntimeCommandError::ColdStartWorkerUnavailable)?;
        if self.engine.ownership().active_discovery(owner.key()) != Some(&owner) {
            return Err(AmmRuntimeCommandError::StaleDiscoveryOwner {
                requested: Box::new(owner.clone()),
                active: self
                    .engine
                    .ownership()
                    .active_discovery(owner.key())
                    .cloned()
                    .map(Box::new),
            });
        }
        let watcher = self.factory_watchers.get(&owner).ok_or_else(|| {
            AmmRuntimeCommandError::UntrustedBatch(
                "active discovery ownership had no factory watcher".to_owned(),
            )
        })?;
        if request.protocol().is_some_and(|protocol| {
            !watcher
                .ownership
                .adapter()
                .key()
                .protocols()
                .contains(&protocol)
        }) {
            return Err(AmmRuntimeCommandError::ColdStartWorker(
                "connector discovery protocol is outside its adapter family".to_owned(),
            ));
        }
        let prepared = watcher
            .discovery
            .prepare_reads([request.query()])
            .map_err(|error| AmmRuntimeCommandError::ColdStartWorker(error.to_string()))?;
        self.next_event_sequence(1)?;
        let work = RuntimeWorkId::new(RuntimeOwnerId::Discovery(owner.clone()), self.next_work_id);
        let next_work_id = self.next_work_id.checked_next()?;
        self.engine
            .ownership_mut()
            .track_work(work.clone())
            .map_err(AmmSyncError::Ownership)?;
        let job = AmmDiscoveryJob {
            work: work.clone(),
            owner: owner.clone(),
            request: request.clone(),
            prepared,
            discovery: Arc::clone(&watcher.discovery),
            baseline: self.point,
            class: options.class(),
        };
        if let Err(error) = worker.submit_discovery(job) {
            self.engine.ownership_mut().untrack_work(&work);
            return Err(AmmRuntimeCommandError::ColdStartWorker(error.to_string()));
        }
        self.next_work_id = next_work_id;
        self.active_work.insert(
            work.clone(),
            AmmWorkProgress::new(AmmWorkKind::Discovery, 0, Some(1))
                .expect("zero of one is valid discovery progress"),
        );
        self.scheduled_discovery.insert(
            work.clone(),
            AmmScheduledDiscoveryWork {
                owner: owner.clone(),
                request,
                baseline: self.point,
                class: options.class(),
                max_candidates: options.max_candidates(),
                queued: true,
                revalidate_pool: None,
            },
        );
        self.adjust_queue_depth(options.class(), 1);
        self.publish_runtime_events(vec![AmmRuntimeEventKind::WorkQueued {
            work: work.clone(),
            class: options.class(),
            kind: AmmWorkKind::Discovery,
        }])?;
        Ok(AmmScheduledDiscovery::new(owner, work))
    }

    fn queue_repair(
        &mut self,
        pool: super::PoolInstanceId,
        action: RepairAction,
    ) -> Result<AmmScheduledFollowUp, AmmRuntimeCommandError> {
        let task = match action {
            RepairAction::ColdStart {
                pool: requested,
                policy,
            } if requested == *pool.key() => AmmFollowUpTask::Refresh {
                policy,
                after_slots: Vec::new(),
            },
            RepairAction::VerifySlots(mut slots) => {
                slots.sort_unstable();
                slots.dedup();
                if slots.is_empty() {
                    return Err(AmmRuntimeCommandError::UnsupportedFollowUp(
                        "repair slot set is empty".to_owned(),
                    ));
                }
                AmmFollowUpTask::SlotPatch { slots }
            }
            RepairAction::ColdStart { .. } => {
                return Err(AmmRuntimeCommandError::UnsupportedFollowUp(
                    "repair pool key does not match its generation".to_owned(),
                ));
            }
            other => {
                return Err(AmmRuntimeCommandError::UnsupportedFollowUp(format!(
                    "required action {other:?} is not a background repair target"
                )));
            }
        };
        self.queue_followup(pool, AmmWorkClass::Repair, AmmWorkKind::Repair, task)
    }

    fn queue_deferred(
        &mut self,
        pool: super::PoolInstanceId,
        deferred: Vec<DeferredWork>,
    ) -> Result<Option<AmmScheduledFollowUp>, AmmRuntimeCommandError> {
        if deferred.is_empty() {
            return Ok(None);
        }
        let mut slots = Vec::new();
        let mut refresh = None;
        for work in deferred {
            match work {
                DeferredWork::VerifySlots(found)
                | DeferredWork::Repair(RepairAction::VerifySlots(found)) => slots.extend(found),
                DeferredWork::ColdStart {
                    pool: requested,
                    policy,
                }
                | DeferredWork::Repair(RepairAction::ColdStart {
                    pool: requested,
                    policy,
                }) if requested == *pool.key() => {
                    if refresh.is_some_and(|existing| existing != policy) {
                        return Err(AmmRuntimeCommandError::UnsupportedFollowUp(
                            "deferred cold-start policies conflict".to_owned(),
                        ));
                    }
                    refresh = Some(policy);
                }
                DeferredWork::ColdStart { .. }
                | DeferredWork::Repair(RepairAction::ColdStart { .. }) => {
                    return Err(AmmRuntimeCommandError::UnsupportedFollowUp(
                        "deferred pool key does not match its generation".to_owned(),
                    ));
                }
                other => {
                    return Err(AmmRuntimeCommandError::UnsupportedFollowUp(format!(
                        "deferred item {other:?} is not schedulable"
                    )));
                }
            }
        }
        slots.sort_unstable();
        slots.dedup();
        let task = if let Some(policy) = refresh {
            AmmFollowUpTask::Refresh {
                policy,
                after_slots: slots,
            }
        } else {
            if slots.is_empty() {
                return Ok(None);
            }
            AmmFollowUpTask::SlotPatch { slots }
        };
        self.queue_followup(
            pool,
            AmmWorkClass::Deferred,
            AmmWorkKind::DeferredWarmup,
            task,
        )
        .map(Some)
    }

    fn retain_followup_intent(
        &mut self,
        pool: super::PoolInstanceId,
        intent: AmmPendingFollowUpIntent,
    ) {
        match (&intent, self.pending_followup_intents.get_mut(&pool)) {
            (AmmPendingFollowUpIntent::Deferred(_), Some(AmmPendingFollowUpIntent::Repair(_))) => {}
            (
                AmmPendingFollowUpIntent::Repair(incoming),
                Some(AmmPendingFollowUpIntent::Repair(existing)),
            ) => {
                *existing = std::mem::take(existing).combine(incoming.clone());
            }
            _ => {
                self.pending_followup_intents.insert(pool, intent);
            }
        }
    }

    fn drain_followup_intents(&mut self) {
        if self.cold_start_worker.is_none() {
            return;
        }
        let pending = self
            .pending_followup_intents
            .iter()
            .map(|(pool, intent)| (pool.clone(), intent.clone()))
            .collect::<Vec<_>>();
        for (pool, intent) in pending {
            if self.engine.ownership().active_pool(pool.key()) != Some(&pool) {
                self.pending_followup_intents.remove(&pool);
                continue;
            }
            let result = match intent {
                AmmPendingFollowUpIntent::Repair(action) => {
                    self.queue_repair(pool.clone(), action).map(Some)
                }
                AmmPendingFollowUpIntent::Deferred(deferred) => {
                    self.queue_deferred(pool.clone(), deferred)
                }
            };
            match result {
                Ok(_) => {
                    self.pending_followup_intents.remove(&pool);
                }
                Err(AmmRuntimeCommandError::PoolAlreadyScheduled(_)) => {}
                Err(AmmRuntimeCommandError::StalePoolInstance { .. })
                | Err(AmmRuntimeCommandError::UnsupportedFollowUp(_)) => {
                    self.pending_followup_intents.remove(&pool);
                }
                Err(_) => {}
            }
        }
    }

    fn drain_revalidations(&mut self) {
        if self.cold_start_worker.is_none() {
            return;
        }
        let pending = self
            .pending_revalidations
            .iter()
            .map(|((pool, owner), request)| ((pool.clone(), owner.clone()), request.clone()))
            .collect::<Vec<_>>();
        for ((pool, owner), request) in pending {
            if self.engine.ownership().active_pool(pool.key()) != Some(&pool) {
                self.pending_revalidations
                    .remove(&(pool.clone(), owner.clone()));
                continue;
            }
            if self.engine.ownership().active_discovery(owner.key()) != Some(&owner) {
                self.mark_untrusted();
                break;
            }
            match self.queue_discovery(
                owner.clone(),
                request,
                AmmDiscoveryOptions::default().with_class(AmmWorkClass::Repair),
            ) {
                Ok(scheduled) => {
                    if let Some(work) = self.scheduled_discovery.get_mut(scheduled.work()) {
                        work.revalidate_pool = Some(pool.clone());
                    }
                    self.pending_revalidations.remove(&(pool, owner));
                }
                Err(AmmRuntimeCommandError::ColdStartWorkerUnavailable)
                | Err(AmmRuntimeCommandError::ColdStartWorker(_)) => break,
                Err(_) => {
                    self.mark_untrusted();
                    break;
                }
            }
        }
    }

    fn queue_followup(
        &mut self,
        pool: super::PoolInstanceId,
        class: AmmWorkClass,
        kind: AmmWorkKind,
        task: AmmFollowUpTask,
    ) -> Result<AmmScheduledFollowUp, AmmRuntimeCommandError> {
        self.require_trusted()?;
        if self.engine.ownership().active_pool(pool.key()) != Some(&pool) {
            return Err(AmmRuntimeCommandError::StalePoolInstance {
                requested: Box::new(pool.clone()),
                active: self
                    .engine
                    .ownership()
                    .active_pool(pool.key())
                    .cloned()
                    .map(Box::new),
            });
        }
        let worker = self
            .cold_start_worker
            .clone()
            .ok_or(AmmRuntimeCommandError::ColdStartWorkerUnavailable)?;
        if let Some(existing) = self.pending_pool_followup.get(&pool).cloned() {
            let scheduled = self
                .scheduled_followups
                .get(&existing)
                .ok_or_else(|| AmmRuntimeCommandError::StaleWork(existing.clone()))?;
            if scheduled.task == task && scheduled.kind == kind {
                return Ok(AmmScheduledFollowUp::new(pool, existing, class, kind));
            }
            if kind != AmmWorkKind::Repair || scheduled.kind == AmmWorkKind::Repair {
                return Err(AmmRuntimeCommandError::PoolAlreadyScheduled(
                    pool.key().clone(),
                ));
            }
            let _ = worker.cancel(existing.clone());
            self.cancel_scheduled_work(&existing)?;
        }
        let registration = self
            .engine
            .registry()
            .pool(pool.key())
            .cloned()
            .ok_or_else(|| AmmRuntimeCommandError::StalePoolInstance {
                requested: Box::new(pool.clone()),
                active: None,
            })?;
        self.next_event_sequence(2)?;
        let work = RuntimeWorkId::new(RuntimeOwnerId::Pool(pool.clone()), self.next_work_id);
        let next_work_id = self.next_work_id.checked_next()?;
        self.engine
            .ownership_mut()
            .track_work(work.clone())
            .map_err(AmmSyncError::Ownership)?;
        let submit = match &task {
            AmmFollowUpTask::Refresh { policy, .. } => worker.submit(vec![AmmColdStartJob {
                work: work.clone(),
                pool: pool.clone(),
                registration,
                baseline: self.point,
                registry: Arc::clone(&self.registry_snapshot),
                cache: self.cache.snapshot(),
                policy: *policy,
                class,
                target: AmmColdStartTarget::ActiveRefresh,
            }]),
            AmmFollowUpTask::SlotPatch { slots } => worker.submit_slot_patch(AmmSlotPatchJob::new(
                work.clone(),
                pool.clone(),
                self.point,
                slots.iter().copied(),
                class,
            )),
        };
        if let Err(error) = submit {
            self.engine.ownership_mut().untrack_work(&work);
            return Err(AmmRuntimeCommandError::ColdStartWorker(error.to_string()));
        }
        self.next_work_id = next_work_id;
        self.active_work.insert(
            work.clone(),
            AmmWorkProgress::new(
                kind,
                0,
                matches!(task, AmmFollowUpTask::SlotPatch { .. }).then_some(1),
            )
            .expect("new follow-up progress is valid"),
        );
        self.scheduled_followups.insert(
            work.clone(),
            AmmScheduledFollowUpWork {
                pool: pool.clone(),
                class,
                kind,
                task,
                queued: true,
            },
        );
        self.pending_pool_followup
            .insert(pool.clone(), work.clone());
        self.adjust_queue_depth(class, 1);
        let mut events = Vec::new();
        if kind == AmmWorkKind::Repair {
            let from = self
                .engine
                .lifecycles()
                .pool(&pool)
                .unwrap_or(PoolRuntimeState::Degraded);
            self.pending_lifecycles
                .insert(pool.clone(), PoolRuntimeState::CatchingUp);
            events.push(AmmRuntimeEventKind::PoolLifecycleTransition {
                pool: pool.clone(),
                from,
                to: PoolRuntimeState::CatchingUp,
            });
        }
        events.push(AmmRuntimeEventKind::WorkQueued {
            work: work.clone(),
            class,
            kind,
        });
        self.publish_runtime_events(events)?;
        Ok(AmmScheduledFollowUp::new(pool, work, class, kind))
    }

    fn start_scheduled_work(&mut self, work: &RuntimeWorkId) -> Result<(), AmmRuntimeCommandError> {
        if let Some(scheduled) = self.scheduled_followups.get(work).cloned() {
            if !scheduled.queued
                || self.engine.ownership().active_pool(scheduled.pool.key())
                    != Some(&scheduled.pool)
            {
                return Err(AmmRuntimeCommandError::StaleWork(work.clone()));
            }
            let is_refresh = matches!(scheduled.task, AmmFollowUpTask::Refresh { .. });
            self.next_event_sequence(if is_refresh { 2 } else { 1 })?;
            if let Some(scheduled) = self.scheduled_followups.get_mut(work) {
                scheduled.queued = false;
            }
            self.adjust_queue_depth(scheduled.class, -1);
            let progress = self
                .active_work
                .get(work)
                .cloned()
                .ok_or_else(|| AmmRuntimeCommandError::StaleWork(work.clone()))?;
            let mut events = Vec::new();
            if is_refresh {
                events.push(AmmRuntimeEventKind::ColdStartRoundStarted {
                    work: work.clone(),
                    round: 0,
                    total_rounds: None,
                });
            }
            events.push(AmmRuntimeEventKind::WorkProgress {
                work: work.clone(),
                progress,
            });
            return self.publish_runtime_events(events);
        }
        if let Some(scheduled) = self.scheduled_discovery.get(work).cloned() {
            if !scheduled.queued
                || self
                    .engine
                    .ownership()
                    .active_discovery(scheduled.owner.key())
                    != Some(&scheduled.owner)
            {
                return Err(AmmRuntimeCommandError::StaleWork(work.clone()));
            }
            self.next_event_sequence(1)?;
            if let Some(scheduled) = self.scheduled_discovery.get_mut(work) {
                scheduled.queued = false;
            }
            self.adjust_queue_depth(scheduled.class, -1);
            let progress = self
                .active_work
                .get(work)
                .cloned()
                .ok_or_else(|| AmmRuntimeCommandError::StaleWork(work.clone()))?;
            return self.publish_runtime_events(vec![AmmRuntimeEventKind::WorkProgress {
                work: work.clone(),
                progress,
            }]);
        }
        let scheduled = self
            .scheduled_work
            .get(work)
            .cloned()
            .ok_or_else(|| AmmRuntimeCommandError::StaleWork(work.clone()))?;
        let pool = scheduled.reservation.instance().clone();
        if self.pending_lifecycles.get(&pool) != Some(&PoolRuntimeState::Queued) {
            return Err(AmmRuntimeCommandError::StaleWork(work.clone()));
        }
        self.next_event_sequence(3)?;
        self.pending_lifecycles
            .insert(pool.clone(), PoolRuntimeState::Hydrating);
        if let Some(scheduled) = self.scheduled_work.get_mut(work) {
            scheduled.queued = false;
        }
        self.adjust_queue_depth(scheduled.class, -1);
        let progress = self
            .active_work
            .get(work)
            .cloned()
            .ok_or_else(|| AmmRuntimeCommandError::StaleWork(work.clone()))?;
        self.publish_runtime_events(vec![
            AmmRuntimeEventKind::PoolLifecycleTransition {
                pool,
                from: PoolRuntimeState::Queued,
                to: PoolRuntimeState::Hydrating,
            },
            AmmRuntimeEventKind::ColdStartRoundStarted {
                work: work.clone(),
                round: 0,
                total_rounds: None,
            },
            AmmRuntimeEventKind::WorkProgress {
                work: work.clone(),
                progress,
            },
        ])
    }

    fn report_scheduled_round(
        &mut self,
        work: &RuntimeWorkId,
        round: u64,
        next_round: Option<u64>,
    ) -> Result<(), AmmRuntimeCommandError> {
        if let Some(scheduled) = self.scheduled_followups.get(work).cloned() {
            if scheduled.queued {
                return Err(AmmRuntimeCommandError::StaleWork(work.clone()));
            }
            let current = self
                .active_work
                .get(work)
                .cloned()
                .ok_or_else(|| AmmRuntimeCommandError::StaleWork(work.clone()))?;
            if current.completed() != round
                || next_round.is_some_and(|next| next != round.saturating_add(1))
            {
                return Err(AmmRuntimeCommandError::StaleWork(work.clone()));
            }
            let completed = round
                .checked_add(1)
                .ok_or_else(|| RuntimeSequenceOverflow::new("follow-up round"))?;
            let progress = AmmWorkProgress::new(scheduled.kind, completed, current.total())
                .map_err(|_| AmmRuntimeCommandError::StaleWork(work.clone()))?;
            let is_refresh = matches!(scheduled.task, AmmFollowUpTask::Refresh { .. });
            self.next_event_sequence(if is_refresh { 2 } else { 1 })?;
            self.active_work.insert(work.clone(), progress.clone());
            if next_round.is_some() {
                self.adjust_queue_depth(scheduled.class, 1);
                if let Some(scheduled) = self.scheduled_followups.get_mut(work) {
                    scheduled.queued = true;
                }
            }
            let mut events = Vec::new();
            if is_refresh {
                events.push(AmmRuntimeEventKind::ColdStartRoundCompleted {
                    work: work.clone(),
                    round,
                });
            }
            events.push(AmmRuntimeEventKind::WorkProgress {
                work: work.clone(),
                progress,
            });
            return self.publish_runtime_events(events);
        }
        if let Some(scheduled) = self.scheduled_discovery.get(work).cloned() {
            if scheduled.queued || round != 0 || next_round.is_some() {
                return Err(AmmRuntimeCommandError::StaleWork(work.clone()));
            }
            let current = self
                .active_work
                .get(work)
                .ok_or_else(|| AmmRuntimeCommandError::StaleWork(work.clone()))?;
            if current.completed() != 0 {
                return Err(AmmRuntimeCommandError::StaleWork(work.clone()));
            }
            self.next_event_sequence(1)?;
            let progress = AmmWorkProgress::new(AmmWorkKind::Discovery, 1, Some(1))
                .expect("one of one is valid discovery progress");
            self.active_work.insert(work.clone(), progress.clone());
            return self.publish_runtime_events(vec![AmmRuntimeEventKind::WorkProgress {
                work: work.clone(),
                progress,
            }]);
        }
        let scheduled = self
            .scheduled_work
            .get(work)
            .cloned()
            .ok_or_else(|| AmmRuntimeCommandError::StaleWork(work.clone()))?;
        if self
            .pending_lifecycles
            .get(scheduled.reservation.instance())
            != Some(&PoolRuntimeState::Hydrating)
        {
            return Err(AmmRuntimeCommandError::StaleWork(work.clone()));
        }
        let current = self
            .active_work
            .get(work)
            .ok_or_else(|| AmmRuntimeCommandError::StaleWork(work.clone()))?;
        if current.completed() != round
            || next_round.is_some_and(|next| next != round.saturating_add(1))
        {
            return Err(AmmRuntimeCommandError::StaleWork(work.clone()));
        }
        self.next_event_sequence(2)?;
        let completed = round
            .checked_add(1)
            .ok_or_else(|| RuntimeSequenceOverflow::new("cold-start round"))?;
        let progress = AmmWorkProgress::new(AmmWorkKind::ColdStart, completed, None)
            .expect("unknown-total progress is always valid");
        self.active_work.insert(work.clone(), progress.clone());
        if next_round.is_some() {
            self.adjust_queue_depth(scheduled.class, 1);
            if let Some(scheduled) = self.scheduled_work.get_mut(work) {
                scheduled.queued = true;
            }
        }
        self.publish_runtime_events(vec![
            AmmRuntimeEventKind::ColdStartRoundCompleted {
                work: work.clone(),
                round,
            },
            AmmRuntimeEventKind::WorkProgress {
                work: work.clone(),
                progress,
            },
        ])
    }

    fn begin_scheduled_round(
        &mut self,
        work: &RuntimeWorkId,
        round: u64,
    ) -> Result<(), AmmRuntimeCommandError> {
        if let Some(scheduled) = self.scheduled_followups.get(work).cloned() {
            if !matches!(scheduled.task, AmmFollowUpTask::Refresh { .. })
                || self
                    .active_work
                    .get(work)
                    .is_none_or(|progress| progress.completed() != round)
                || !scheduled.queued
            {
                return Err(AmmRuntimeCommandError::StaleWork(work.clone()));
            }
            self.next_event_sequence(1)?;
            if let Some(scheduled) = self.scheduled_followups.get_mut(work) {
                scheduled.queued = false;
            }
            self.adjust_queue_depth(scheduled.class, -1);
            return self.publish_runtime_events(vec![AmmRuntimeEventKind::ColdStartRoundStarted {
                work: work.clone(),
                round,
                total_rounds: None,
            }]);
        }
        let scheduled = self
            .scheduled_work
            .get(work)
            .cloned()
            .ok_or_else(|| AmmRuntimeCommandError::StaleWork(work.clone()))?;
        if self
            .pending_lifecycles
            .get(scheduled.reservation.instance())
            != Some(&PoolRuntimeState::Hydrating)
            || self
                .active_work
                .get(work)
                .is_none_or(|progress| progress.completed() != round)
            || !scheduled.queued
        {
            return Err(AmmRuntimeCommandError::StaleWork(work.clone()));
        }
        self.next_event_sequence(1)?;
        if let Some(scheduled) = self.scheduled_work.get_mut(work) {
            scheduled.queued = false;
        }
        self.adjust_queue_depth(scheduled.class, -1);
        self.publish_runtime_events(vec![AmmRuntimeEventKind::ColdStartRoundStarted {
            work: work.clone(),
            round,
            total_rounds: None,
        }])
    }

    async fn commit_scheduled_discovery(
        &mut self,
        work: RuntimeWorkId,
        owner: DiscoveryOwnerId,
        report: TokenEdgeDiscoveryReport,
    ) -> Result<(), AmmRuntimeCommandError> {
        let scheduled = self
            .scheduled_discovery
            .get(&work)
            .cloned()
            .ok_or_else(|| AmmRuntimeCommandError::StaleWork(work.clone()))?;
        if scheduled.baseline != self.point {
            return Err(AmmRuntimeCommandError::StaleBaseline {
                expected: self.point,
                actual: scheduled.baseline,
            });
        }
        if scheduled.owner != owner
            || report.request() != &scheduled.request
            || self.engine.ownership().active_discovery(owner.key()) != Some(&owner)
        {
            return Err(AmmRuntimeCommandError::StaleWork(work));
        }
        let watcher = self.factory_watchers.get(&owner).ok_or_else(|| {
            AmmRuntimeCommandError::UntrustedBatch(
                "discovery completion had no active watcher".to_owned(),
            )
        })?;
        let protocols = watcher.ownership.adapter().key().protocols();
        if let Some(pool) = scheduled.revalidate_pool.clone() {
            let found = report.discovered().iter().any(|discovered| {
                discovered.key == *pool.key()
                    && discovered.registration.key == *pool.key()
                    && protocols.contains(&discovered.registration.protocol())
            });
            self.next_event_sequence(1)?;
            self.scheduled_discovery.remove(&work);
            self.active_work.remove(&work);
            self.engine.ownership_mut().untrack_work(&work);
            self.publish_runtime_events(vec![AmmRuntimeEventKind::WorkCompleted { work }])?;
            if found {
                let retained = self
                    .registration_evidence
                    .get(&pool)
                    .map(|evidence| {
                        evidence
                            .evidence()
                            .iter()
                            .filter(|item| {
                                !matches!(
                                    item,
                                    RegistrationProvenance::StateQuery { owner: source, .. }
                                        if source == &owner
                                )
                            })
                            .cloned()
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                self.registration_evidence.insert(
                    pool.clone(),
                    RegistrationEvidenceSet::new(
                        RegistrationProvenance::state_query(
                            owner.clone(),
                            self.point.chain_id(),
                            self.point.block_number(),
                            self.point.block_hash(),
                            QueryEvidencePolicy::RevalidateOnReorg,
                        ),
                        retained,
                    ),
                );
                self.registration_revalidation
                    .entry(pool)
                    .or_default()
                    .insert(owner, scheduled.request);
            } else if self.engine.ownership().active_pool(pool.key()) == Some(&pool) {
                if let Some(requests) = self.registration_revalidation.get_mut(&pool) {
                    requests.remove(&owner);
                    if requests.is_empty() {
                        self.registration_revalidation.remove(&pool);
                    }
                }
                if let Some(retained) = self
                    .registration_evidence
                    .get(&pool)
                    .and_then(|evidence| evidence_without_owner(evidence, &owner))
                {
                    self.registration_evidence.insert(pool, retained);
                } else {
                    Box::pin(self.remove_pool_serialized(pool, super::AmmEvictionPolicy::Retain))
                        .await?;
                }
            }
            self.drain_revalidations();
            return Ok(());
        }
        let mut keys = BTreeSet::new();
        let mut registrations = report
            .discovered()
            .iter()
            .filter(|discovered| {
                protocols.contains(&discovered.registration.protocol())
                    && discovered.key == discovered.registration.key
                    && keys.insert(discovered.key.clone())
            })
            .map(|discovered| discovered.registration.clone())
            .collect::<Vec<_>>();
        if let Some(max_candidates) = scheduled.max_candidates {
            registrations.sort_by(|left, right| left.key.cmp(&right.key));
            registrations.truncate(max_candidates);
        }
        for registration in registrations {
            self.retain_factory_candidate(AmmFactoryCandidate {
                owner: owner.clone(),
                registration,
                evidence: RegistrationEvidenceSet::new(
                    RegistrationProvenance::state_query(
                        owner.clone(),
                        scheduled.baseline.chain_id(),
                        scheduled.baseline.block_number(),
                        scheduled.baseline.block_hash(),
                        QueryEvidencePolicy::RevalidateOnReorg,
                    ),
                    [],
                ),
                revalidate: Some(scheduled.request.clone()),
            });
        }
        self.next_event_sequence(1)?;
        self.scheduled_discovery.remove(&work);
        self.active_work.remove(&work);
        self.engine.ownership_mut().untrack_work(&work);
        self.publish_runtime_events(vec![AmmRuntimeEventKind::WorkCompleted { work }])?;
        self.drain_factory_candidates();
        self.drain_revalidations();
        Ok(())
    }

    async fn commit_scheduled_refresh(
        &mut self,
        prepared: AmmPreparedPoolState,
    ) -> Result<Arc<AmmChangeSet>, AmmRuntimeCommandError> {
        let (registration, baseline, storage, accounts, mut deferred, schedule) =
            prepared.into_parts();
        let (work, pool) = schedule.ok_or_else(|| {
            AmmRuntimeCommandError::ColdStartWorker(
                "scheduled refresh has no generation proof".to_owned(),
            )
        })?;
        let scheduled = self
            .scheduled_followups
            .get(&work)
            .cloned()
            .ok_or_else(|| AmmRuntimeCommandError::StaleWork(work.clone()))?;
        if baseline != self.point {
            return Err(AmmRuntimeCommandError::StaleBaseline {
                expected: self.point,
                actual: baseline,
            });
        }
        if scheduled.pool != pool
            || !matches!(scheduled.task, AmmFollowUpTask::Refresh { .. })
            || self.pending_pool_followup.get(&pool) != Some(&work)
            || self.engine.ownership().active_pool(pool.key()) != Some(&pool)
            || registration.key != *pool.key()
        {
            return Err(AmmRuntimeCommandError::StaleWork(work));
        }
        let deferred_slots = deferred
            .iter()
            .filter_map(|work| match work {
                DeferredWork::VerifySlots(slots)
                | DeferredWork::Repair(RepairAction::VerifySlots(slots)) => Some(slots),
                _ => None,
            })
            .flatten()
            .copied()
            .collect::<BTreeSet<_>>();
        self.validate_prepared_pool_state(
            std::slice::from_ref(&registration),
            &storage,
            true,
            &deferred_slots,
        )?;
        self.validate_prepared_pool_accounts(
            std::slice::from_ref(&registration),
            accounts.as_ref(),
            true,
        )?;
        if let Some(accounts) = accounts.as_ref() {
            self.cache
                .validate_prepared_account_patch(accounts)
                .map_err(|error| AmmRuntimeCommandError::ColdStartWorker(error.to_string()))?;
        }
        let refresh = self
            .engine
            .prepare_pool_refresh(pool.clone(), registration)?;
        if refresh.previous_subscription().handler() != refresh.replacement_subscription().handler()
        {
            return Err(AmmRuntimeCommandError::UntrustedBatch(
                "same-generation refresh changed its subscriber owner".to_owned(),
            ));
        }
        let interests_changed = format!("{:?}", refresh.previous_subscription().interests())
            != format!("{:?}", refresh.replacement_subscription().interests());
        let next_interest_revision = if interests_changed {
            self.interest_revision
                .checked_add(1)
                .ok_or(AmmRuntimeCommandError::InterestRevisionExhausted)?
        } else {
            self.interest_revision
        };
        let subscriber = self.subscriber.clone();
        let subscriber_transaction = match (&subscriber, interests_changed) {
            (Some(subscriber), true) => {
                let plan = AmmSubscriberOwnerPlan::new(
                    refresh.replacement_subscription().handler().clone(),
                    refresh.replacement_subscription().interests().to_vec(),
                );
                let subscriber = subscriber.clone();
                let point = self.point;
                Some(
                    self.await_subscriber_fence(async move {
                        subscriber.begin_replace(plan, point).await
                    })
                    .await?,
                )
            }
            _ => None,
        };
        if let Err(error) = self.require_trusted() {
            if let (Some(subscriber), Some(transaction)) = (&subscriber, subscriber_transaction) {
                let _ = subscriber.abort(transaction).await;
            }
            return Err(error);
        }
        let post_fence_valid = baseline == self.point
            && self.scheduled_followups.get(&work).is_some_and(|current| {
                current.pool == pool && matches!(current.task, AmmFollowUpTask::Refresh { .. })
            })
            && self.pending_pool_followup.get(&pool) == Some(&work)
            && self.engine.ownership().active_pool(pool.key()) == Some(&pool);
        if !post_fence_valid {
            if let (Some(subscriber), Some(transaction)) = (&subscriber, subscriber_transaction) {
                let _ = subscriber.abort(transaction).await;
            }
            if baseline != self.point {
                return Err(AmmRuntimeCommandError::StaleBaseline {
                    expected: self.point,
                    actual: baseline,
                });
            }
            return Err(AmmRuntimeCommandError::StaleWork(work));
        }
        let revision = match self
            .revisions
            .get(&pool)
            .copied()
            .unwrap_or_else(|| PoolStateRevision::new(0))
            .checked_next()
        {
            Ok(revision) => revision,
            Err(error) => {
                if let (Some(subscriber), Some(transaction)) = (&subscriber, subscriber_transaction)
                {
                    let _ = subscriber.abort(transaction).await;
                }
                return Err(error.into());
            }
        };
        let repair = scheduled.kind == AmmWorkKind::Repair;
        let expected_quality = if self.engine.has_other_degraded_pool(pool.key()) {
            AmmStateQuality::Degraded
        } else {
            AmmStateQuality::Coherent
        };
        let health_changes = runtime_health(expected_quality) != self.health;
        let event_count = 1 + usize::from(repair) + usize::from(health_changes);
        let (version, first_sequence) = self.next_commit_identity(event_count)?;
        let permit = match self.reserve_critical().await {
            Ok(permit) => permit,
            Err(error) => {
                if let (Some(subscriber), Some(transaction)) = (&subscriber, subscriber_transaction)
                {
                    let _ = subscriber.abort(transaction).await;
                }
                return Err(error);
            }
        };
        if let (Some(subscriber), Some(transaction)) = (&subscriber, subscriber_transaction)
            && let Err(error) = subscriber
                .commit(transaction, next_interest_revision, self.point)
                .await
        {
            return Err(AmmRuntimeCommandError::Subscriber(error.to_string()));
        }
        let storage_rollback = self.apply_prepared_storage(&storage);
        if let Err(error) = self.engine.commit_pool_refresh(refresh) {
            self.restore_prepared_storage(&storage_rollback);
            self.mark_untrusted();
            return Err(error.into());
        }
        self.interest_revision = next_interest_revision;
        self.interest_revisions.send_replace(next_interest_revision);
        if let Some(accounts) = accounts.as_ref()
            && let Err(error) = self.cache.apply_prepared_account_patch(accounts)
        {
            self.mark_untrusted();
            return Err(AmmRuntimeCommandError::ColdStartWorker(error.to_string()));
        }
        let mut revisions = (*self.revisions).clone();
        revisions.insert(pool.clone(), revision);
        let registry_snapshot = Arc::new(AdapterRegistrySnapshot::try_new(
            self.engine.registry(),
            self.engine.ownership(),
        )?);
        let quality = self.registry_quality();
        let changes = Arc::new(AmmChangeSet::new(
            version,
            self.point,
            quality,
            [AmmPoolChange::new(
                pool.clone(),
                revision,
                if repair {
                    AmmPoolChangeKind::Recovered
                } else {
                    AmmPoolChangeKind::Updated
                },
                super::AmmChangeImpact::all(),
            )],
            [],
            false,
        )?);
        let mut events = Vec::with_capacity(event_count);
        if repair {
            self.pending_lifecycles.remove(&pool);
            events.push(AmmRuntimeEventKind::PoolLifecycleTransition {
                pool: pool.clone(),
                from: PoolRuntimeState::CatchingUp,
                to: self
                    .engine
                    .lifecycles()
                    .pool(&pool)
                    .unwrap_or(PoolRuntimeState::Searchable),
            });
        }
        if health_changes {
            events.push(AmmRuntimeEventKind::HealthChanged {
                from: self.health,
                to: runtime_health(quality),
            });
        }
        events.push(AmmRuntimeEventKind::StateCommitted {
            version,
            point: self.point,
        });
        let published = self.publish_commit(
            changes,
            registry_snapshot,
            Arc::new(revisions),
            permit,
            first_sequence,
            events,
        );
        if let AmmFollowUpTask::Refresh { after_slots, .. } = &scheduled.task
            && !after_slots.is_empty()
        {
            deferred.push(DeferredWork::VerifySlots(after_slots.clone()));
        }
        self.finish_followup(&work, &scheduled)?;
        if !deferred.is_empty() {
            self.retain_followup_intent(pool, AmmPendingFollowUpIntent::Deferred(deferred));
            self.drain_followup_intents();
        }
        Ok(published)
    }

    async fn commit_scheduled_slot_patch(
        &mut self,
        work: RuntimeWorkId,
        pool: super::PoolInstanceId,
        baseline: AmmStatePoint,
        storage: Vec<AmmPreparedStorage>,
    ) -> Result<Arc<AmmChangeSet>, AmmRuntimeCommandError> {
        let scheduled = self
            .scheduled_followups
            .get(&work)
            .cloned()
            .ok_or_else(|| AmmRuntimeCommandError::StaleWork(work.clone()))?;
        let AmmFollowUpTask::SlotPatch { slots } = &scheduled.task else {
            return Err(AmmRuntimeCommandError::StaleWork(work));
        };
        if baseline != self.point {
            return Err(AmmRuntimeCommandError::StaleBaseline {
                expected: self.point,
                actual: baseline,
            });
        }
        if scheduled.pool != pool
            || self.pending_pool_followup.get(&pool) != Some(&work)
            || self.engine.ownership().active_pool(pool.key()) != Some(&pool)
        {
            return Err(AmmRuntimeCommandError::StaleWork(work));
        }
        let provided = storage
            .iter()
            .map(|entry| (entry.address(), entry.slot()))
            .collect::<BTreeSet<_>>();
        let expected = slots.iter().copied().collect::<BTreeSet<_>>();
        if provided != expected || provided.len() != storage.len() {
            return Err(AmmRuntimeCommandError::ColdStartWorker(
                "scheduled slot patch did not exactly cover its declared slots".to_owned(),
            ));
        }
        let mut registration = self
            .engine
            .registry()
            .pool(pool.key())
            .cloned()
            .ok_or_else(|| AmmRuntimeCommandError::StaleWork(work.clone()))?;
        let adapter = self
            .engine
            .registry()
            .adapter(registration.protocol())
            .ok_or_else(|| AmmRuntimeCommandError::StaleWork(work.clone()))?;
        let declared = adapter
            .state_dependencies(&registration)
            .slots()
            .iter()
            .map(|slot| (slot.address(), slot.slot()))
            .collect::<BTreeSet<_>>();
        if !provided.is_subset(&declared) {
            return Err(AmmRuntimeCommandError::ColdStartWorker(
                "scheduled slot patch escaped the active adapter read set".to_owned(),
            ));
        }
        let repair = scheduled.kind == AmmWorkKind::Repair;
        if repair {
            registration.status = super::PoolStatus::Ready;
        }
        let expected_quality = if repair && !self.engine.has_other_degraded_pool(pool.key()) {
            AmmStateQuality::Coherent
        } else {
            self.registry_quality()
        };
        let health_changes = runtime_health(expected_quality) != self.health;
        let event_count = 1 + usize::from(repair) + usize::from(health_changes);
        let (version, first_sequence) = self.next_commit_identity(event_count)?;
        let permit = self.reserve_critical().await?;
        let storage_rollback = self.apply_prepared_storage(&storage);
        if repair {
            let refresh = self
                .engine
                .prepare_pool_refresh(pool.clone(), registration)?;
            if let Err(error) = self.engine.commit_pool_refresh(refresh) {
                self.restore_prepared_storage(&storage_rollback);
                return Err(error.into());
            }
        }
        let mut revisions = (*self.revisions).clone();
        let revision = revisions
            .get(&pool)
            .copied()
            .unwrap_or_else(|| PoolStateRevision::new(0))
            .checked_next()?;
        revisions.insert(pool.clone(), revision);
        let registry_snapshot = Arc::new(AdapterRegistrySnapshot::try_new(
            self.engine.registry(),
            self.engine.ownership(),
        )?);
        let quality = self.registry_quality();
        let changes = Arc::new(AmmChangeSet::new(
            version,
            self.point,
            quality,
            [AmmPoolChange::new(
                pool.clone(),
                revision,
                if repair {
                    AmmPoolChangeKind::Recovered
                } else {
                    AmmPoolChangeKind::Updated
                },
                if repair {
                    super::AmmChangeImpact::new(true, true, false)
                } else {
                    super::AmmChangeImpact::state_only()
                },
            )],
            [],
            false,
        )?);
        let mut events = Vec::with_capacity(event_count);
        if repair {
            self.pending_lifecycles.remove(&pool);
            events.push(AmmRuntimeEventKind::PoolLifecycleTransition {
                pool: pool.clone(),
                from: PoolRuntimeState::CatchingUp,
                to: self
                    .engine
                    .lifecycles()
                    .pool(&pool)
                    .unwrap_or(PoolRuntimeState::Searchable),
            });
        }
        if health_changes {
            events.push(AmmRuntimeEventKind::HealthChanged {
                from: self.health,
                to: runtime_health(quality),
            });
        }
        events.push(AmmRuntimeEventKind::StateCommitted {
            version,
            point: self.point,
        });
        let published = self.publish_commit(
            changes,
            registry_snapshot,
            Arc::new(revisions),
            permit,
            first_sequence,
            events,
        );
        self.finish_followup(&work, &scheduled)?;
        Ok(published)
    }

    fn finish_followup(
        &mut self,
        work: &RuntimeWorkId,
        scheduled: &AmmScheduledFollowUpWork,
    ) -> Result<(), AmmRuntimeCommandError> {
        if scheduled.queued {
            self.adjust_queue_depth(scheduled.class, -1);
        }
        self.scheduled_followups.remove(work);
        self.pending_pool_followup.remove(&scheduled.pool);
        self.pending_lifecycles.remove(&scheduled.pool);
        self.active_work.remove(work);
        self.engine.ownership_mut().untrack_work(work);
        self.publish_runtime_events(vec![AmmRuntimeEventKind::WorkCompleted {
            work: work.clone(),
        }])?;
        self.drain_followup_intents();
        Ok(())
    }

    fn detach_scheduled_work(&mut self, work: &RuntimeWorkId) -> bool {
        let Some(worker) = self.cold_start_worker.clone() else {
            return false;
        };
        let detached = if let Some(scheduled) = self.scheduled_work.remove(work) {
            if scheduled.queued {
                self.adjust_queue_depth(scheduled.class, -1);
            }
            let pool = scheduled.reservation.instance().clone();
            self.engine.cancel_pool_reservation(&scheduled.reservation);
            self.pending_pool_work.remove(pool.key());
            self.pending_lifecycles.remove(&pool);
            true
        } else if let Some(scheduled) = self.scheduled_discovery.remove(work) {
            if scheduled.queued {
                self.adjust_queue_depth(scheduled.class, -1);
            }
            true
        } else if let Some(scheduled) = self.scheduled_followups.remove(work) {
            if scheduled.queued {
                self.adjust_queue_depth(scheduled.class, -1);
            }
            self.pending_pool_followup.remove(&scheduled.pool);
            self.pending_lifecycles.remove(&scheduled.pool);
            true
        } else {
            false
        };
        if detached {
            let _ = worker.cancel(work.clone());
            self.active_work.remove(work);
            self.engine.ownership_mut().untrack_work(work);
        }
        detached
    }

    async fn commit_scheduled_pool(
        &mut self,
        prepared: AmmPreparedPoolState,
    ) -> Result<Arc<AmmChangeSet>, AmmRuntimeCommandError> {
        let (registration, baseline, storage, accounts, deferred, schedule) = prepared.into_parts();
        let (work, pool) = schedule.ok_or_else(|| {
            AmmRuntimeCommandError::ColdStartWorker(
                "scheduled prepared state has no generation proof".to_owned(),
            )
        })?;
        let scheduled = self
            .scheduled_work
            .get(&work)
            .cloned()
            .ok_or_else(|| AmmRuntimeCommandError::StaleWork(work.clone()))?;
        if scheduled.reservation.instance() != &pool
            || scheduled.registration.key != registration.key
            || self.pending_pool_work.get(&registration.key) != Some(&work)
            || self
                .engine
                .ownership()
                .active_adapter(scheduled.adapter.key())
                != Some(&scheduled.adapter)
            || (scheduled.discovery_owner.is_some() && scheduled.supporting_discovery.is_empty())
            || scheduled
                .supporting_discovery
                .iter()
                .any(|owner| self.engine.ownership().active_discovery(owner.key()) != Some(owner))
        {
            return Err(AmmRuntimeCommandError::StaleWork(work));
        }
        // One-pool publication can emit registration, lifecycle, health, and
        // state-commit events before the three terminal work events.
        self.next_event_sequence(7)?;
        let deferred_slots = deferred
            .iter()
            .filter_map(|work| match work {
                DeferredWork::VerifySlots(slots)
                | DeferredWork::Repair(RepairAction::VerifySlots(slots)) => Some(slots),
                _ => None,
            })
            .flatten()
            .copied()
            .collect::<BTreeSet<_>>();
        let changes = self
            .install_prepared_pools(
                vec![registration],
                baseline,
                &storage,
                accounts.as_ref(),
                Some(&scheduled.reservation),
                AmmPreparedInstallPolicy::new(true, &deferred_slots),
            )
            .await?;
        if self
            .active_work
            .get(&work)
            .is_some_and(|progress| progress.completed() == 0)
        {
            self.report_scheduled_round(&work, 0, None)?;
        }
        self.registration_evidence
            .insert(pool.clone(), scheduled.evidence.clone());
        if !scheduled.revalidations.is_empty() {
            self.registration_revalidation
                .insert(pool.clone(), scheduled.revalidations.clone());
        }
        self.scheduled_work.remove(&work);
        self.pending_pool_work.remove(pool.key());
        self.pending_lifecycles.remove(&pool);
        self.active_work.remove(&work);
        self.publish_runtime_events(vec![AmmRuntimeEventKind::WorkCompleted { work }])?;
        if !deferred.is_empty() {
            self.retain_followup_intent(pool, AmmPendingFollowUpIntent::Deferred(deferred));
            self.drain_followup_intents();
        }
        Ok(changes)
    }

    fn fail_scheduled_work(
        &mut self,
        work: &RuntimeWorkId,
        message: String,
    ) -> Result<(), AmmRuntimeCommandError> {
        if let Some(scheduled) = self.scheduled_followups.remove(work) {
            let repair = scheduled.kind == AmmWorkKind::Repair;
            let retry = message.contains("baseline is stale").then(|| {
                let intent = match &scheduled.task {
                    AmmFollowUpTask::Refresh { policy, .. } if repair => {
                        AmmPendingFollowUpIntent::Repair(RepairAction::ColdStart {
                            pool: scheduled.pool.key().clone(),
                            policy: *policy,
                        })
                    }
                    AmmFollowUpTask::Refresh {
                        policy,
                        after_slots,
                    } => {
                        let mut work = vec![DeferredWork::ColdStart {
                            pool: scheduled.pool.key().clone(),
                            policy: *policy,
                        }];
                        if !after_slots.is_empty() {
                            work.push(DeferredWork::VerifySlots(after_slots.clone()));
                        }
                        AmmPendingFollowUpIntent::Deferred(work)
                    }
                    AmmFollowUpTask::SlotPatch { slots } if repair => {
                        AmmPendingFollowUpIntent::Repair(RepairAction::VerifySlots(slots.clone()))
                    }
                    AmmFollowUpTask::SlotPatch { slots } => AmmPendingFollowUpIntent::Deferred(
                        vec![DeferredWork::VerifySlots(slots.clone())],
                    ),
                };
                (scheduled.pool.clone(), intent)
            });
            self.next_event_sequence(if repair { 2 } else { 1 })?;
            if scheduled.queued {
                self.adjust_queue_depth(scheduled.class, -1);
            }
            self.pending_pool_followup.remove(&scheduled.pool);
            self.active_work.remove(work);
            self.engine.ownership_mut().untrack_work(work);
            let mut events = Vec::new();
            if repair {
                self.pending_lifecycles
                    .insert(scheduled.pool.clone(), PoolRuntimeState::Degraded);
                events.push(AmmRuntimeEventKind::PoolLifecycleTransition {
                    pool: scheduled.pool,
                    from: PoolRuntimeState::CatchingUp,
                    to: PoolRuntimeState::Degraded,
                });
            }
            events.push(AmmRuntimeEventKind::WorkFailed {
                work: work.clone(),
                message,
            });
            self.publish_runtime_events(events)?;
            if let Some((pool, intent)) = retry {
                self.retain_followup_intent(pool, intent);
                self.drain_followup_intents();
            } else {
                self.drain_followup_intents();
            }
            return Ok(());
        }
        if let Some(scheduled) = self.scheduled_discovery.remove(work) {
            let stale_baseline = message.contains("baseline is stale");
            let owner_active = self
                .engine
                .ownership()
                .active_discovery(scheduled.owner.key())
                == Some(&scheduled.owner);
            self.next_event_sequence(1)?;
            if scheduled.queued {
                self.adjust_queue_depth(scheduled.class, -1);
            }
            self.active_work.remove(work);
            self.engine.ownership_mut().untrack_work(work);
            self.publish_runtime_events(vec![AmmRuntimeEventKind::WorkFailed {
                work: work.clone(),
                message,
            }])?;
            if let Some(pool) = scheduled.revalidate_pool {
                if owner_active && self.engine.ownership().active_pool(pool.key()) == Some(&pool) {
                    self.pending_revalidations
                        .insert((pool, scheduled.owner), scheduled.request);
                    self.drain_revalidations();
                } else {
                    self.mark_untrusted();
                }
            } else if stale_baseline && owner_active {
                let _ = self.queue_discovery(
                    scheduled.owner,
                    scheduled.request,
                    AmmDiscoveryOptions::default().with_class(scheduled.class),
                );
            }
            return Ok(());
        }
        let scheduled = self
            .scheduled_work
            .get(work)
            .cloned()
            .ok_or_else(|| AmmRuntimeCommandError::StaleWork(work.clone()))?;
        let pool = scheduled.reservation.instance().clone();
        let from = self
            .pending_lifecycles
            .get(&pool)
            .copied()
            .unwrap_or(PoolRuntimeState::Queued);
        self.next_event_sequence(2)?;
        self.scheduled_work.remove(work);
        if scheduled.queued {
            self.adjust_queue_depth(scheduled.class, -1);
        }
        self.engine.cancel_pool_reservation(&scheduled.reservation);
        self.pending_pool_work.remove(pool.key());
        self.pending_lifecycles
            .insert(pool.clone(), PoolRuntimeState::Failed);
        self.active_work.remove(work);
        let retry = message.contains("baseline is stale").then(|| {
            scheduled
                .supporting_discovery
                .iter()
                .find(|owner| self.engine.ownership().active_discovery(owner.key()) == Some(*owner))
                .cloned()
                .map(|owner| {
                    let revalidate = scheduled.revalidations.get(&owner).cloned();
                    AmmFactoryCandidate {
                        owner,
                        registration: scheduled.registration.clone(),
                        evidence: scheduled.evidence.clone(),
                        revalidate,
                    }
                })
        });
        self.publish_runtime_events(vec![
            AmmRuntimeEventKind::PoolLifecycleTransition {
                pool: pool.clone(),
                from,
                to: PoolRuntimeState::Failed,
            },
            AmmRuntimeEventKind::WorkFailed {
                work: work.clone(),
                message,
            },
        ])?;
        if let Some(Some(candidate)) = retry {
            self.retain_factory_candidate(candidate);
            self.drain_factory_candidates();
        }
        Ok(())
    }

    fn cancel_scheduled_work(
        &mut self,
        work: &RuntimeWorkId,
    ) -> Result<(), AmmRuntimeCommandError> {
        if let Some(scheduled) = self.scheduled_followups.remove(work) {
            let repair = scheduled.kind == AmmWorkKind::Repair;
            self.next_event_sequence(if repair { 2 } else { 1 })?;
            if scheduled.queued {
                self.adjust_queue_depth(scheduled.class, -1);
            }
            self.pending_pool_followup.remove(&scheduled.pool);
            self.active_work.remove(work);
            self.engine.ownership_mut().untrack_work(work);
            let mut events = Vec::new();
            if repair {
                self.pending_lifecycles
                    .insert(scheduled.pool.clone(), PoolRuntimeState::Degraded);
                events.push(AmmRuntimeEventKind::PoolLifecycleTransition {
                    pool: scheduled.pool,
                    from: PoolRuntimeState::CatchingUp,
                    to: PoolRuntimeState::Degraded,
                });
            }
            events.push(AmmRuntimeEventKind::WorkCancelled { work: work.clone() });
            return self.publish_runtime_events(events);
        }
        if let Some(scheduled) = self.scheduled_discovery.remove(work) {
            self.next_event_sequence(1)?;
            if scheduled.queued {
                self.adjust_queue_depth(scheduled.class, -1);
            }
            self.active_work.remove(work);
            self.engine.ownership_mut().untrack_work(work);
            return self.publish_runtime_events(vec![AmmRuntimeEventKind::WorkCancelled {
                work: work.clone(),
            }]);
        }
        let scheduled = self
            .scheduled_work
            .get(work)
            .cloned()
            .ok_or_else(|| AmmRuntimeCommandError::StaleWork(work.clone()))?;
        let pool = scheduled.reservation.instance().clone();
        let from = self
            .pending_lifecycles
            .get(&pool)
            .copied()
            .unwrap_or(PoolRuntimeState::Queued);
        self.next_event_sequence(3)?;
        self.scheduled_work.remove(work);
        if scheduled.queued {
            self.adjust_queue_depth(scheduled.class, -1);
        }
        self.engine.cancel_pool_reservation(&scheduled.reservation);
        self.pending_pool_work.remove(pool.key());
        self.pending_lifecycles
            .insert(pool.clone(), PoolRuntimeState::Removed);
        self.active_work.remove(work);
        self.publish_runtime_events(vec![
            AmmRuntimeEventKind::PoolLifecycleTransition {
                pool: pool.clone(),
                from,
                to: PoolRuntimeState::Removing,
            },
            AmmRuntimeEventKind::PoolLifecycleTransition {
                pool,
                from: PoolRuntimeState::Removing,
                to: PoolRuntimeState::Removed,
            },
            AmmRuntimeEventKind::WorkCancelled { work: work.clone() },
        ])
    }

    async fn install_prepared_pools(
        &mut self,
        pools: Vec<super::PoolRegistration>,
        baseline: AmmStatePoint,
        prepared_storage: &[AmmPreparedStorage],
        prepared_accounts: Option<&evm_fork_cache::PreparedAccountPatch>,
        reservation: Option<&AmmPoolGenerationReservation>,
        policy: AmmPreparedInstallPolicy<'_>,
    ) -> Result<Arc<AmmChangeSet>, AmmRuntimeCommandError> {
        if self.subscriber.is_none() {
            self.drain_ready_canonical().await;
        }
        self.require_trusted()?;
        if baseline != self.point {
            return Err(AmmRuntimeCommandError::StaleBaseline {
                expected: self.point,
                actual: baseline,
            });
        }
        if let Some(pool) = pools
            .iter()
            .find(|pool| pool.status != super::PoolStatus::Ready)
        {
            return Err(AmmRuntimeCommandError::PoolNotReady(pool.key.clone()));
        }
        if reservation.is_none()
            && let Some(pool) = pools
                .iter()
                .find(|pool| self.pending_pool_work.contains_key(&pool.key))
        {
            return Err(AmmRuntimeCommandError::PoolAlreadyScheduled(
                pool.key.clone(),
            ));
        }
        self.validate_prepared_pool_state(
            &pools,
            prepared_storage,
            policy.require_artifact,
            policy.allowed_missing_slots,
        )?;
        self.validate_prepared_pool_accounts(&pools, prepared_accounts, policy.require_artifact)?;
        if let Some(accounts) = prepared_accounts {
            self.cache
                .validate_prepared_account_patch(accounts)
                .map_err(|error| AmmRuntimeCommandError::ColdStartWorker(error.to_string()))?;
        }
        let _preflight_interest_revision = if pools.is_empty() {
            self.interest_revision
        } else {
            self.interest_revision
                .checked_add(1)
                .ok_or(AmmRuntimeCommandError::InterestRevisionExhausted)?
        };
        let maximum_events = pools
            .len()
            .checked_mul(2)
            .and_then(|count| count.checked_add(2))
            .ok_or_else(|| RuntimeSequenceOverflow::new("AmmRuntimeEvent.sequence"))?;
        let _preflight_identity = self.next_commit_identity(maximum_events)?;
        let pool_keys: Vec<_> = pools.iter().map(|pool| pool.key.clone()).collect();
        let plans = match reservation {
            Some(reservation) => vec![
                self.engine
                    .preview_reserved_pool_subscription(reservation, &pools[0])?,
            ],
            None => self.engine.preview_pool_subscriptions(&pools)?,
        };
        let expected_instances: Vec<_> = plans.iter().map(|plan| plan.instance().clone()).collect();
        let subscriber = self.subscriber.clone();
        let subscriber_transaction = match &subscriber {
            Some(subscriber) => {
                let subscriber = subscriber.clone();
                let point = self.point;
                Some(
                    self.await_subscriber_fence(
                        async move { subscriber.begin_add(plans, point).await },
                    )
                    .await?,
                )
            }
            None => None,
        };
        let post_fence = (|| {
            self.require_trusted()?;
            if baseline != self.point {
                return Err(AmmRuntimeCommandError::StaleBaseline {
                    expected: self.point,
                    actual: baseline,
                });
            }
            self.validate_prepared_pool_state(
                &pools,
                prepared_storage,
                policy.require_artifact,
                policy.allowed_missing_slots,
            )?;
            self.validate_prepared_pool_accounts(
                &pools,
                prepared_accounts,
                policy.require_artifact,
            )?;
            if let Some(accounts) = prepared_accounts {
                self.cache
                    .validate_prepared_account_patch(accounts)
                    .map_err(|error| AmmRuntimeCommandError::ColdStartWorker(error.to_string()))?;
            }
            let next_interest_revision = if pools.is_empty() {
                self.interest_revision
            } else {
                self.interest_revision
                    .checked_add(1)
                    .ok_or(AmmRuntimeCommandError::InterestRevisionExhausted)?
            };
            let identity = self.next_commit_identity(maximum_events)?;
            Ok((next_interest_revision, identity))
        })();
        let (next_interest_revision, (version, first_sequence)) = match post_fence {
            Ok(preflight) => preflight,
            Err(error) => {
                if let (Some(subscriber), Some(transaction)) = (&subscriber, subscriber_transaction)
                {
                    let _ = subscriber.abort(transaction).await;
                }
                return Err(error);
            }
        };
        let permit = match self.reserve_critical().await {
            Ok(permit) => permit,
            Err(error) => {
                if let (Some(subscriber), Some(transaction)) = (&subscriber, subscriber_transaction)
                {
                    let _ = subscriber.abort(transaction).await;
                }
                return Err(error);
            }
        };
        let storage_rollback = self.apply_prepared_storage(prepared_storage);
        let lifecycle_result = match reservation {
            Some(reservation) => self
                .engine
                .commit_pool_reservation(reservation, pools[0].clone()),
            None => self.engine.add_pools(pools),
        };
        let lifecycle = match lifecycle_result {
            Ok(lifecycle) => lifecycle,
            Err(error) => {
                self.restore_prepared_storage(&storage_rollback);
                if let (Some(subscriber), Some(transaction)) = (&subscriber, subscriber_transaction)
                {
                    let _ = subscriber.abort(transaction).await;
                }
                return Err(error.into());
            }
        };
        if lifecycle.registered_pools() != expected_instances {
            self.rollback_added_pools(&pool_keys, subscriber.as_ref(), subscriber_transaction)
                .await;
            self.restore_prepared_storage(&storage_rollback);
            self.mark_untrusted();
            return Err(AmmRuntimeCommandError::UntrustedBatch(
                "prepared pool generations changed behind the subscriber fence".to_owned(),
            ));
        }
        let live_transitions = if subscriber.is_some() {
            match self
                .engine
                .acknowledge_live_delivery(lifecycle.registered_pools())
            {
                Ok(transitions) => transitions,
                Err(error) => {
                    self.rollback_added_pools(
                        &pool_keys,
                        subscriber.as_ref(),
                        subscriber_transaction,
                    )
                    .await;
                    self.restore_prepared_storage(&storage_rollback);
                    return Err(error.into());
                }
            }
        } else {
            Vec::new()
        };
        let mut revisions = (*self.revisions).clone();
        let mut pool_changes = Vec::with_capacity(lifecycle.registered_pools().len());
        for pool in lifecycle.registered_pools() {
            let revision = PoolStateRevision::new(0);
            revisions.insert(pool.clone(), revision);
            pool_changes.push(AmmPoolChange::new(
                pool.clone(),
                revision,
                AmmPoolChangeKind::Added,
                super::AmmChangeImpact::all(),
            ));
        }
        let registry_snapshot =
            match AdapterRegistrySnapshot::try_new(self.engine.registry(), self.engine.ownership())
            {
                Ok(snapshot) => Arc::new(snapshot),
                Err(error) => {
                    self.rollback_added_pools(
                        &pool_keys,
                        subscriber.as_ref(),
                        subscriber_transaction,
                    )
                    .await;
                    self.restore_prepared_storage(&storage_rollback);
                    return Err(error.into());
                }
            };
        let quality = self.registry_quality();
        let mut observer_events = lifecycle
            .registered_pools()
            .iter()
            .cloned()
            .map(|pool| AmmRuntimeEventKind::RegistrationAccepted { pool })
            .collect::<Vec<_>>();
        observer_events.extend(live_transitions.into_iter().map(|pool| {
            AmmRuntimeEventKind::PoolLifecycleTransition {
                pool,
                from: PoolRuntimeState::Searchable,
                to: PoolRuntimeState::Live,
            }
        }));
        if runtime_health(quality) != self.health {
            observer_events.push(AmmRuntimeEventKind::HealthChanged {
                from: self.health,
                to: runtime_health(quality),
            });
        }
        observer_events.push(AmmRuntimeEventKind::StateCommitted {
            version,
            point: self.point,
        });
        let changes = match AmmChangeSet::new(version, self.point, quality, pool_changes, [], false)
        {
            Ok(changes) => Arc::new(changes),
            Err(error) => {
                self.rollback_added_pools(&pool_keys, subscriber.as_ref(), subscriber_transaction)
                    .await;
                self.restore_prepared_storage(&storage_rollback);
                return Err(error.into());
            }
        };
        if let (Some(subscriber), Some(transaction)) = (&subscriber, subscriber_transaction)
            && let Err(error) = subscriber
                .commit(transaction, next_interest_revision, self.point)
                .await
        {
            self.rollback_added_pools(&pool_keys, Some(subscriber), Some(transaction))
                .await;
            self.restore_prepared_storage(&storage_rollback);
            self.mark_untrusted();
            return Err(AmmRuntimeCommandError::Subscriber(error.to_string()));
        }
        self.interest_revision = next_interest_revision;
        self.interest_revisions.send_replace(next_interest_revision);
        if let Some(accounts) = prepared_accounts
            && let Err(error) = self.cache.apply_prepared_account_patch(accounts)
        {
            self.mark_untrusted();
            return Err(AmmRuntimeCommandError::ColdStartWorker(error.to_string()));
        }
        for pool in lifecycle.registered_pools() {
            self.registration_evidence
                .entry(pool.clone())
                .or_insert_with(|| {
                    RegistrationEvidenceSet::new(
                        RegistrationProvenance::stable(RegistrationSourceKey::new(
                            "runtime.prepared",
                        )),
                        [],
                    )
                });
        }
        if let Some(reservation) = reservation {
            self.pending_lifecycles.remove(reservation.instance());
        }
        Ok(self.publish_commit(
            changes,
            registry_snapshot,
            Arc::new(revisions),
            permit,
            first_sequence,
            observer_events,
        ))
    }

    fn apply_prepared_storage(
        &mut self,
        prepared: &[AmmPreparedStorage],
    ) -> Vec<(
        alloy_primitives::Address,
        alloy_primitives::U256,
        Option<alloy_primitives::U256>,
    )> {
        let rollback = prepared
            .iter()
            .map(|entry| {
                (
                    entry.address(),
                    entry.slot(),
                    self.cache
                        .cached_storage_value(entry.address(), entry.slot()),
                )
            })
            .collect();
        let values: Vec<_> = prepared
            .iter()
            .map(|entry| (entry.address(), entry.slot(), entry.value()))
            .collect();
        self.cache.inject_storage_batch_fresh(&values);
        rollback
    }

    fn restore_prepared_storage(
        &mut self,
        rollback: &[(
            alloy_primitives::Address,
            alloy_primitives::U256,
            Option<alloy_primitives::U256>,
        )],
    ) {
        let prior: Vec<_> = rollback
            .iter()
            .filter_map(|(address, slot, value)| value.map(|value| (*address, *slot, value)))
            .collect();
        self.cache.inject_storage_batch_fresh(&prior);
        for (address, slot, value) in rollback {
            if value.is_none() {
                self.cache
                    .purge_contract_slots(*address, std::slice::from_ref(slot));
            }
        }
    }

    fn validate_prepared_pool_state(
        &self,
        pools: &[super::PoolRegistration],
        prepared_storage: &[AmmPreparedStorage],
        require_artifact_slots: bool,
        allowed_missing_slots: &BTreeSet<(alloy_primitives::Address, alloy_primitives::U256)>,
    ) -> Result<(), AmmRuntimeCommandError> {
        let prepared: BTreeSet<_> = prepared_storage
            .iter()
            .map(|entry| (entry.address(), entry.slot()))
            .collect();
        let mut declared = BTreeSet::new();
        for pool in pools {
            let adapter = self.engine.registry().adapter(pool.protocol()).ok_or(
                AmmRuntimeCommandError::Sync(AmmSyncError::LifecycleInvariant(
                    "prepared pool has no active adapter",
                )),
            )?;
            let dependencies = adapter.state_dependencies(pool);
            declared.extend(
                dependencies
                    .slots()
                    .iter()
                    .map(|slot| (slot.address(), slot.slot())),
            );
            if !dependencies.whole_accounts().is_empty() {
                return Err(AmmRuntimeCommandError::UnverifiablePreparedState {
                    pool: Box::new(pool.key.clone()),
                    whole_accounts: dependencies.whole_accounts().len(),
                });
            }
            let missing: Vec<_> = dependencies
                .slots()
                .iter()
                .copied()
                .filter(|slot| {
                    let identity = (slot.address(), slot.slot());
                    !prepared.contains(&identity)
                        && !allowed_missing_slots.contains(&identity)
                        && (require_artifact_slots
                            || self
                                .cache
                                .cached_storage_value(slot.address(), slot.slot())
                                .is_none())
                })
                .collect();
            if !missing.is_empty() {
                return Err(AmmRuntimeCommandError::MissingPreparedState {
                    pool: Box::new(pool.key.clone()),
                    missing: missing.into_boxed_slice(),
                });
            }
        }
        if let Some((address, slot)) = prepared.difference(&declared).next().copied() {
            return Err(AmmRuntimeCommandError::UnexpectedPreparedState { address, slot });
        }
        Ok(())
    }

    fn validate_prepared_pool_accounts(
        &self,
        pools: &[PoolRegistration],
        prepared_accounts: Option<&evm_fork_cache::PreparedAccountPatch>,
        require_artifact_accounts: bool,
    ) -> Result<(), AmmRuntimeCommandError> {
        let mut declared = BTreeMap::new();
        let mut verified_targets = BTreeSet::new();
        let mut owners = BTreeMap::new();
        for pool in pools {
            let adapter = self.engine.registry().adapter(pool.protocol()).ok_or(
                AmmRuntimeCommandError::Sync(AmmSyncError::LifecycleInvariant(
                    "prepared pool has no active adapter",
                )),
            )?;
            for seed in adapter
                .code_seeds(pool)
                .map_err(|error| AmmRuntimeCommandError::ColdStartWorker(error.to_string()))?
            {
                declared.insert(seed.address, seed.code_hash);
                owners.insert(seed.address, pool.key.clone());
            }
            for address in adapter.verified_code_targets(pool) {
                verified_targets.insert(address);
                owners.insert(address, pool.key.clone());
            }
        }
        let mut provided = BTreeSet::new();
        if let Some(accounts) = prepared_accounts {
            for value in accounts.values() {
                let address = value.address();
                let expected = declared.get(&address);
                if expected.is_none() && !verified_targets.contains(&address) {
                    return Err(AmmRuntimeCommandError::UnexpectedPreparedAccount { address });
                }
                let actual = alloy_primitives::keccak256(value.code());
                if value.proof().code_hash != actual
                    || expected.is_some_and(|expected| *expected != actual)
                {
                    return Err(AmmRuntimeCommandError::PreparedAccountClaimMismatch { address });
                }
                provided.insert(address);
            }
        }
        if require_artifact_accounts
            && let Some(address) = declared
                .keys()
                .chain(verified_targets.iter())
                .find(|address| !provided.contains(*address))
                .copied()
        {
            return Err(AmmRuntimeCommandError::MissingPreparedAccount {
                pool: Box::new(
                    owners
                        .get(&address)
                        .expect("every declared seed has an owner")
                        .clone(),
                ),
                address,
            });
        }
        Ok(())
    }

    async fn rollback_added_pools(
        &mut self,
        pool_keys: &[super::PoolKey],
        subscriber: Option<&AmmSubscriberControl>,
        transaction: Option<super::subscriber_driver::SubscriberTransaction>,
    ) {
        let runtime_rollback = self.engine.remove_pools(pool_keys).is_ok();
        let subscriber_rollback = match (subscriber, transaction) {
            (Some(subscriber), Some(transaction)) => subscriber.abort(transaction).await.is_ok(),
            _ => true,
        };
        if !runtime_rollback || !subscriber_rollback {
            self.mark_untrusted();
        }
    }

    async fn remove_pool(
        &mut self,
        pool: super::PoolInstanceId,
        eviction: super::AmmEvictionPolicy,
    ) -> Result<Arc<AmmChangeSet>, AmmRuntimeCommandError> {
        if self.subscriber.is_none() {
            self.drain_ready_canonical().await;
        }
        self.remove_pool_serialized(pool, eviction).await
    }

    async fn remove_pool_serialized(
        &mut self,
        pool: super::PoolInstanceId,
        eviction: super::AmmEvictionPolicy,
    ) -> Result<Arc<AmmChangeSet>, AmmRuntimeCommandError> {
        self.require_trusted()?;
        let active = self.engine.ownership().active_pool(pool.key()).cloned();
        if active.as_ref() != Some(&pool) {
            return Err(AmmRuntimeCommandError::StalePoolInstance {
                requested: Box::new(pool),
                active: active.map(Box::new),
            });
        }
        let cancelled_work = self
            .scheduled_followups
            .iter()
            .filter(|(_, scheduled)| scheduled.pool == pool)
            .map(|(work, _)| work.clone())
            .collect::<Vec<_>>();
        let maximum_events = 3usize
            .checked_add(cancelled_work.len())
            .ok_or_else(|| RuntimeSequenceOverflow::new("AmmRuntimeEvent.sequence"))?;
        let _preflight_interest_revision = self
            .interest_revision
            .checked_add(1)
            .ok_or(AmmRuntimeCommandError::InterestRevisionExhausted)?;
        let _preflight_identity = self.next_commit_identity(maximum_events)?;
        let _preflight_removal_revision = self
            .revisions
            .get(&pool)
            .copied()
            .unwrap_or_else(|| PoolStateRevision::new(0))
            .checked_next()?;
        let subscriber = self.subscriber.clone();
        let subscriber_transaction = match &subscriber {
            Some(subscriber) => {
                let subscriber = subscriber.clone();
                let owner = super::AmmPoolReactiveHandler::handler_id(&pool);
                Some(
                    self.await_subscriber_fence(async move {
                        subscriber.begin_remove(vec![owner]).await
                    })
                    .await?,
                )
            }
            None => None,
        };
        let post_fence_active = self.engine.ownership().active_pool(pool.key()).cloned();
        if post_fence_active.as_ref() != Some(&pool) {
            if let (Some(subscriber), Some(transaction)) = (&subscriber, subscriber_transaction) {
                let _ = subscriber.abort(transaction).await;
            }
            return Err(AmmRuntimeCommandError::StalePoolInstance {
                requested: Box::new(pool.clone()),
                active: post_fence_active.map(Box::new),
            });
        }
        let cancelled_work = self
            .scheduled_followups
            .iter()
            .filter(|(_, scheduled)| scheduled.pool == pool)
            .map(|(work, _)| work.clone())
            .collect::<Vec<_>>();
        let maximum_events = 3usize
            .checked_add(cancelled_work.len())
            .ok_or_else(|| RuntimeSequenceOverflow::new("AmmRuntimeEvent.sequence"))?;
        let post_fence = (|| {
            self.require_trusted()?;
            let next_interest_revision = self
                .interest_revision
                .checked_add(1)
                .ok_or(AmmRuntimeCommandError::InterestRevisionExhausted)?;
            let identity = self.next_commit_identity(maximum_events)?;
            let removal_revision = self
                .revisions
                .get(&pool)
                .copied()
                .unwrap_or_else(|| PoolStateRevision::new(0))
                .checked_next()?;
            Ok((next_interest_revision, identity, removal_revision))
        })();
        let (next_interest_revision, (version, first_sequence), removal_revision) = match post_fence
        {
            Ok(preflight) => preflight,
            Err(error) => {
                if let (Some(subscriber), Some(transaction)) = (&subscriber, subscriber_transaction)
                {
                    let _ = subscriber.abort(transaction).await;
                }
                return Err(error);
            }
        };
        let permit = match self.reserve_critical().await {
            Ok(permit) => permit,
            Err(error) => {
                if let (Some(subscriber), Some(transaction)) = (&subscriber, subscriber_transaction)
                {
                    let _ = subscriber.abort(transaction).await;
                }
                return Err(error);
            }
        };
        if let (Some(subscriber), Some(transaction)) = (&subscriber, subscriber_transaction)
            && let Err(error) = subscriber
                .commit(transaction, next_interest_revision, self.point)
                .await
        {
            return Err(AmmRuntimeCommandError::Subscriber(error.to_string()));
        }
        let lifecycle_result = match eviction {
            super::AmmEvictionPolicy::Retain => {
                self.engine.remove_pools(std::slice::from_ref(pool.key()))
            }
            super::AmmEvictionPolicy::Exclusive => self
                .engine
                .remove_pools_evicting(std::slice::from_ref(pool.key()), &mut self.cache),
        };
        let lifecycle = match lifecycle_result {
            Ok(lifecycle) => lifecycle,
            Err(error) => {
                self.mark_untrusted();
                return Err(error.into());
            }
        };
        if lifecycle
            .removed_pools()
            .iter()
            .all(|removed| removed.instance() != &pool)
        {
            self.mark_untrusted();
            return Err(AmmRuntimeCommandError::UntrustedBatch(
                "exact pool disappeared during serialized removal".to_owned(),
            ));
        }
        for work in &cancelled_work {
            self.detach_scheduled_work(work);
        }
        self.pending_followup_intents.remove(&pool);
        self.registration_evidence.remove(&pool);
        self.registration_revalidation.remove(&pool);
        self.pending_revalidations
            .retain(|(candidate, _), _| candidate != &pool);
        let mut revisions = (*self.revisions).clone();
        revisions.remove(&pool);
        let registry_snapshot =
            match AdapterRegistrySnapshot::try_new(self.engine.registry(), self.engine.ownership())
            {
                Ok(snapshot) => Arc::new(snapshot),
                Err(error) => {
                    self.mark_untrusted();
                    return Err(error.into());
                }
            };
        let quality = self.registry_quality();
        let mut observer_events = cancelled_work
            .into_iter()
            .map(|work| AmmRuntimeEventKind::WorkCancelled { work })
            .collect::<Vec<_>>();
        observer_events.push(AmmRuntimeEventKind::RegistrationRemoved { pool: pool.clone() });
        if runtime_health(quality) != self.health {
            observer_events.push(AmmRuntimeEventKind::HealthChanged {
                from: self.health,
                to: runtime_health(quality),
            });
        }
        observer_events.push(AmmRuntimeEventKind::StateCommitted {
            version,
            point: self.point,
        });
        let changes = match AmmChangeSet::new(
            version,
            self.point,
            quality,
            [AmmPoolChange::new(
                pool,
                removal_revision,
                AmmPoolChangeKind::Removed,
                super::AmmChangeImpact::all(),
            )],
            [],
            false,
        ) {
            Ok(changes) => Arc::new(changes),
            Err(error) => {
                self.mark_untrusted();
                return Err(error.into());
            }
        };
        self.interest_revision = next_interest_revision;
        self.interest_revisions.send_replace(next_interest_revision);
        Ok(self.publish_commit(
            changes,
            registry_snapshot,
            Arc::new(revisions),
            permit,
            first_sequence,
            observer_events,
        ))
    }

    fn require_trusted(&self) -> Result<(), AmmRuntimeCommandError> {
        if self.trusted {
            Ok(())
        } else {
            Err(AmmRuntimeCommandError::Untrusted)
        }
    }

    async fn reserve_critical(
        &mut self,
    ) -> Result<Option<mpsc::OwnedPermit<Arc<AmmStateCommit>>>, AmmRuntimeCommandError> {
        match self.critical.as_ref().cloned() {
            Some(sender) => tokio::select! {
                permit = sender.reserve_owned() => match permit {
                    Ok(permit) => Ok(Some(permit)),
                    Err(_) => {
                        self.critical = None;
                        Ok(None)
                    }
                },
                _ = self.shutdown.changed() => Err(AmmRuntimeCommandError::Closed),
            },
            None => Ok(None),
        }
    }

    fn next_commit_identity(
        &self,
        observer_event_count: usize,
    ) -> Result<(AmmStateVersion, u64), AmmRuntimeCommandError> {
        let version = self.version.checked_next()?;
        let first_sequence = self.next_event_sequence(observer_event_count)?;
        Ok((version, first_sequence))
    }

    fn next_event_sequence(
        &self,
        observer_event_count: usize,
    ) -> Result<u64, AmmRuntimeCommandError> {
        let event_count = u64::try_from(observer_event_count)
            .map_err(|_| RuntimeSequenceOverflow::new("AmmRuntimeEvent.sequence"))?;
        let first_sequence = self
            .event_sequence
            .checked_add(1)
            .ok_or_else(|| RuntimeSequenceOverflow::new("AmmRuntimeEvent.sequence"))?;
        self.event_sequence
            .checked_add(event_count)
            .ok_or_else(|| RuntimeSequenceOverflow::new("AmmRuntimeEvent.sequence"))?;
        Ok(first_sequence)
    }

    fn adjust_queue_depth(&mut self, class: AmmWorkClass, delta: isize) {
        let current = self.queue_depths.get(class);
        let next = if delta >= 0 {
            current.saturating_add(delta as usize)
        } else {
            current.saturating_sub(delta.unsigned_abs())
        };
        self.queue_depths.set(class, next);
    }

    fn lifecycle_snapshot(&self) -> RuntimeLifecycleMap {
        let mut lifecycles = self.engine.lifecycles().clone();
        for (pool, state) in &self.pending_lifecycles {
            lifecycles.set_pool(pool.clone(), *state);
        }
        for (owner, state) in &self.discovery_lifecycles {
            lifecycles.set_discovery(owner.clone(), *state);
        }
        lifecycles
    }

    fn publish_runtime_events(
        &mut self,
        events: Vec<AmmRuntimeEventKind>,
    ) -> Result<(), AmmRuntimeCommandError> {
        if events.is_empty() {
            return Ok(());
        }
        let first_sequence = self.next_event_sequence(events.len())?;
        for (offset, event) in events.into_iter().enumerate() {
            let sequence = first_sequence + offset as u64;
            self.event_sequence = sequence;
            self.status
                .send_replace(Arc::new(AmmRuntimeStatusSnapshot::new(
                    sequence,
                    self.version,
                    self.lifecycle_snapshot(),
                    self.active_work.clone(),
                    self.queue_depths.clone(),
                    self.health,
                )));
            let _ = self.observers.send(AmmRuntimeEvent::new(sequence, event));
        }
        Ok(())
    }

    fn publish_live_transitions(
        &mut self,
        transitions: Vec<super::PoolInstanceId>,
    ) -> Result<(), AmmRuntimeCommandError> {
        if transitions.is_empty() {
            return Ok(());
        }
        let first_sequence = self.next_event_sequence(transitions.len())?;
        for (offset, pool) in transitions.into_iter().enumerate() {
            let sequence = first_sequence + offset as u64;
            self.event_sequence = sequence;
            self.status
                .send_replace(Arc::new(AmmRuntimeStatusSnapshot::new(
                    sequence,
                    self.version,
                    self.lifecycle_snapshot(),
                    self.active_work.clone(),
                    self.queue_depths.clone(),
                    self.health,
                )));
            let _ = self.observers.send(AmmRuntimeEvent::new(
                sequence,
                AmmRuntimeEventKind::PoolLifecycleTransition {
                    pool,
                    from: PoolRuntimeState::Searchable,
                    to: PoolRuntimeState::Live,
                },
            ));
        }
        Ok(())
    }

    fn registry_quality(&self) -> AmmStateQuality {
        if self.engine.degraded_pool_count() > 0 {
            AmmStateQuality::Degraded
        } else {
            AmmStateQuality::Coherent
        }
    }

    fn factory_candidates(
        &self,
        records: &ReactiveInputBatch<Ethereum>,
    ) -> Vec<AmmFactoryCandidate> {
        let mut candidates = Vec::new();
        let mut seen = BTreeSet::new();
        for record in records.records() {
            let ReactiveInput::Log(log) = &record.input else {
                continue;
            };
            if log.removed {
                continue;
            }
            for owner in self.factory_watcher_index.owners_for(log) {
                let Some(watcher) = self.factory_watchers.get(&owner) else {
                    debug_assert!(false, "factory dispatch index referenced a missing watcher");
                    continue;
                };
                let context = super::CreationLogContext::new(log.block_number, log.log_index);
                let Ok(Some(discovered)) = watcher.discovery.decode_creation(&log.inner, context)
                else {
                    continue;
                };
                if !watcher
                    .ownership
                    .adapter()
                    .key()
                    .protocols()
                    .contains(&discovered.registration.protocol())
                    || discovered.key != discovered.registration.key
                    || !seen.insert((owner.clone(), discovered.key.clone()))
                {
                    continue;
                }
                candidates.push(AmmFactoryCandidate {
                    owner: owner.clone(),
                    registration: discovered.registration,
                    evidence: RegistrationEvidenceSet::new(
                        RegistrationProvenance::factory_log(
                            owner.clone(),
                            log.inner.address,
                            self.point.chain_id(),
                            log.block_number.unwrap_or_default(),
                            log.block_hash.unwrap_or_default(),
                            log.transaction_hash.unwrap_or_default(),
                            log.log_index.unwrap_or_default(),
                        ),
                        [],
                    ),
                    revalidate: None,
                });
            }
        }
        candidates
    }

    fn publish_commit(
        &mut self,
        changes: Arc<AmmChangeSet>,
        registry_snapshot: Arc<AdapterRegistrySnapshot>,
        revisions: Arc<PoolRevisionMap>,
        permit: Option<mpsc::OwnedPermit<Arc<AmmStateCommit>>>,
        first_sequence: u64,
        observer_events: Vec<AmmRuntimeEventKind>,
    ) -> Arc<AmmChangeSet> {
        let version = changes.version();
        let point = changes.point();
        let quality = changes.quality();
        let snapshot = Arc::new(AmmStateSnapshot::new(
            self.runtime_id,
            version,
            point,
            self.interest_revision,
            self.cache.snapshot(),
            Arc::clone(&registry_snapshot),
            Arc::clone(&revisions),
        ));
        let commit = Arc::new(AmmStateCommit::new(
            Arc::clone(&snapshot),
            Arc::clone(&changes),
        ));
        self.version = version;
        self.point = point;
        self.registry_snapshot = registry_snapshot;
        self.revisions = revisions;
        self.snapshots.send_replace(snapshot);
        if let Some(permit) = permit {
            permit.send(commit);
        }
        self.publish_commit_status(first_sequence, version, quality, observer_events);
        changes
    }

    async fn ingest(
        &mut self,
        batch: AmmCanonicalBatch,
        origin: AmmCanonicalOrigin,
    ) -> Result<Arc<AmmChangeSet>, AmmRuntimeCommandError> {
        self.require_trusted()?;
        if self.subscriber.is_some() && origin != AmmCanonicalOrigin::Subscriber {
            return Err(AmmRuntimeCommandError::AttachedSubscriberOwnsCanonicalInput);
        }
        if batch.chain_id() != self.point.chain_id() {
            return Err(AmmRuntimeCommandError::ChainMismatch {
                expected: self.point.chain_id(),
                actual: batch.chain_id(),
            });
        }
        if batch.interest_revision() != self.interest_revision {
            return Err(AmmRuntimeCommandError::InterestRevisionMismatch {
                expected: self.interest_revision,
                actual: batch.interest_revision(),
            });
        }
        let next_point =
            AmmStatePoint::post_block(batch.chain_id(), batch.block().number, batch.block().hash);
        let expected_cache_block = batch.block().number;
        let permit = self.reserve_critical().await?;
        let (next_version, _) = self.next_commit_identity(1)?;
        let (header, records) = batch.into_parts();
        let factory_candidates = self.factory_candidates(&records);
        let block = BlockRef {
            number: header.inner.number,
            hash: header.hash,
            parent_hash: Some(header.inner.parent_hash),
            timestamp: Some(header.inner.timestamp),
        };
        let mut complete_records = Vec::with_capacity(records.records().len() + 1);
        complete_records.push(evm_fork_cache::reactive::ReactiveInputRecord::new(
            ReactiveInput::BlockHeader(header),
            evm_fork_cache::reactive::ReactiveContext {
                chain_id: Some(self.point.chain_id()),
                source: evm_fork_cache::reactive::InputSource::Batch,
                chain_status: ChainStatus::Included {
                    block: block.clone(),
                    confirmations: 0,
                },
                block: Some(block),
                transaction_index: None,
                log_index: None,
            },
        ));
        complete_records.extend(records.into_records());
        let report = match self.engine.ingest_batch_deferred_repairs(
            &mut self.cache,
            ReactiveInputBatch::new(complete_records),
        ) {
            Ok(report) => report,
            Err(error) => {
                self.mark_untrusted();
                return Err(error.into());
            }
        };
        if let Some(message) = report.reactive.reports.iter().find_map(|report| {
            if let ReactiveReport::Error(error) = report.as_ref() {
                Some(error.message.clone())
            } else {
                None
            }
        }) {
            self.mark_untrusted();
            return Err(AmmRuntimeCommandError::UntrustedBatch(message));
        }
        if self.cache.block_number() != Some(expected_cache_block) {
            self.mark_untrusted();
            return Err(AmmRuntimeCommandError::CacheContextMismatch {
                expected: expected_cache_block,
                actual: self.cache.block_number(),
            });
        }
        let basefee = self.cache.basefee();
        self.cache
            .set_block(BlockId::from((next_point.block_hash(), Some(true))));
        self.cache
            .set_block_context(Some(expected_cache_block), basefee);
        let pending_repairs = report.pending_repairs().to_vec();
        let dropped_hashes = report
            .incidents
            .iter()
            .filter_map(|incident| match incident {
                AmmSyncIncident::Reorg { dropped_blocks } => Some(dropped_blocks),
                _ => None,
            })
            .flatten()
            .map(|block| block.hash)
            .collect::<Vec<_>>();
        let reorg_actions = if dropped_hashes.is_empty() {
            Vec::new()
        } else {
            self.registration_evidence
                .iter()
                .filter_map(|(pool, evidence)| {
                    let action = evidence.reorg_action(&dropped_hashes);
                    (action != RegistrationReorgAction::Keep).then(|| {
                        (
                            pool.clone(),
                            action,
                            self.registration_revalidation.get(pool).cloned(),
                        )
                    })
                })
                .collect::<Vec<_>>()
        };

        let publication = (|| {
            let mut pool_changes = Vec::with_capacity(report.pool_changes.len());
            let revisions = if report.pool_changes.is_empty() {
                Arc::clone(&self.revisions)
            } else {
                let mut revisions = (*self.revisions).clone();
                for change in &report.pool_changes {
                    let instance = self
                        .engine
                        .ownership()
                        .active_pool(change.pool())
                        .cloned()
                        .ok_or_else(|| {
                            AmmRuntimeCommandError::UntrustedBatch(format!(
                                "sync report referenced inactive pool {:?}",
                                change.pool()
                            ))
                        })?;
                    let revision = revisions
                        .get(&instance)
                        .copied()
                        .unwrap_or_else(|| PoolStateRevision::new(0))
                        .checked_next()?;
                    revisions.insert(instance.clone(), revision);
                    pool_changes.push(AmmPoolChange::new(
                        instance,
                        revision,
                        committed_change_kind(change.kind()),
                        change.impact(),
                    ));
                }
                Arc::new(revisions)
            };
            let quality = self.registry_quality();
            let incidents = report
                .incidents
                .iter()
                .map(|incident| committed_incident(self.point.chain_id(), incident))
                .collect::<Vec<_>>();
            let changes = Arc::new(AmmChangeSet::new(
                next_version,
                next_point,
                quality,
                pool_changes,
                incidents,
                report.requires_full_refresh,
            )?);
            let registry_changed = report.requires_full_refresh
                || !report.degraded_pools.is_empty()
                || !report.recovered_pools.is_empty();
            let registry_snapshot = if registry_changed {
                Arc::new(AdapterRegistrySnapshot::try_new(
                    self.engine.registry(),
                    self.engine.ownership(),
                )?)
            } else {
                Arc::clone(&self.registry_snapshot)
            };
            let mut observer_events = report
                .incidents
                .iter()
                .map(|incident| committed_runtime_incident(self.point.chain_id(), incident))
                .collect::<Vec<_>>();
            let next_health = runtime_health(quality);
            if next_health != self.health {
                observer_events.push(AmmRuntimeEventKind::HealthChanged {
                    from: self.health,
                    to: next_health,
                });
            }
            observer_events.push(AmmRuntimeEventKind::StateCommitted {
                version: next_version,
                point: next_point,
            });
            let (_, first_sequence) = self.next_commit_identity(observer_events.len())?;
            Ok::<_, AmmRuntimeCommandError>((
                changes,
                registry_snapshot,
                revisions,
                first_sequence,
                observer_events,
            ))
        })();
        let (changes, registry_snapshot, revisions, first_sequence, observer_events) =
            match publication {
                Ok(publication) => publication,
                Err(error) => {
                    self.mark_untrusted();
                    return Err(error);
                }
            };
        let published = self.publish_commit(
            changes,
            registry_snapshot,
            revisions,
            permit,
            first_sequence,
            observer_events,
        );
        for (pool, action, revalidation) in reorg_actions {
            if self.engine.ownership().active_pool(pool.key()) != Some(&pool) {
                continue;
            }
            match (action, revalidation) {
                (RegistrationReorgAction::Remove, _) => {
                    Box::pin(self.remove_pool_serialized(pool, super::AmmEvictionPolicy::Retain))
                        .await?;
                }
                (RegistrationReorgAction::Revalidate, Some(revalidations)) => {
                    if let Some(evidence) = self.registration_evidence.get(&pool) {
                        let mut retained = evidence
                            .evidence()
                            .iter()
                            .filter(|item| {
                                !matches!(
                                    item,
                                    RegistrationProvenance::FactoryLog { block_hash, .. }
                                        if dropped_hashes.contains(block_hash)
                                )
                            })
                            .cloned()
                            .collect::<Vec<_>>();
                        if retained.is_empty() {
                            self.mark_untrusted();
                            continue;
                        }
                        let primary = retained.remove(0);
                        self.registration_evidence.insert(
                            pool.clone(),
                            RegistrationEvidenceSet::new(primary, retained),
                        );
                    }
                    for (owner, request) in revalidations {
                        self.pending_revalidations
                            .insert((pool.clone(), owner), request);
                    }
                }
                (RegistrationReorgAction::Revalidate, None) => self.mark_untrusted(),
                (RegistrationReorgAction::Keep, _) => {}
            }
        }
        self.reconcile_orphaned_pending_registrations(&dropped_hashes)?;
        self.drain_revalidations();
        for candidate in factory_candidates {
            self.retain_factory_candidate(candidate);
        }
        self.drain_factory_candidates();
        for repair in pending_repairs {
            self.retain_followup_intent(
                repair.pool().clone(),
                AmmPendingFollowUpIntent::Repair(repair.action().clone()),
            );
        }
        self.drain_followup_intents();
        Ok(published)
    }

    fn publish_commit_status(
        &mut self,
        first_sequence: u64,
        version: AmmStateVersion,
        quality: AmmStateQuality,
        observer_events: Vec<AmmRuntimeEventKind>,
    ) {
        let health = runtime_health(quality);
        self.health = health;
        for (offset, event) in observer_events.into_iter().enumerate() {
            let sequence = first_sequence + offset as u64;
            self.event_sequence = sequence;
            self.status
                .send_replace(Arc::new(AmmRuntimeStatusSnapshot::new(
                    sequence,
                    version,
                    self.lifecycle_snapshot(),
                    self.active_work.clone(),
                    self.queue_depths.clone(),
                    health,
                )));
            let _ = self.observers.send(AmmRuntimeEvent::new(sequence, event));
        }
    }

    fn mark_untrusted(&mut self) {
        self.trusted = false;
        if self.health == AmmRuntimeHealth::Untrusted {
            return;
        }
        let previous = self.health;
        let Some(sequence) = self.event_sequence.checked_add(1) else {
            return;
        };
        self.event_sequence = sequence;
        self.health = AmmRuntimeHealth::Untrusted;
        self.status
            .send_replace(Arc::new(AmmRuntimeStatusSnapshot::new(
                sequence,
                self.version,
                self.lifecycle_snapshot(),
                self.active_work.clone(),
                self.queue_depths.clone(),
                AmmRuntimeHealth::Untrusted,
            )));
        let _ = self.observers.send(AmmRuntimeEvent::new(
            sequence,
            AmmRuntimeEventKind::HealthChanged {
                from: previous,
                to: AmmRuntimeHealth::Untrusted,
            },
        ));
    }

    fn publish_shutting_down(&mut self) {
        if self.health == AmmRuntimeHealth::ShuttingDown {
            return;
        }
        let previous = self.health;
        let Some(sequence) = self.event_sequence.checked_add(1) else {
            self.health = AmmRuntimeHealth::ShuttingDown;
            return;
        };
        self.event_sequence = sequence;
        self.health = AmmRuntimeHealth::ShuttingDown;
        self.status
            .send_replace(Arc::new(AmmRuntimeStatusSnapshot::new(
                sequence,
                self.version,
                self.lifecycle_snapshot(),
                self.active_work.clone(),
                self.queue_depths.clone(),
                AmmRuntimeHealth::ShuttingDown,
            )));
        let _ = self.observers.send(AmmRuntimeEvent::new(
            sequence,
            AmmRuntimeEventKind::HealthChanged {
                from: previous,
                to: AmmRuntimeHealth::ShuttingDown,
            },
        ));
    }
}

const fn runtime_health(quality: AmmStateQuality) -> AmmRuntimeHealth {
    match quality {
        AmmStateQuality::Coherent => AmmRuntimeHealth::Healthy,
        AmmStateQuality::Degraded => AmmRuntimeHealth::Degraded,
        AmmStateQuality::Untrusted => AmmRuntimeHealth::Untrusted,
    }
}

fn evidence_contains_owner(evidence: &RegistrationEvidenceSet, owner: &DiscoveryOwnerId) -> bool {
    evidence.evidence().iter().any(|item| match item {
        RegistrationProvenance::StateQuery { owner: source, .. }
        | RegistrationProvenance::FactoryLog { owner: source, .. } => source == owner,
        RegistrationProvenance::Stable { .. } => false,
    })
}

fn evidence_contains_state_query_owner(
    evidence: &RegistrationEvidenceSet,
    owner: &DiscoveryOwnerId,
) -> bool {
    evidence.evidence().iter().any(|item| {
        matches!(
            item,
            RegistrationProvenance::StateQuery { owner: source, .. } if source == owner
        )
    })
}

fn evidence_without_dropped_hashes(
    evidence: &RegistrationEvidenceSet,
    dropped_hashes: &[alloy_primitives::B256],
) -> Option<RegistrationEvidenceSet> {
    let mut retained = evidence
        .evidence()
        .iter()
        .filter(|item| match item {
            RegistrationProvenance::StateQuery { block_hash, .. }
            | RegistrationProvenance::FactoryLog { block_hash, .. } => {
                !dropped_hashes.contains(block_hash)
            }
            RegistrationProvenance::Stable { .. } => true,
        })
        .cloned()
        .collect::<Vec<_>>();
    if retained.is_empty() {
        return None;
    }
    let primary = retained.remove(0);
    Some(RegistrationEvidenceSet::new(primary, retained))
}

fn evidence_without_owner(
    evidence: &RegistrationEvidenceSet,
    owner: &DiscoveryOwnerId,
) -> Option<RegistrationEvidenceSet> {
    let mut retained = evidence
        .evidence()
        .iter()
        .filter(|item| match item {
            RegistrationProvenance::StateQuery { owner: source, .. }
            | RegistrationProvenance::FactoryLog { owner: source, .. } => source != owner,
            RegistrationProvenance::Stable { .. } => true,
        })
        .cloned()
        .collect::<Vec<_>>();
    if retained.is_empty() {
        None
    } else {
        let primary = retained.remove(0);
        Some(RegistrationEvidenceSet::new(primary, retained))
    }
}

fn committed_change_kind(kind: AmmSyncPoolChangeKind) -> AmmPoolChangeKind {
    match kind {
        AmmSyncPoolChangeKind::Updated => AmmPoolChangeKind::Updated,
        AmmSyncPoolChangeKind::Degraded | AmmSyncPoolChangeKind::UnknownImpact => {
            AmmPoolChangeKind::Degraded
        }
        AmmSyncPoolChangeKind::Recovered => AmmPoolChangeKind::Recovered,
    }
}

fn committed_incident(chain_id: u64, incident: &AmmSyncIncident) -> AmmStateIncident {
    match incident {
        AmmSyncIncident::Reorg { dropped_blocks } => AmmStateIncident::Reorg {
            dropped: dropped_blocks
                .iter()
                .map(|block: &BlockRef| {
                    AmmStatePoint::post_block(chain_id, block.number, block.hash)
                })
                .collect(),
        },
        AmmSyncIncident::Gap { from, to } => AmmStateIncident::Gap {
            from: *from,
            to: *to,
        },
        AmmSyncIncident::CoverageGap { address, block } => AmmStateIncident::CoverageGap {
            address: *address,
            block: *block,
        },
    }
}

fn committed_runtime_incident(chain_id: u64, incident: &AmmSyncIncident) -> AmmRuntimeEventKind {
    match incident {
        AmmSyncIncident::Reorg { dropped_blocks } => AmmRuntimeEventKind::Reorg {
            dropped: dropped_blocks
                .iter()
                .map(|block| AmmStatePoint::post_block(chain_id, block.number, block.hash))
                .collect(),
        },
        AmmSyncIncident::Gap { from, to } => AmmRuntimeEventKind::Gap {
            from: *from,
            to: *to,
        },
        AmmSyncIncident::CoverageGap { address, block } => AmmRuntimeEventKind::CoverageGap {
            address: *address,
            block: *block,
        },
    }
}
