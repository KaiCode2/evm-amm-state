//! Alloy-specific subscriber owner and complete-block driver.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::time::{Duration, Instant};

use alloy_network::{Ethereum, primitives::BlockResponse as _};
use alloy_provider::Provider;
use alloy_rpc_types_eth::{Filter, Header as RpcHeader, Log as RpcLog};
use evm_fork_cache::reactive::{
    AlloySubscriber, BlockInterest, BlockRef, ChainControl, ChainStatus, EventSubscriber,
    HandlerId, InputSource, PreconfirmationMode, ReactiveContext, ReactiveInput,
    ReactiveInputBatch, ReactiveInputRecord, ReactiveInterest, SubscriberDriverPoll,
    SubscriberMode, SubscriberOwnerEpoch, SubscriberOwnerError, SubscriberOwnerStart,
    SubscriberOwnerState,
};
use tokio::sync::{mpsc, oneshot, watch};

use super::{
    AmmCanonicalBatch, AmmCanonicalBatchError, AmmPoolSubscriptionPlan, AmmRuntimeCommandError,
    AmmRuntimeHandle, AmmStatePoint,
};

/// Policy for a data-quality rejection of one disposable preconfirmation.
///
/// Canonical invariants, subscriber transport errors, and runtime lifecycle
/// failures are unaffected by this policy and always remain fatal. Only the
/// typed [`AmmRuntimeCommandError::PreconfirmationBatch`] rejection may be
/// discarded so a `Preferred` preview stream cannot tear down canonical
/// delivery.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AmmPreconfirmationRejectionPolicy {
    /// Preserve the historical fail-closed behavior for required previews.
    #[default]
    FailDriver,
    /// Discard the rejected preview and continue canonical delivery.
    ///
    /// Attachment rejects this policy unless the subscriber explicitly uses
    /// [`PreconfirmationMode::Preferred`].
    ContinueCanonical,
}

/// Where canonical log data comes from.
///
/// The subscriber already delivers every matching log over its subscription.
/// Historically the driver discarded them and re-fetched each block's logs with
/// a hash-pinned `eth_getLogs`, because it could not tell a complete stream from
/// a punctured one. It can now: the subscriber attests through
/// [`ChainControl::LogCoverage`](evm_fork_cache::reactive::ChainControl) that no
/// notification loss went unhealed at or below a block, and any block that is
/// *not* attested still takes the reconciliation path. The fetch stops being
/// unconditional and becomes exact.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CanonicalLogSource {
    /// Assemble blocks from the subscription, reconciling only what is not
    /// attested.
    ///
    /// A block is submitted once its logs are attested complete, its header has
    /// arrived, and its log set is known closed — proven either by an
    /// observation from a strictly later block, or by `grace` elapsing with no
    /// such observation. The grace window is a quiet-tail fallback, not the
    /// steady-state path.
    Subscription {
        /// How long to wait for stragglers before sealing a block whose
        /// successor has not been observed.
        grace: Duration,
    },
    /// Re-fetch every canonical block's logs with a hash-pinned `eth_getLogs`.
    ///
    /// The historical behaviour, retained as a one-line opt-out.
    Reconcile,
}

impl CanonicalLogSource {
    /// Default straggler window for subscription-sourced assembly.
    ///
    /// Chosen to be short relative to any supported block time: the later-block
    /// observation normally seals first, so this bounds only the tail.
    pub const DEFAULT_GRACE: Duration = Duration::from_millis(40);
}

impl Default for CanonicalLogSource {
    fn default() -> Self {
        Self::Subscription {
            grace: Self::DEFAULT_GRACE,
        }
    }
}

/// Configuration for the Alloy subscriber driver.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AmmSubscriberDriverConfig {
    control_capacity: usize,
    max_addresses_per_get_logs: usize,
    preconfirmation_rejection_policy: AmmPreconfirmationRejectionPolicy,
    canonical_log_source: CanonicalLogSource,
}

impl Default for AmmSubscriberDriverConfig {
    fn default() -> Self {
        Self {
            control_capacity: 32,
            max_addresses_per_get_logs: 256,
            preconfirmation_rejection_policy: AmmPreconfirmationRejectionPolicy::FailDriver,
            canonical_log_source: CanonicalLogSource::default(),
        }
    }
}

impl AmmSubscriberDriverConfig {
    /// Set the bounded subscriber-control capacity.
    pub const fn with_control_capacity(mut self, capacity: usize) -> Self {
        self.control_capacity = capacity;
        self
    }

    /// Bounded subscriber-control capacity.
    pub const fn control_capacity(&self) -> usize {
        self.control_capacity
    }

    /// Bound provider address arrays used for hash-pinned block reconciliation.
    pub const fn with_max_addresses_per_get_logs(mut self, maximum: usize) -> Self {
        self.max_addresses_per_get_logs = maximum;
        self
    }

    /// Maximum addresses placed in one hash-pinned `eth_getLogs` request.
    pub const fn max_addresses_per_get_logs(&self) -> usize {
        self.max_addresses_per_get_logs
    }

    /// Select whether a rejected disposable preview terminates this driver.
    pub const fn with_preconfirmation_rejection_policy(
        mut self,
        policy: AmmPreconfirmationRejectionPolicy,
    ) -> Self {
        self.preconfirmation_rejection_policy = policy;
        self
    }

    /// Choose where canonical log data comes from.
    pub const fn with_canonical_log_source(mut self, source: CanonicalLogSource) -> Self {
        self.canonical_log_source = source;
        self
    }

    /// Configured canonical log source.
    pub const fn canonical_log_source(&self) -> CanonicalLogSource {
        self.canonical_log_source
    }

    /// Active rejected-preview policy.
    pub const fn preconfirmation_rejection_policy(&self) -> AmmPreconfirmationRejectionPolicy {
        self.preconfirmation_rejection_policy
    }
}

/// Latest recoverable subscriber-driver state.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum AmmSubscriberDriverState {
    /// Driver exists but canonical delivery is paused behind attach/lifecycle work.
    Paused,
    /// Subscriber and actor are continuously delivering the same interest revision.
    Running {
        /// Interest revision carried by new canonical deliveries.
        interest_revision: u64,
        /// Latest actor point acknowledged by the driver.
        point: AmmStatePoint,
    },
    /// Driver stopped after a typed failure.
    Failed(String),
    /// Driver was explicitly shut down or its control surface was dropped.
    Stopped,
}

/// Error attaching, controlling, or running the subscriber driver.
#[derive(Debug)]
#[non_exhaustive]
pub enum AmmSubscriberDriverError {
    /// A required bounded capacity was zero.
    ZeroControlCapacity,
    /// Stage 4 complete-block delivery requires header-capable pubsub mode.
    UnsupportedMode,
    /// Canonical continuation was paired with a subscriber mode that does not
    /// permit disposable previews.
    IncompatiblePreconfirmationPolicy,
    /// The driver task or its control channel is closed.
    Closed,
    /// The upstream subscriber rejected or failed an operation.
    Subscriber(Box<evm_fork_cache::reactive::SubscriberError>),
    /// An exact subscriber owner lifecycle operation failed.
    Owner(Box<SubscriberOwnerError>),
    /// A provider reconciliation request failed.
    Provider(String),
    /// A required canonical block was unavailable.
    MissingBlock(u64),
    /// A provider-supplied parent did not form an exact descending hash lineage.
    InvalidCanonicalLineage(&'static str),
    /// A replacement branch diverged before the driver's retained canonical history.
    ReorgBeyondRetainedLineage {
        /// Oldest retained canonical block number.
        oldest_retained: u64,
        /// Replacement block whose parent walk crossed the retained boundary.
        replacement: u64,
    },
    /// A provider returned a malformed or cross-block log.
    InvalidCanonicalLog(&'static str),
    /// The actor rejected a complete canonical delivery or lifecycle command.
    Runtime(Box<AmmRuntimeCommandError>),
    /// The complete-block envelope failed validation.
    Canonical(Box<AmmCanonicalBatchError>),
    /// A lifecycle command did not match the driver's paused transaction.
    StaleTransaction,
    /// Another lifecycle transaction already owns the subscriber fence.
    TransactionInProgress,
    /// An expected exact owner generation was absent or in the wrong state.
    OwnerState,
    /// Stage 4 received owner-only catch-up outside its current-point commit seam.
    OwnerCatchupRequiresStaging,
}

impl fmt::Display for AmmSubscriberDriverError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroControlCapacity => write!(f, "subscriber control capacity must be non-zero"),
            Self::UnsupportedMode => write!(
                f,
                "AMM subscriber driver requires Auto or PubSub header delivery"
            ),
            Self::IncompatiblePreconfirmationPolicy => write!(
                f,
                "canonical continuation requires Preferred preconfirmations"
            ),
            Self::Closed => write!(f, "AMM subscriber driver is closed"),
            Self::Subscriber(error) => write!(f, "AMM subscriber failed: {error}"),
            Self::Owner(error) => write!(f, "AMM subscriber owner failed: {error}"),
            Self::Provider(error) => write!(f, "AMM subscriber reconciliation failed: {error}"),
            Self::MissingBlock(block) => {
                write!(
                    f,
                    "canonical block {block} was unavailable during reconciliation"
                )
            }
            Self::InvalidCanonicalLineage(message) => {
                write!(f, "invalid canonical block lineage: {message}")
            }
            Self::ReorgBeyondRetainedLineage {
                oldest_retained,
                replacement,
            } => write!(
                f,
                "replacement block {replacement} diverged before retained canonical block {oldest_retained}"
            ),
            Self::InvalidCanonicalLog(message) => {
                write!(f, "invalid canonical reconciliation log: {message}")
            }
            Self::Runtime(error) => write!(f, "AMM runtime rejected subscriber work: {error}"),
            Self::Canonical(error) => write!(f, "AMM canonical delivery failed: {error}"),
            Self::StaleTransaction => write!(f, "stale AMM subscriber transaction"),
            Self::TransactionInProgress => {
                write!(f, "an AMM subscriber transaction is already in progress")
            }
            Self::OwnerState => write!(f, "AMM subscriber owner is absent or in the wrong state"),
            Self::OwnerCatchupRequiresStaging => write!(
                f,
                "owner-only catch-up requires the progressive staging scheduler"
            ),
        }
    }
}

impl std::error::Error for AmmSubscriberDriverError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Subscriber(error) => Some(error.as_ref()),
            Self::Owner(error) => Some(error.as_ref()),
            Self::Runtime(error) => Some(error.as_ref()),
            Self::Canonical(error) => Some(error.as_ref()),
            _ => None,
        }
    }
}

impl From<evm_fork_cache::reactive::SubscriberError> for AmmSubscriberDriverError {
    fn from(error: evm_fork_cache::reactive::SubscriberError) -> Self {
        Self::Subscriber(Box::new(error))
    }
}

impl From<SubscriberOwnerError> for AmmSubscriberDriverError {
    fn from(error: SubscriberOwnerError) -> Self {
        Self::Owner(Box::new(error))
    }
}

impl From<AmmRuntimeCommandError> for AmmSubscriberDriverError {
    fn from(error: AmmRuntimeCommandError) -> Self {
        Self::Runtime(Box::new(error))
    }
}

impl From<AmmCanonicalBatchError> for AmmSubscriberDriverError {
    fn from(error: AmmCanonicalBatchError) -> Self {
        Self::Canonical(Box::new(error))
    }
}

/// Canonical-delivery accounting for one attached subscriber driver.
///
/// The driver is the layer that decides *where* canonical log data comes from,
/// so this is where the cost of that decision is visible. Today every canonical
/// header triggers a hash-pinned `eth_getLogs` reconciliation while the logs the
/// WebSocket already delivered are discarded; `subscription_logs_discarded`
/// beside `reconciliation_requests` reports exactly that, from inside the
/// process, without reading a provider invoice.
///
/// Counts are cumulative for the driver task's lifetime.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AmmSubscriberDriverStats {
    canonical_headers_ingested: u64,
    canonical_blocks_delivered: u64,
    reconciliation_requests: u64,
    reconciliation_logs_fetched: u64,
    subscription_logs_discarded: u64,
    subscription_logs_applied: u64,
    blocks_sealed_from_subscription: u64,
    blocks_sealed_by_successor: u64,
    blocks_sealed_by_bloom_absence: u64,
    blocks_reconciled: u64,
    lineage_parent_requests: u64,
}

impl AmmSubscriberDriverStats {
    /// Canonical headers accepted from the subscriber, before lineage expansion
    /// and before the no-op check against the runtime's current point.
    pub const fn canonical_headers_ingested(self) -> u64 {
        self.canonical_headers_ingested
    }

    /// Canonical blocks reconciled and submitted to the runtime. Exceeds
    /// [`canonical_headers_ingested`](Self::canonical_headers_ingested) when a
    /// reorg or header gap expands one header into a lineage.
    pub const fn canonical_blocks_delivered(self) -> u64 {
        self.canonical_blocks_delivered
    }

    /// Hash-pinned `eth_getLogs` requests issued for canonical reconciliation.
    ///
    /// One per address-chunked filter per delivered block, so this scales with
    /// block rate times
    /// [`max_addresses_per_get_logs`](AmmSubscriberDriverConfig::max_addresses_per_get_logs)
    /// chunks — not with pool activity.
    pub const fn reconciliation_requests(self) -> u64 {
        self.reconciliation_requests
    }

    /// Logs returned by those reconciliation requests.
    pub const fn reconciliation_logs_fetched(self) -> u64 {
        self.reconciliation_logs_fetched
    }

    /// Canonical logs delivered over the subscriber's live stream and then
    /// dropped, because canonical log data is sourced from reconciliation
    /// instead. Every one of these was already paid for.
    ///
    /// Under [`CanonicalLogSource::Subscription`] this counts only logs a
    /// reconciled block superseded; a steadily climbing value means blocks are
    /// failing to seal from the stream and the fetch is not actually being
    /// avoided.
    pub const fn subscription_logs_discarded(self) -> u64 {
        self.subscription_logs_discarded
    }

    /// Canonical logs applied straight from the subscription, costing no
    /// provider request.
    pub const fn subscription_logs_applied(self) -> u64 {
        self.subscription_logs_applied
    }

    /// Blocks assembled from the subscription instead of being re-fetched.
    pub const fn blocks_sealed_from_subscription(self) -> u64 {
        self.blocks_sealed_from_subscription
    }

    /// Blocks sealed because a strictly later block was observed, proving the
    /// earlier block's log set closed. The steady-state path.
    pub const fn blocks_sealed_by_successor(self) -> u64 {
        self.blocks_sealed_by_successor
    }

    /// Blocks sealed because the grace window elapsed with no successor
    /// observed. Expected only on a quiet tail; a high proportion here means
    /// the window is doing work the successor rule should be doing.
    pub const fn blocks_sealed_by_bloom_absence(self) -> u64 {
        self.blocks_sealed_by_bloom_absence
    }

    /// Blocks that took the hash-pinned reconciliation path because their logs
    /// were not attested complete.
    pub const fn blocks_reconciled(self) -> u64 {
        self.blocks_reconciled
    }

    /// `eth_getBlockByHash` requests walking a replacement branch back to
    /// retained canonical lineage. Non-zero only on a reorg or header gap.
    pub const fn lineage_parent_requests(self) -> u64 {
        self.lineage_parent_requests
    }
}

/// Interior-mutable counters behind [`AmmSubscriberDriverStats`].
///
/// Shared with every [`AmmSubscriberDriverHandle`] clone so a caller can read
/// the driver's accounting without a round trip through its control channel.
#[derive(Debug, Default)]
struct AmmSubscriberDriverCounters {
    canonical_headers_ingested: AtomicU64,
    canonical_blocks_delivered: AtomicU64,
    reconciliation_requests: AtomicU64,
    reconciliation_logs_fetched: AtomicU64,
    subscription_logs_discarded: AtomicU64,
    subscription_logs_applied: AtomicU64,
    blocks_sealed_from_subscription: AtomicU64,
    blocks_sealed_by_successor: AtomicU64,
    blocks_sealed_by_bloom_absence: AtomicU64,
    blocks_reconciled: AtomicU64,
    lineage_parent_requests: AtomicU64,
}

impl AmmSubscriberDriverCounters {
    fn add(counter: &AtomicU64, amount: u64) {
        counter.fetch_add(amount, AtomicOrdering::Relaxed);
    }

    fn snapshot(&self) -> AmmSubscriberDriverStats {
        let load = |counter: &AtomicU64| counter.load(AtomicOrdering::Relaxed);
        AmmSubscriberDriverStats {
            canonical_headers_ingested: load(&self.canonical_headers_ingested),
            canonical_blocks_delivered: load(&self.canonical_blocks_delivered),
            reconciliation_requests: load(&self.reconciliation_requests),
            reconciliation_logs_fetched: load(&self.reconciliation_logs_fetched),
            subscription_logs_discarded: load(&self.subscription_logs_discarded),
            subscription_logs_applied: load(&self.subscription_logs_applied),
            blocks_sealed_from_subscription: load(&self.blocks_sealed_from_subscription),
            blocks_sealed_by_successor: load(&self.blocks_sealed_by_successor),
            blocks_sealed_by_bloom_absence: load(&self.blocks_sealed_by_bloom_absence),
            blocks_reconciled: load(&self.blocks_reconciled),
            lineage_parent_requests: load(&self.lineage_parent_requests),
        }
    }
}

/// Public status/shutdown handle for an attached subscriber task.
#[derive(Clone)]
pub struct AmmSubscriberDriverHandle {
    control: AmmSubscriberControl,
    state: watch::Receiver<AmmSubscriberDriverState>,
    stats: Arc<AmmSubscriberDriverCounters>,
}

impl AmmSubscriberDriverHandle {
    /// Latest driver state without replaying diagnostic events.
    pub fn latest_state(&self) -> AmmSubscriberDriverState {
        self.state.borrow().clone()
    }

    /// Subscribe to recoverable latest-value driver state changes.
    pub fn subscribe_state(&self) -> watch::Receiver<AmmSubscriberDriverState> {
        self.state.clone()
    }

    /// Canonical-delivery accounting for the attached driver, including the
    /// provider requests it issues on its own initiative.
    pub fn stats(&self) -> AmmSubscriberDriverStats {
        self.stats.snapshot()
    }

    /// Subscribe first, then reconcile every canonical block through `header`.
    ///
    /// This is the warm-resume boundary: the driver validates parent lineage,
    /// fetches exact block-hash logs for current interests, and advances the
    /// runtime one block at a time before acknowledging completion.
    pub async fn catch_up_to(&self, header: RpcHeader) -> Result<(), AmmSubscriberDriverError> {
        self.control
            .request(|response| SubscriberControlCommand::CatchUp {
                header: Box::new(header),
                response,
            })
            .await
    }

    /// Stop subscriber delivery. Runtime shutdown remains a separate operation.
    pub async fn shutdown(&self) -> Result<(), AmmSubscriberDriverError> {
        self.control.shutdown(true).await
    }
}

impl AmmRuntimeHandle {
    /// Attach one Alloy subscriber and transactionally adopt all active pool owners.
    ///
    /// The subscriber must not already contain transaction-aware owner epochs;
    /// its existing base interests are preserved and a canonical header interest
    /// is added automatically.
    pub async fn attach_alloy_subscriber<P>(
        &self,
        subscriber: AlloySubscriber<P, Ethereum>,
        config: AmmSubscriberDriverConfig,
    ) -> Result<AmmSubscriberDriverHandle, AmmSubscriberDriverError>
    where
        P: Provider<Ethereum> + Clone + Send + Sync + 'static,
    {
        let (control, handle) = spawn_alloy_subscriber(self.clone(), subscriber, config)?;
        if let Err(error) = self.attach_subscriber_control(control).await {
            let _ = handle.control.shutdown(false).await;
            return Err(error.into());
        }
        Ok(handle)
    }
}

#[derive(Clone)]
pub(crate) struct AmmSubscriberControl {
    commands: mpsc::Sender<SubscriberControlCommand>,
}

/// Generation-agnostic subscriber ownership payload used by lifecycle fences.
///
/// Pool additions project their existing public plan into this type. Discovery
/// watchers can use the same transaction machinery without pretending to be a
/// pool or exposing subscriber internals publicly.
#[derive(Clone)]
pub(crate) struct AmmSubscriberOwnerPlan {
    owner: HandlerId,
    interests: Vec<ReactiveInterest<Ethereum>>,
}

impl AmmSubscriberOwnerPlan {
    pub(crate) const fn new(owner: HandlerId, interests: Vec<ReactiveInterest<Ethereum>>) -> Self {
        Self { owner, interests }
    }

    pub(crate) const fn owner(&self) -> &HandlerId {
        &self.owner
    }

    pub(crate) fn interests(&self) -> &[ReactiveInterest<Ethereum>] {
        &self.interests
    }
}

impl From<AmmPoolSubscriptionPlan> for AmmSubscriberOwnerPlan {
    fn from(plan: AmmPoolSubscriptionPlan) -> Self {
        Self::new(plan.handler().clone(), plan.interests().to_vec())
    }
}

impl AmmSubscriberControl {
    async fn request<T>(
        &self,
        command: impl FnOnce(
            oneshot::Sender<Result<T, AmmSubscriberDriverError>>,
        ) -> SubscriberControlCommand,
    ) -> Result<T, AmmSubscriberDriverError> {
        let (response, result) = oneshot::channel();
        self.commands
            .send(command(response))
            .await
            .map_err(|_| AmmSubscriberDriverError::Closed)?;
        result.await.map_err(|_| AmmSubscriberDriverError::Closed)?
    }

    /// Adopt arbitrary generation-scoped owners in the initial atomic
    /// subscriber transaction.
    pub(crate) async fn adopt_existing_owners(
        &self,
        plans: Vec<AmmSubscriberOwnerPlan>,
        point: AmmStatePoint,
        interest_revision: u64,
    ) -> Result<(), AmmSubscriberDriverError> {
        self.request(|response| SubscriberControlCommand::AdoptExisting {
            plans,
            point,
            interest_revision,
            response,
        })
        .await
    }

    pub(crate) async fn begin_add(
        &self,
        plans: Vec<AmmPoolSubscriptionPlan>,
        point: AmmStatePoint,
    ) -> Result<SubscriberTransaction, AmmSubscriberDriverError> {
        self.begin_add_owners(plans.into_iter().map(Into::into).collect(), point)
            .await
    }

    /// Stage arbitrary generation-scoped owners through the same atomic fence
    /// used by pool additions.
    pub(crate) async fn begin_add_owners(
        &self,
        plans: Vec<AmmSubscriberOwnerPlan>,
        point: AmmStatePoint,
    ) -> Result<SubscriberTransaction, AmmSubscriberDriverError> {
        self.request(|response| SubscriberControlCommand::BeginAdd {
            plans,
            point,
            response,
        })
        .await
    }

    pub(crate) async fn begin_remove(
        &self,
        owners: Vec<HandlerId>,
    ) -> Result<SubscriberTransaction, AmmSubscriberDriverError> {
        self.request(|response| SubscriberControlCommand::BeginRemove { owners, response })
            .await
    }

    pub(crate) async fn begin_replace(
        &self,
        plan: AmmSubscriberOwnerPlan,
        point: AmmStatePoint,
    ) -> Result<SubscriberTransaction, AmmSubscriberDriverError> {
        self.request(|response| SubscriberControlCommand::BeginReplace {
            plan,
            point,
            response,
        })
        .await
    }

    pub(crate) async fn commit(
        &self,
        transaction: SubscriberTransaction,
        interest_revision: u64,
        point: AmmStatePoint,
    ) -> Result<(), AmmSubscriberDriverError> {
        self.request(|response| SubscriberControlCommand::Commit {
            transaction,
            interest_revision,
            point,
            response,
        })
        .await
    }

    pub(crate) async fn abort(
        &self,
        transaction: SubscriberTransaction,
    ) -> Result<(), AmmSubscriberDriverError> {
        self.request(|response| SubscriberControlCommand::Abort {
            transaction,
            response,
        })
        .await
    }

    async fn shutdown(&self, report_loss: bool) -> Result<(), AmmSubscriberDriverError> {
        self.request(|response| SubscriberControlCommand::Shutdown {
            report_loss,
            response,
        })
        .await
    }

    pub(crate) fn shutdown_for_runtime(&self) {
        let (response, _result) = oneshot::channel();
        let command = SubscriberControlCommand::Shutdown {
            report_loss: false,
            response,
        };
        match self.commands.try_send(command) {
            Ok(()) | Err(mpsc::error::TrySendError::Closed(_)) => {}
            Err(mpsc::error::TrySendError::Full(command)) => {
                let commands = self.commands.clone();
                tokio::spawn(async move {
                    let _ = commands.send(command).await;
                });
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SubscriberTransaction(u64);

enum SubscriberControlCommand {
    AdoptExisting {
        plans: Vec<AmmSubscriberOwnerPlan>,
        point: AmmStatePoint,
        interest_revision: u64,
        response: oneshot::Sender<Result<(), AmmSubscriberDriverError>>,
    },
    BeginAdd {
        plans: Vec<AmmSubscriberOwnerPlan>,
        point: AmmStatePoint,
        response: oneshot::Sender<Result<SubscriberTransaction, AmmSubscriberDriverError>>,
    },
    BeginRemove {
        owners: Vec<HandlerId>,
        response: oneshot::Sender<Result<SubscriberTransaction, AmmSubscriberDriverError>>,
    },
    BeginReplace {
        plan: AmmSubscriberOwnerPlan,
        point: AmmStatePoint,
        response: oneshot::Sender<Result<SubscriberTransaction, AmmSubscriberDriverError>>,
    },
    Commit {
        transaction: SubscriberTransaction,
        interest_revision: u64,
        point: AmmStatePoint,
        response: oneshot::Sender<Result<(), AmmSubscriberDriverError>>,
    },
    Abort {
        transaction: SubscriberTransaction,
        response: oneshot::Sender<Result<(), AmmSubscriberDriverError>>,
    },
    CatchUp {
        header: Box<RpcHeader>,
        response: oneshot::Sender<Result<(), AmmSubscriberDriverError>>,
    },
    Shutdown {
        report_loss: bool,
        response: oneshot::Sender<Result<(), AmmSubscriberDriverError>>,
    },
}

enum PendingSubscriberTransaction {
    Add {
        id: SubscriberTransaction,
        epochs: Vec<SubscriberOwnerEpoch>,
    },
    Remove {
        id: SubscriberTransaction,
        epochs: Vec<SubscriberOwnerEpoch>,
    },
    Replace {
        id: SubscriberTransaction,
        active: SubscriberOwnerEpoch,
        replacement: SubscriberOwnerEpoch,
    },
}

impl PendingSubscriberTransaction {
    const fn id(&self) -> SubscriberTransaction {
        match self {
            Self::Add { id, .. } | Self::Remove { id, .. } => *id,
            Self::Replace { id, .. } => *id,
        }
    }
}

pub(crate) fn spawn_alloy_subscriber<P>(
    runtime: AmmRuntimeHandle,
    subscriber: AlloySubscriber<P, Ethereum>,
    config: AmmSubscriberDriverConfig,
) -> Result<(AmmSubscriberControl, AmmSubscriberDriverHandle), AmmSubscriberDriverError>
where
    P: Provider<Ethereum> + Clone + Send + Sync + 'static,
{
    if config.control_capacity == 0 || config.max_addresses_per_get_logs == 0 {
        return Err(AmmSubscriberDriverError::ZeroControlCapacity);
    }
    if subscriber.mode() == SubscriberMode::Polling {
        return Err(AmmSubscriberDriverError::UnsupportedMode);
    }
    if config.preconfirmation_rejection_policy
        == AmmPreconfirmationRejectionPolicy::ContinueCanonical
        && subscriber.config().preconfirmations != PreconfirmationMode::Preferred
    {
        return Err(AmmSubscriberDriverError::IncompatiblePreconfirmationPolicy);
    }
    let mut base_interests = subscriber.registered_interests().to_vec();
    if !base_interests
        .iter()
        .any(|interest| matches!(interest, ReactiveInterest::Blocks(_)))
    {
        base_interests.push(ReactiveInterest::Blocks(BlockInterest::default()));
    }
    let (command_tx, command_rx) = mpsc::channel(config.control_capacity);
    let (state_tx, state_rx) = watch::channel(AmmSubscriberDriverState::Paused);
    let stats = Arc::new(AmmSubscriberDriverCounters::default());
    let canonical_lineage = initial_canonical_lineage(runtime.latest_snapshot().point());
    let control = AmmSubscriberControl {
        commands: command_tx,
    };
    let actor = AlloyAmmSubscriberDriver {
        runtime,
        subscriber,
        initial_interests: base_interests,
        commands: command_rx,
        state: state_tx,
        paused: true,
        interest_revision: 0,
        owners: HashMap::new(),
        pending: None,
        next_transaction: 0,
        max_addresses_per_get_logs: config.max_addresses_per_get_logs,
        preconfirmation_rejection_policy: config.preconfirmation_rejection_policy,
        report_stop: true,
        stop_requested: false,
        stats: Arc::clone(&stats),
        canonical_lineage,
        canonical_log_source: config.canonical_log_source,
        attested_log_coverage: None,
        buffered_logs: BTreeMap::new(),
        pending_headers: BTreeMap::new(),
        highest_observed_block: None,
    };
    tokio::spawn(actor.run());
    Ok((
        control.clone(),
        AmmSubscriberDriverHandle {
            control,
            state: state_rx,
            stats,
        },
    ))
}

struct AlloyAmmSubscriberDriver<P> {
    runtime: AmmRuntimeHandle,
    subscriber: AlloySubscriber<P, Ethereum>,
    initial_interests: Vec<ReactiveInterest<Ethereum>>,
    commands: mpsc::Receiver<SubscriberControlCommand>,
    state: watch::Sender<AmmSubscriberDriverState>,
    paused: bool,
    interest_revision: u64,
    owners: HashMap<HandlerId, SubscriberOwnerEpoch>,
    pending: Option<PendingSubscriberTransaction>,
    next_transaction: u64,
    max_addresses_per_get_logs: usize,
    preconfirmation_rejection_policy: AmmPreconfirmationRejectionPolicy,
    report_stop: bool,
    stop_requested: bool,
    stats: Arc<AmmSubscriberDriverCounters>,
    canonical_lineage: BTreeMap<u64, alloy_primitives::B256>,
    canonical_log_source: CanonicalLogSource,
    /// Highest block the subscriber has attested carries no unhealed log loss.
    /// Reset on any reorg, because the watermark describes the branch that was
    /// canonical when it was issued.
    attested_log_coverage: Option<u64>,
    /// Logs delivered by the subscription, keyed by their block identity and
    /// then by global log index so ordering is restored locally.
    buffered_logs: BTreeMap<(u64, alloy_primitives::B256), BTreeMap<u64, RpcLog>>,
    /// Canonical headers waiting for their log set to be provably closed, with
    /// the instant each started waiting.
    pending_headers: BTreeMap<u64, (RpcHeader, Instant)>,
    /// Highest block number observed from any log or header. An observation
    /// strictly above a pending block proves that block's log set is closed.
    highest_observed_block: Option<u64>,
}

const RETAINED_CANONICAL_LINEAGE: usize = 65;

fn initial_canonical_lineage(point: AmmStatePoint) -> BTreeMap<u64, alloy_primitives::B256> {
    BTreeMap::from([(point.block_number(), point.block_hash())])
}

fn handle_preconfirmation_result<T>(
    policy: AmmPreconfirmationRejectionPolicy,
    result: Result<T, AmmRuntimeCommandError>,
) -> Result<(), AmmSubscriberDriverError> {
    match result {
        Ok(_) => Ok(()),
        Err(AmmRuntimeCommandError::PreconfirmationBatch(_))
            if policy == AmmPreconfirmationRejectionPolicy::ContinueCanonical =>
        {
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

impl<P> AlloyAmmSubscriberDriver<P>
where
    P: Provider<Ethereum> + Clone + Send + Sync + 'static,
{
    async fn run(mut self) {
        let result = match self
            .subscriber
            .register_interests(&self.initial_interests)
            .await
        {
            Ok(()) => self.run_inner().await,
            Err(error) => Err(error.into()),
        };
        let message = match result {
            Err(error) => {
                let message = error.to_string();
                self.state
                    .send_replace(AmmSubscriberDriverState::Failed(message.clone()));
                Some(message)
            }
            Ok(()) if self.report_stop && !self.runtime.shutdown_requested() => {
                Some("AMM subscriber driver stopped before runtime shutdown".to_owned())
            }
            Ok(()) => None,
        };
        if let Some(message) = message {
            let runtime = self.runtime.clone();
            tokio::spawn(async move {
                let _ = runtime.report_subscriber_failure(message).await;
            });
        }
    }

    async fn run_inner(&mut self) -> Result<(), AmmSubscriberDriverError> {
        loop {
            if self.stop_requested {
                self.state.send_replace(AmmSubscriberDriverState::Stopped);
                return Ok(());
            }
            if self.paused {
                let Some(command) = self.commands.recv().await else {
                    self.state.send_replace(AmmSubscriberDriverState::Stopped);
                    return Ok(());
                };
                if self.handle_control(command).await? {
                    self.state.send_replace(AmmSubscriberDriverState::Stopped);
                    return Ok(());
                }
                continue;
            }

            // A block waiting on stragglers needs the loop to wake when its
            // grace window expires, not only when the next batch arrives.
            // `next_batch` is documented cancellation-safe — dropping it while
            // pending must not discard a complete input — so racing it against
            // a deadline cannot lose delivery.
            let outcome = match self.next_seal_deadline() {
                Some(deadline) => {
                    let mut control = Box::pin(self.commands.recv());
                    let polled = tokio::time::timeout_at(
                        deadline.into(),
                        self.subscriber.next_scoped_batch_or(control.as_mut()),
                    )
                    .await;
                    drop(control);
                    match polled {
                        Ok(outcome) => outcome?,
                        Err(_elapsed) => {
                            let now = Instant::now();
                            self.drain_sealable(now, Duration::ZERO).await?;
                            continue;
                        }
                    }
                }
                None => {
                    let mut control = Box::pin(self.commands.recv());
                    let outcome = self
                        .subscriber
                        .next_scoped_batch_or(control.as_mut())
                        .await?;
                    drop(control);
                    outcome
                }
            };
            match outcome {
                SubscriberDriverPoll::Control(Some(command)) => {
                    if self.handle_control(command).await? {
                        self.state.send_replace(AmmSubscriberDriverState::Stopped);
                        return Ok(());
                    }
                }
                SubscriberDriverPoll::Control(None) => {
                    self.state.send_replace(AmmSubscriberDriverState::Stopped);
                    return Ok(());
                }
                SubscriberDriverPoll::Batch(Some(batch)) => self.handle_batch(batch).await?,
                SubscriberDriverPoll::Batch(None) => {
                    self.state.send_replace(AmmSubscriberDriverState::Stopped);
                    return Ok(());
                }
                _ => return Err(AmmSubscriberDriverError::Closed),
            }
        }
    }

    async fn handle_control(
        &mut self,
        command: SubscriberControlCommand,
    ) -> Result<bool, AmmSubscriberDriverError> {
        match command {
            SubscriberControlCommand::AdoptExisting {
                plans,
                point,
                interest_revision,
                response,
            } => {
                let result = self.adopt_existing(plans, point, interest_revision).await;
                let _ = response.send(result);
            }
            SubscriberControlCommand::BeginAdd {
                plans,
                point,
                response,
            } => {
                let result = self.begin_add(plans, point).await;
                let _ = response.send(result);
            }
            SubscriberControlCommand::BeginRemove { owners, response } => {
                let result = self.begin_remove(owners);
                let _ = response.send(result);
            }
            SubscriberControlCommand::BeginReplace {
                plan,
                point,
                response,
            } => {
                let result = self.begin_replace(plan, point).await;
                let _ = response.send(result);
            }
            SubscriberControlCommand::Commit {
                transaction,
                interest_revision,
                point,
                response,
            } => {
                let result = self.commit(transaction, interest_revision, point);
                let _ = response.send(result);
            }
            SubscriberControlCommand::Abort {
                transaction,
                response,
            } => {
                let result = self.abort(transaction);
                let _ = response.send(result);
            }
            SubscriberControlCommand::CatchUp { header, response } => {
                // `deliver_through` services control commands while committing
                // each block, so box this edge to keep the recursive control
                // future finite-sized.
                let result = Box::pin(self.deliver_through(*header)).await;
                let _ = response.send(result);
            }
            SubscriberControlCommand::Shutdown {
                report_loss,
                response,
            } => {
                self.report_stop = report_loss;
                self.stop_requested = true;
                let _ = response.send(Ok(()));
                return Ok(true);
            }
        }
        Ok(false)
    }

    async fn adopt_existing(
        &mut self,
        plans: Vec<AmmSubscriberOwnerPlan>,
        point: AmmStatePoint,
        interest_revision: u64,
    ) -> Result<(), AmmSubscriberDriverError> {
        if self.pending.is_some() || !self.owners.is_empty() {
            return Err(AmmSubscriberDriverError::TransactionInProgress);
        }
        let epochs = self.stage_and_reconcile(&plans, point).await?;
        if !epochs.iter().all(|epoch| {
            self.subscriber.interest_owner_state(epoch) == Some(SubscriberOwnerState::Staged)
        }) {
            self.abort_staged(&epochs);
            return Err(AmmSubscriberDriverError::OwnerState);
        }
        for (plan, epoch) in plans.iter().zip(&epochs) {
            if !self.subscriber.activate_interest_owner(epoch) {
                self.abort_staged(&epochs);
                return Err(AmmSubscriberDriverError::OwnerState);
            }
            self.owners.insert(plan.owner().clone(), epoch.clone());
        }
        self.interest_revision = interest_revision;
        self.paused = false;
        self.publish_running(point);
        Ok(())
    }

    async fn begin_add(
        &mut self,
        plans: Vec<AmmSubscriberOwnerPlan>,
        point: AmmStatePoint,
    ) -> Result<SubscriberTransaction, AmmSubscriberDriverError> {
        if self.pending.is_some() {
            return Err(AmmSubscriberDriverError::TransactionInProgress);
        }
        self.paused = true;
        self.state.send_replace(AmmSubscriberDriverState::Paused);
        let epochs = match self.stage_and_reconcile(&plans, point).await {
            Ok(epochs) => epochs,
            Err(error) => {
                self.paused = false;
                self.publish_running(self.runtime.latest_snapshot().point());
                return Err(error);
            }
        };
        let id = self.allocate_transaction()?;
        self.pending = Some(PendingSubscriberTransaction::Add { id, epochs });
        Ok(id)
    }

    fn begin_remove(
        &mut self,
        owners: Vec<HandlerId>,
    ) -> Result<SubscriberTransaction, AmmSubscriberDriverError> {
        if self.pending.is_some() {
            return Err(AmmSubscriberDriverError::TransactionInProgress);
        }
        let epochs: Vec<_> = owners
            .iter()
            .map(|owner| self.owners.get(owner).cloned())
            .collect::<Option<_>>()
            .ok_or(AmmSubscriberDriverError::OwnerState)?;
        if !epochs.iter().all(|epoch| {
            self.subscriber.interest_owner_state(epoch) == Some(SubscriberOwnerState::Active)
        }) {
            return Err(AmmSubscriberDriverError::OwnerState);
        }
        self.paused = true;
        self.state.send_replace(AmmSubscriberDriverState::Paused);
        for epoch in &epochs {
            if !self.subscriber.prepare_interest_owner_removal(epoch) {
                for prepared in &epochs {
                    let _ = self.subscriber.abort_interest_owner(prepared);
                }
                self.paused = false;
                self.publish_running(self.runtime.latest_snapshot().point());
                return Err(AmmSubscriberDriverError::OwnerState);
            }
        }
        let id = self.allocate_transaction()?;
        self.pending = Some(PendingSubscriberTransaction::Remove { id, epochs });
        Ok(id)
    }

    async fn begin_replace(
        &mut self,
        plan: AmmSubscriberOwnerPlan,
        point: AmmStatePoint,
    ) -> Result<SubscriberTransaction, AmmSubscriberDriverError> {
        if self.pending.is_some() {
            return Err(AmmSubscriberDriverError::TransactionInProgress);
        }
        let active = self
            .owners
            .get(plan.owner())
            .cloned()
            .ok_or(AmmSubscriberDriverError::OwnerState)?;
        self.paused = true;
        self.state.send_replace(AmmSubscriberDriverState::Paused);
        let block = state_point_block(point);
        let replacement = match self.subscriber.stage_interest_owner_replacement(
            plan.owner().clone(),
            plan.interests(),
            SubscriberOwnerStart::PostBlock(block),
        ) {
            Ok(epoch) => epoch,
            Err(error) => {
                self.paused = false;
                self.publish_running(self.runtime.latest_snapshot().point());
                return Err(error.into());
            }
        };
        if let Err(error) = self
            .subscriber
            .reconcile_interest_owners(std::slice::from_ref(&replacement), block)
            .await
        {
            let _ = self.subscriber.abort_interest_owner(&replacement);
            self.paused = false;
            self.publish_running(self.runtime.latest_snapshot().point());
            return Err(error.into());
        }
        let id = self.allocate_transaction()?;
        self.pending = Some(PendingSubscriberTransaction::Replace {
            id,
            active,
            replacement,
        });
        Ok(id)
    }

    fn commit(
        &mut self,
        transaction: SubscriberTransaction,
        interest_revision: u64,
        point: AmmStatePoint,
    ) -> Result<(), AmmSubscriberDriverError> {
        let pending = self
            .pending
            .take()
            .ok_or(AmmSubscriberDriverError::StaleTransaction)?;
        if pending.id() != transaction {
            self.pending = Some(pending);
            return Err(AmmSubscriberDriverError::StaleTransaction);
        }
        match pending {
            PendingSubscriberTransaction::Add { epochs, .. } => {
                if !epochs.iter().all(|epoch| {
                    self.subscriber.interest_owner_state(epoch)
                        == Some(SubscriberOwnerState::Staged)
                }) {
                    self.pending = Some(PendingSubscriberTransaction::Add {
                        id: transaction,
                        epochs,
                    });
                    return Err(AmmSubscriberDriverError::OwnerState);
                }
                for epoch in epochs {
                    if !self.subscriber.activate_interest_owner(&epoch) {
                        return Err(AmmSubscriberDriverError::OwnerState);
                    }
                    self.owners.insert(epoch.owner().clone(), epoch);
                }
            }
            PendingSubscriberTransaction::Remove { epochs, .. } => {
                if !epochs.iter().all(|epoch| {
                    self.subscriber.interest_owner_state(epoch)
                        == Some(SubscriberOwnerState::Removing)
                }) {
                    self.pending = Some(PendingSubscriberTransaction::Remove {
                        id: transaction,
                        epochs,
                    });
                    return Err(AmmSubscriberDriverError::OwnerState);
                }
                for epoch in epochs {
                    self.subscriber
                        .finalize_interest_owner_removal(&epoch)
                        .ok_or(AmmSubscriberDriverError::OwnerState)?;
                    self.owners.remove(epoch.owner());
                }
            }
            PendingSubscriberTransaction::Replace {
                active,
                replacement,
                ..
            } => {
                if !self
                    .subscriber
                    .commit_interest_owner_replacement(&active, &replacement)
                {
                    self.pending = Some(PendingSubscriberTransaction::Replace {
                        id: transaction,
                        active,
                        replacement,
                    });
                    return Err(AmmSubscriberDriverError::OwnerState);
                }
                self.owners.insert(replacement.owner().clone(), replacement);
            }
        }
        self.interest_revision = interest_revision;
        self.paused = false;
        self.publish_running(point);
        Ok(())
    }

    fn abort(
        &mut self,
        transaction: SubscriberTransaction,
    ) -> Result<(), AmmSubscriberDriverError> {
        let pending = self
            .pending
            .take()
            .ok_or(AmmSubscriberDriverError::StaleTransaction)?;
        if pending.id() != transaction {
            self.pending = Some(pending);
            return Err(AmmSubscriberDriverError::StaleTransaction);
        }
        match pending {
            PendingSubscriberTransaction::Add { epochs, .. } => self.abort_staged(&epochs),
            PendingSubscriberTransaction::Remove { epochs, .. } => {
                for epoch in epochs {
                    let _ = self.subscriber.abort_interest_owner(&epoch);
                }
            }
            PendingSubscriberTransaction::Replace { replacement, .. } => {
                let _ = self.subscriber.abort_interest_owner(&replacement);
            }
        }
        self.paused = false;
        self.publish_running(self.runtime.latest_snapshot().point());
        Ok(())
    }

    async fn stage_and_reconcile(
        &mut self,
        plans: &[AmmSubscriberOwnerPlan],
        point: AmmStatePoint,
    ) -> Result<Vec<SubscriberOwnerEpoch>, AmmSubscriberDriverError> {
        let block = state_point_block(point);
        let mut epochs = Vec::with_capacity(plans.len());
        for plan in plans {
            let epoch = match self.subscriber.stage_interest_owner(
                plan.owner().clone(),
                plan.interests(),
                SubscriberOwnerStart::PostBlock(block),
            ) {
                Ok(epoch) => epoch,
                Err(error) => {
                    self.abort_staged(&epochs);
                    return Err(error.into());
                }
            };
            epochs.push(epoch);
        }
        if let Err(error) = self
            .subscriber
            .reconcile_interest_owners(&epochs, block)
            .await
        {
            self.abort_staged(&epochs);
            return Err(error.into());
        }
        Ok(epochs)
    }

    fn abort_staged(&mut self, epochs: &[SubscriberOwnerEpoch]) {
        for epoch in epochs {
            let _ = self.subscriber.abort_interest_owner(epoch);
        }
    }

    fn allocate_transaction(&mut self) -> Result<SubscriberTransaction, AmmSubscriberDriverError> {
        self.next_transaction = self
            .next_transaction
            .checked_add(1)
            .ok_or(AmmSubscriberDriverError::StaleTransaction)?;
        Ok(SubscriberTransaction(self.next_transaction))
    }

    async fn handle_batch(
        &mut self,
        batch: evm_fork_cache::reactive::SubscriberInputBatch<Ethereum>,
    ) -> Result<(), AmmSubscriberDriverError> {
        // `SubscriberInputBatch` is already the typed domain form at this
        // boundary. Preserve this ingress across canonical reconciliation so
        // provider wait is included instead of resetting latency downstream.
        let source_ingress = Instant::now();
        let decoded_after_ingress = Duration::ZERO;
        if batch.preconfirmation_invalidated() {
            self.runtime.invalidate_preconfirmation().await?;
            if batch.records().is_empty() && batch.chain_controls().is_empty() {
                return Ok(());
            }
        }
        if !batch.records().is_empty()
            && batch
                .records()
                .iter()
                .all(|record| record.scope().is_preconfirmed())
        {
            handle_preconfirmation_result(
                self.preconfirmation_rejection_policy,
                self.runtime
                    .ingest_preconfirmation(batch.into_reactive_batch())
                    .await,
            )?;
            return Ok(());
        }
        // An attestation names the branch that was canonical when it was
        // issued, so record it before consuming the batch's records.
        self.note_log_coverage(batch.chain_controls());

        let mut headers = Vec::new();
        let mut discarded_logs = 0u64;
        for scoped in batch.into_records() {
            if !scoped.scope().is_canonical() {
                return Err(AmmSubscriberDriverError::OwnerCatchupRequiresStaging);
            }
            match scoped.into_record().input {
                ReactiveInput::BlockHeader(header) => headers.push(header),
                ReactiveInput::Log(log) => {
                    if self.buffers_subscription_logs() {
                        self.buffer_subscription_log(log)?;
                    } else {
                        // Reconcile mode sources canonical logs from
                        // `eth_getLogs`, so the delivered copy is redundant.
                        // Counting it keeps the duplicated cost visible.
                        discarded_logs = discarded_logs.saturating_add(1);
                    }
                }
                ReactiveInput::FullBlock(_)
                | ReactiveInput::PendingTxHash(_)
                | ReactiveInput::PendingTx(_) => {}
            }
        }
        AmmSubscriberDriverCounters::add(&self.stats.subscription_logs_discarded, discarded_logs);
        AmmSubscriberDriverCounters::add(
            &self.stats.canonical_headers_ingested,
            headers.len() as u64,
        );
        headers.sort_by_key(|header| header.inner.number);
        if !self.buffers_subscription_logs() {
            for header in headers {
                self.deliver_through_with_timing(header, source_ingress, decoded_after_ingress)
                    .await?;
            }
            return Ok(());
        }
        for header in headers {
            let number = header.inner.number;
            // Deliberately NOT note_observed_block: a header proves only that
            // the header stream advanced, never that an earlier block's logs
            // have all been delivered.
            self.pending_headers
                .insert(number, (header, Instant::now()));
        }
        self.drain_sealable(source_ingress, decoded_after_ingress)
            .await
    }

    /// Deliver every pending block whose log set is provably closed.
    ///
    /// Stops at the first block that must still wait, so delivery stays in
    /// canonical order. A block whose grace window elapsed without an
    /// attestation falls back to reconciliation rather than stalling: the point
    /// is to avoid the fetch when it is unnecessary, never to withhold state.
    async fn drain_sealable(
        &mut self,
        source_ingress: Instant,
        decoded_after_ingress: Duration,
    ) -> Result<(), AmmSubscriberDriverError> {
        loop {
            let Some((&number, (pending, waiting_since))) = self.pending_headers.iter().next()
            else {
                return Ok(());
            };
            let waiting_since = *waiting_since;
            let sealable = self.subscription_seal_reason(number, pending);
            // The straggler window no longer authorises publishing a buffer; it
            // only bounds how long we wait for one of the two proofs before
            // falling back to a hash-pinned reconciliation.
            if sealable.is_none() && waiting_since.elapsed() < self.seal_grace() {
                return Ok(());
            }
            let Some((header, _)) = self.pending_headers.remove(&number) else {
                return Ok(());
            };
            self.deliver_through_sealed(header, source_ingress, decoded_after_ingress, sealable)
                .await?;
            if self.stop_requested {
                return Ok(());
            }
        }
    }

    /// When the oldest pending block's grace window expires, if one is waiting.
    fn next_seal_deadline(&self) -> Option<Instant> {
        if !self.buffers_subscription_logs() {
            return None;
        }
        self.pending_headers
            .values()
            .map(|(_, since)| *since + self.seal_grace())
            .min()
    }

    /// Whether canonical logs are assembled from the subscription.
    const fn buffers_subscription_logs(&self) -> bool {
        matches!(
            self.canonical_log_source,
            CanonicalLogSource::Subscription { .. }
        )
    }

    /// Straggler window before a block with no observed successor is sealed.
    const fn seal_grace(&self) -> Duration {
        match self.canonical_log_source {
            CanonicalLogSource::Subscription { grace } => grace,
            CanonicalLogSource::Reconcile => Duration::ZERO,
        }
    }

    /// Record the subscriber's log-coverage attestation.
    ///
    /// The watermark is the only evidence that the delivered log set is whole;
    /// without one at or above a block, that block is reconciled.
    fn note_log_coverage(&mut self, controls: &[ChainControl]) {
        for control in controls {
            if let ChainControl::LogCoverage(block) = control {
                let advances = self
                    .attested_log_coverage
                    .is_none_or(|current| block.number > current);
                if advances {
                    self.attested_log_coverage = Some(block.number);
                }
            }
        }
    }

    /// Buffer one canonical log under its own block identity.
    ///
    /// Ordering is restored locally from the global log index, so out-of-order
    /// arrival costs nothing. A log missing its block identity cannot be placed
    /// and is rejected rather than guessed at.
    fn buffer_subscription_log(&mut self, log: RpcLog) -> Result<(), AmmSubscriberDriverError> {
        let (Some(number), Some(hash)) = (log.block_number, log.block_hash) else {
            return Err(AmmSubscriberDriverError::InvalidCanonicalLog(
                "canonical log is missing its block identity",
            ));
        };
        self.note_observed_block(number);
        let key = global_log_key(&log)?;
        match self
            .buffered_logs
            .entry((number, hash))
            .or_default()
            .entry(key)
        {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(log);
            }
            std::collections::btree_map::Entry::Occupied(entry) if entry.get() != &log => {
                return Err(AmmSubscriberDriverError::InvalidCanonicalLog(
                    "conflicting logs share one global log index",
                ));
            }
            std::collections::btree_map::Entry::Occupied(_) => {}
        }
        Ok(())
    }

    /// Note the highest block observed **on the log subscription**.
    ///
    /// An observation strictly above a pending block is what proves that
    /// block's log set closed: a single log subscription delivers in canonical
    /// order, so nothing further can arrive for an earlier block.
    ///
    /// The premise is specific to the log stream. `newHeads` is an independent
    /// subscription and a header for a later block proves nothing about whether
    /// an earlier block's logs have been delivered, so headers must never reach
    /// this. Feeding it from both is how a block came to be sealed carrying an
    /// empty log set while its logs were still in flight.
    fn note_observed_block(&mut self, number: u64) {
        let advances = self
            .highest_observed_block
            .is_none_or(|current| number > current);
        if advances {
            self.highest_observed_block = Some(number);
        }
    }

    /// Whether `block` can be submitted straight from the subscription.
    ///
    /// Two proofs are accepted, and only two. Both establish that the buffered
    /// set for `number` is the complete set; a timer does not, which is why the
    /// straggler window now leads to reconciliation rather than to publishing
    /// whatever happens to have arrived.
    fn subscription_seal_reason(&self, number: u64, header: &RpcHeader) -> Option<SealReason> {
        if !self.buffers_subscription_logs() {
            return None;
        }
        // Necessary but not sufficient: this attests that no notification loss
        // went unhealed at or below `number`, not that delivery has reached it.
        if self
            .attested_log_coverage
            .is_none_or(|attested| attested < number)
        {
            return None;
        }
        // Proof 1: the log subscription itself delivered something for a later
        // block, so in-order delivery puts `number` behind the write head.
        if self
            .highest_observed_block
            .is_some_and(|observed| observed > number)
        {
            return Some(SealReason::Successor);
        }
        // Proof 2: the block's own `logsBloom` excludes every registered
        // interest. A bloom admits false positives and never false negatives,
        // so exclusion is a positive proof that no matching log exists to wait
        // for. This is what keeps a quiet pool off the provider: the vast
        // majority of blocks touch none of our addresses.
        if self.bloom_excludes_every_interest(&header.inner.logs_bloom) {
            return Some(SealReason::BloomAbsent);
        }
        None
    }

    /// Whether `bloom` proves no registered log interest can match this block.
    ///
    /// A log matches an interest only if the bloom carries *both* its address
    /// and its `topic0`, so either being absent excludes the interest. Testing
    /// both matters: measured against live headers, the address alone leaves
    /// about half of Ethereum blocks unprovable because a saturated bloom admits
    /// it by false positive, and the topic check recovers a third of those.
    ///
    /// Conservative in the only direction that matters: a wildcard, or an
    /// interest the bloom admits on both counts, yields `false` and the caller
    /// waits for a log observation or reconciles.
    fn bloom_excludes_every_interest(&self, bloom: &alloy_primitives::Bloom) -> bool {
        let admits = |value: &[u8]| bloom.contains_input(alloy_primitives::BloomInput::Raw(value));
        let mut saw_log_interest = false;
        for interest in self.subscriber.registered_interests() {
            let ReactiveInterest::Logs(logs) = interest else {
                continue;
            };
            saw_log_interest = true;
            let filter = &logs.provider_filter;
            if filter.address.is_empty() {
                // An address wildcard could match anything in the bloom.
                return false;
            }
            if !filter
                .address
                .iter()
                .any(|address| admits(address.as_slice()))
            {
                // No address of this interest can appear, so it cannot match.
                continue;
            }
            // The address is admitted; a declared `topic0` set gives a second,
            // independent chance to exclude. An empty set is a wildcard.
            if !filter.topics[0].is_empty()
                && !filter.topics[0]
                    .iter()
                    .any(|topic| admits(topic.as_slice()))
            {
                continue;
            }
            return false;
        }
        // With no log interest registered there is nothing to wait for either,
        // but say so only when at least one interest was examined.
        saw_log_interest
    }

    /// Discard assembly state whose branch is no longer canonical.
    ///
    /// A reorg invalidates both the buffered logs and the attestation: the
    /// watermark described the branch that was canonical when it was issued, so
    /// carrying it across a replacement would vouch for blocks on a branch the
    /// subscriber never attested.
    fn reset_subscription_assembly(&mut self) {
        self.buffered_logs.clear();
        self.pending_headers.clear();
        self.attested_log_coverage = None;
    }

    /// Drop buffered logs for blocks already delivered.
    fn retire_buffered_logs_through(&mut self, number: u64) {
        self.buffered_logs.retain(|(block, _), _| *block > number);
        self.pending_headers.retain(|block, _| *block > number);
    }

    async fn deliver_through(&mut self, header: RpcHeader) -> Result<(), AmmSubscriberDriverError> {
        let source_ingress = Instant::now();
        self.deliver_through_with_timing(header, source_ingress, Duration::ZERO)
            .await
    }

    async fn deliver_through_with_timing(
        &mut self,
        header: RpcHeader,
        source_ingress: Instant,
        decoded_after_ingress: Duration,
    ) -> Result<(), AmmSubscriberDriverError> {
        self.deliver_through_sealed(header, source_ingress, decoded_after_ingress, None)
            .await
    }

    /// Deliver `header`, optionally with a seal decision already made.
    ///
    /// `drain_sealable` removes a block from the pending map before delivering
    /// it, so the instant it started waiting — and therefore whether its grace
    /// window elapsed — cannot be recovered here. The decision travels with the
    /// header instead of being recomputed against a map it has already left.
    async fn deliver_through_sealed(
        &mut self,
        header: RpcHeader,
        source_ingress: Instant,
        decoded_after_ingress: Duration,
        precomputed: Option<SealReason>,
    ) -> Result<(), AmmSubscriberDriverError> {
        let current = self.runtime.latest_snapshot().point();
        if header.inner.number == current.block_number() && header.hash == current.block_hash() {
            return Ok(());
        }
        let lineage = self.delivery_lineage(header).await?;
        // More than one block means the parent walk had to fill a gap or a
        // replacement branch: those blocks were not canonical while the
        // subscription was delivering, so no buffered set describes them and
        // the attestation named a different branch.
        if lineage.len() > 1 {
            self.reset_subscription_assembly();
        }
        let last = lineage.len().saturating_sub(1);
        for (index, header) in lineage.into_iter().enumerate() {
            let number = header.inner.number;
            let hash = header.hash;
            // Only the straight-line head of the walk can be subscription
            // sourced; everything behind it is a reconciled fill.
            // Only the straight-line head of the walk may use a seal decision;
            // everything behind it was filled by the parent walk.
            let sealable = if index == last { precomputed } else { None };
            match sealable {
                Some(reason) => {
                    self.seal_from_subscription(
                        header,
                        reason,
                        source_ingress,
                        decoded_after_ingress,
                    )
                    .await?;
                }
                None => {
                    self.reconcile_and_deliver_with_timing(
                        header,
                        source_ingress,
                        decoded_after_ingress,
                    )
                    .await?;
                    AmmSubscriberDriverCounters::add(&self.stats.blocks_reconciled, 1);
                }
            }
            self.record_canonical_block(number, hash);
            self.retire_buffered_logs_through(number);
        }
        Ok(())
    }

    /// Submit a block whose log set the subscription already delivered whole.
    ///
    /// Costs no provider request: the logs were paid for when they arrived.
    async fn seal_from_subscription(
        &mut self,
        header: RpcHeader,
        reason: SealReason,
        source_ingress: Instant,
        decoded_after_ingress: Duration,
    ) -> Result<(), AmmSubscriberDriverError> {
        let point = self.runtime.latest_snapshot().point();
        let block = BlockRef {
            number: header.inner.number,
            hash: header.hash,
            parent_hash: Some(header.inner.parent_hash),
            timestamp: Some(header.inner.timestamp),
        };
        let buffered = self
            .buffered_logs
            .remove(&(block.number, block.hash))
            .unwrap_or_default();
        let mut records = Vec::with_capacity(buffered.len());
        for log in buffered.into_values() {
            // Re-run the same identity checks reconciliation applies, so a
            // subscription-sourced block is held to the identical standard.
            let _ = validated_log_key(&log, &block)?;
            let context = ReactiveContext {
                chain_id: Some(point.chain_id()),
                source: InputSource::Subscription,
                chain_status: ChainStatus::Included {
                    block,
                    confirmations: 0,
                },
                block: Some(block),
                transaction_index: log.transaction_index,
                log_index: log.log_index,
            };
            records.push(ReactiveInputRecord::new(ReactiveInput::Log(log), context));
        }
        AmmSubscriberDriverCounters::add(
            &self.stats.subscription_logs_applied,
            records.len() as u64,
        );
        let batch = AmmCanonicalBatch::from_verified_block_with_timing(
            point.chain_id(),
            header,
            self.interest_revision,
            ReactiveInputBatch::new(records),
            source_ingress,
            decoded_after_ingress,
        )?;
        self.ingest_while_servicing_controls(batch).await?;
        AmmSubscriberDriverCounters::add(&self.stats.canonical_blocks_delivered, 1);
        AmmSubscriberDriverCounters::add(&self.stats.blocks_sealed_from_subscription, 1);
        match reason {
            SealReason::Successor => {
                AmmSubscriberDriverCounters::add(&self.stats.blocks_sealed_by_successor, 1);
            }
            SealReason::BloomAbsent => {
                AmmSubscriberDriverCounters::add(&self.stats.blocks_sealed_by_bloom_absence, 1);
            }
        }
        if self.stop_requested {
            return Ok(());
        }
        self.publish_running(self.runtime.latest_snapshot().point());
        Ok(())
    }

    async fn delivery_lineage(
        &mut self,
        header: RpcHeader,
    ) -> Result<Vec<RpcHeader>, AmmSubscriberDriverError> {
        let replacement = header.inner.number;
        let oldest_retained = self
            .canonical_lineage
            .first_key_value()
            .map(|(number, _)| *number)
            .unwrap_or(replacement);
        let mut descending = vec![header];
        loop {
            let current = descending
                .last()
                .expect("replacement lineage always contains its head");
            if self.canonical_lineage.get(&current.inner.number) == Some(&current.hash) {
                descending.pop();
                break;
            }
            let Some(parent_number) = current.inner.number.checked_sub(1) else {
                return Err(AmmSubscriberDriverError::ReorgBeyondRetainedLineage {
                    oldest_retained,
                    replacement,
                });
            };
            if self.canonical_lineage.get(&parent_number) == Some(&current.inner.parent_hash) {
                break;
            }
            if parent_number < oldest_retained {
                return Err(AmmSubscriberDriverError::ReorgBeyondRetainedLineage {
                    oldest_retained,
                    replacement,
                });
            }
            AmmSubscriberDriverCounters::add(&self.stats.lineage_parent_requests, 1);
            let parent = self
                .subscriber
                .provider()
                .get_block_by_hash(current.inner.parent_hash)
                .await
                .map_err(|error| AmmSubscriberDriverError::Provider(error.to_string()))?
                .ok_or(AmmSubscriberDriverError::MissingBlock(parent_number))?;
            let parent = parent.header().clone();
            if parent.hash != current.inner.parent_hash {
                return Err(AmmSubscriberDriverError::InvalidCanonicalLineage(
                    "parent response hash does not match the requested parent hash",
                ));
            }
            if parent.inner.number != parent_number {
                return Err(AmmSubscriberDriverError::InvalidCanonicalLineage(
                    "parent response number is not exactly one below its child",
                ));
            }
            descending.push(parent);
        }
        descending.reverse();
        Ok(descending)
    }

    fn record_canonical_block(&mut self, number: u64, hash: alloy_primitives::B256) {
        self.canonical_lineage
            .retain(|retained, _| *retained < number);
        self.canonical_lineage.insert(number, hash);
        while self.canonical_lineage.len() > RETAINED_CANONICAL_LINEAGE {
            self.canonical_lineage.pop_first();
        }
    }

    #[cfg(all(test, feature = "uniswap-v2"))]
    async fn reconcile_and_deliver(
        &mut self,
        header: RpcHeader,
    ) -> Result<(), AmmSubscriberDriverError> {
        let source_ingress = Instant::now();
        self.reconcile_and_deliver_with_timing(header, source_ingress, Duration::ZERO)
            .await
    }

    async fn reconcile_and_deliver_with_timing(
        &mut self,
        header: RpcHeader,
        source_ingress: Instant,
        decoded_after_ingress: Duration,
    ) -> Result<(), AmmSubscriberDriverError> {
        let point = self.runtime.latest_snapshot().point();
        let block = BlockRef {
            number: header.inner.number,
            hash: header.hash,
            parent_hash: Some(header.inner.parent_hash),
            timestamp: Some(header.inner.timestamp),
        };
        let interests: Vec<_> = self
            .subscriber
            .registered_interests()
            .iter()
            .filter_map(|interest| match interest {
                ReactiveInterest::Logs(logs) => Some(logs.provider_filter.clone()),
                ReactiveInterest::Blocks(_) | ReactiveInterest::PendingTransactions(_) => None,
            })
            .collect();
        let filters = reconciliation_filters(&interests, self.max_addresses_per_get_logs);
        let mut logs = BTreeMap::new();
        for filter in filters {
            AmmSubscriberDriverCounters::add(&self.stats.reconciliation_requests, 1);
            let fetched = self
                .subscriber
                .provider()
                .get_logs(&filter.at_block_hash(block.hash))
                .await
                .map_err(|error| AmmSubscriberDriverError::Provider(error.to_string()))?;
            AmmSubscriberDriverCounters::add(
                &self.stats.reconciliation_logs_fetched,
                fetched.len() as u64,
            );
            for log in fetched {
                let key = validated_log_key(&log, &block)?;
                match logs.entry(key) {
                    std::collections::btree_map::Entry::Vacant(entry) => {
                        entry.insert(log);
                    }
                    std::collections::btree_map::Entry::Occupied(entry) if entry.get() != &log => {
                        return Err(AmmSubscriberDriverError::InvalidCanonicalLog(
                            "conflicting logs share one global log index",
                        ));
                    }
                    std::collections::btree_map::Entry::Occupied(_) => {}
                }
            }
        }
        let records = logs
            .into_values()
            .map(|log| {
                let context = ReactiveContext {
                    chain_id: Some(point.chain_id()),
                    source: InputSource::Batch,
                    chain_status: ChainStatus::Included {
                        block,
                        confirmations: 0,
                    },
                    block: Some(block),
                    transaction_index: log.transaction_index,
                    log_index: log.log_index,
                };
                ReactiveInputRecord::new(ReactiveInput::Log(log), context)
            })
            .collect();
        let batch = AmmCanonicalBatch::from_verified_block_with_timing(
            point.chain_id(),
            header,
            self.interest_revision,
            ReactiveInputBatch::new(records),
            source_ingress,
            decoded_after_ingress,
        )?;
        self.ingest_while_servicing_controls(batch).await?;
        // Counted only after the runtime accepted the envelope, so a failed
        // ingest is never reported as a delivered block.
        AmmSubscriberDriverCounters::add(&self.stats.canonical_blocks_delivered, 1);
        if self.stop_requested {
            return Ok(());
        }
        self.publish_running(self.runtime.latest_snapshot().point());
        Ok(())
    }

    async fn ingest_while_servicing_controls(
        &mut self,
        batch: AmmCanonicalBatch,
    ) -> Result<(), AmmSubscriberDriverError> {
        let runtime = self.runtime.clone();
        let delivery = runtime.ingest_subscriber_batch(batch);
        tokio::pin!(delivery);
        loop {
            tokio::select! {
                biased;
                result = &mut delivery => return result.map(|_| ()).map_err(Into::into),
                command = self.commands.recv() => {
                    let Some(command) = command else {
                        return Err(AmmSubscriberDriverError::Closed);
                    };
                    if self.handle_control(command).await? {
                        return Ok(());
                    }
                }
            }
        }
    }

    fn publish_running(&self, point: AmmStatePoint) {
        self.state.send_replace(AmmSubscriberDriverState::Running {
            interest_revision: self.interest_revision,
            point,
        });
    }
}

fn state_point_block(point: AmmStatePoint) -> BlockRef {
    BlockRef {
        number: point.block_number(),
        hash: point.block_hash(),
        parent_hash: None,
        timestamp: None,
    }
}

/// Why a block's log set was treated as closed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SealReason {
    /// A strictly later block was seen **on the log subscription**, proving
    /// in-order delivery has moved past this block.
    Successor,
    /// The block's `logsBloom` excludes every registered interest, proving
    /// there is no matching log to wait for.
    BloomAbsent,
}

/// Global ordering key for a log within its block.
///
/// Identical to the key [`validated_log_key`] derives, but without the
/// cross-block checks, which are applied at seal time once the block's header
/// proves its identity.
fn global_log_key(log: &RpcLog) -> Result<u64, AmmSubscriberDriverError> {
    log.log_index
        .ok_or(AmmSubscriberDriverError::InvalidCanonicalLog(
            "canonical log is missing its global log index",
        ))
}

fn validated_log_key(log: &RpcLog, block: &BlockRef) -> Result<u64, AmmSubscriberDriverError> {
    if log.removed || log.block_number != Some(block.number) || log.block_hash != Some(block.hash) {
        return Err(AmmSubscriberDriverError::InvalidCanonicalLog(
            "log does not belong to the requested canonical block",
        ));
    }
    let transaction_index =
        log.transaction_index
            .ok_or(AmmSubscriberDriverError::InvalidCanonicalLog(
                "missing transaction index",
            ))?;
    let log_index = log
        .log_index
        .ok_or(AmmSubscriberDriverError::InvalidCanonicalLog(
            "missing log index",
        ))?;
    let transaction_hash =
        log.transaction_hash
            .ok_or(AmmSubscriberDriverError::InvalidCanonicalLog(
                "missing transaction hash",
            ))?;
    let _ = (transaction_index, transaction_hash);
    Ok(log_index)
}

fn reconciliation_filters(filters: &[Filter], max_addresses: usize) -> Vec<Filter> {
    if filters.is_empty() {
        return Vec::new();
    }
    let address_wildcard = filters.iter().any(|filter| filter.address.is_empty());
    let topic_wildcard = filters.iter().any(|filter| filter.topics[0].is_empty());
    let addresses: BTreeSet<_> = filters
        .iter()
        .flat_map(|filter| filter.address.iter().copied())
        .collect();
    let topics: Vec<_> = filters
        .iter()
        .flat_map(|filter| filter.topics[0].iter().copied())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();

    let build = |addresses: Option<Vec<alloy_primitives::Address>>| {
        let mut filter = Filter::new();
        if let Some(addresses) = addresses {
            filter = filter.address(addresses);
        }
        if !topic_wildcard {
            filter = filter.event_signature(topics.clone());
        }
        filter
    };
    if address_wildcard {
        vec![build(None)]
    } else {
        addresses
            .into_iter()
            .collect::<Vec<_>>()
            .chunks(max_addresses)
            .map(|chunk| build(Some(chunk.to_vec())))
            .collect()
    }
}

#[cfg(all(test, feature = "uniswap-v2"))]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use super::SealReason;
    use alloy_consensus::Header as ConsensusHeader;
    use alloy_network::Ethereum;
    use alloy_primitives::{Address, B256, Bytes, Log as PrimitiveLog, U256, keccak256};
    use alloy_provider::{ProviderBuilder, RootProvider, network::AnyNetwork};
    use alloy_rpc_client::RpcClient;
    use alloy_rpc_types_eth::{
        Block, EIP1186AccountProofResponse, Filter, Header as RpcHeader, Log as RpcLog,
    };
    use alloy_transport::mock::Asserter;
    use anyhow::Result;
    use evm_fork_cache::cache::EvmCache;
    use evm_fork_cache::reactive::{
        AlloySubscriber, BlockRef, ChainControl, ChainStatus, EventSubscriber, InputSource,
        LogInterest, ReactiveContext, ReactiveInput, ReactiveInputBatch, ReactiveInputRecord,
        ReactiveInterest, SubscriberConfig, SubscriberMode,
    };
    use tokio::sync::{mpsc, watch};

    use super::{
        AlloyAmmSubscriberDriver, AmmPreconfirmationRejectionPolicy, AmmSubscriberControl,
        AmmSubscriberDriverConfig, AmmSubscriberDriverCounters, AmmSubscriberDriverState,
        AmmSubscriberOwnerPlan, BTreeMap, CanonicalLogSource, SubscriberControlCommand,
        SubscriberTransaction, handle_preconfirmation_result, initial_canonical_lineage,
        reconciliation_filters,
    };
    use crate::adapters::{
        AdapterRegistry, AmmAdapter, AmmCanonicalBatch, AmmColdStartWorkerConfig,
        AmmFactoryWatcherRegistration, AmmRuntime, AmmRuntimeBaseline, AmmRuntimeCommandError,
        AmmRuntimeConfig, AmmRuntimeEventKind, AmmRuntimeHandle, CustomPoolKey, DiscoveryOwnerKey,
        EventSource, FactoryConfig, PoolDiscovery, PoolKey, PoolRegistration, PoolRuntimeState,
        PoolStateDependencies, PoolStatus, ProtocolId, UniswapV2Adapter,
        uniswap_v2_pair_runtime_code_hash,
    };

    #[test]
    fn preferred_preview_rejection_continues_canonical_delivery() {
        let result = handle_preconfirmation_result::<()>(
            AmmPreconfirmationRejectionPolicy::ContinueCanonical,
            Err(AmmRuntimeCommandError::PreconfirmationBatch(
                "quote call reverted or halted".to_owned(),
            )),
        );

        assert!(result.is_ok());
    }

    #[test]
    fn preferred_preview_policy_keeps_runtime_failures_fatal() {
        let result = handle_preconfirmation_result::<()>(
            AmmPreconfirmationRejectionPolicy::ContinueCanonical,
            Err(AmmRuntimeCommandError::Closed),
        );

        assert!(result.is_err());
    }

    #[test]
    fn preview_rejections_remain_fatal_by_default() {
        assert_eq!(
            AmmSubscriberDriverConfig::default().preconfirmation_rejection_policy(),
            AmmPreconfirmationRejectionPolicy::FailDriver
        );
        let result = handle_preconfirmation_result::<()>(
            AmmPreconfirmationRejectionPolicy::FailDriver,
            Err(AmmRuntimeCommandError::PreconfirmationBatch(
                "quote call reverted or halted".to_owned(),
            )),
        );

        assert!(result.is_err());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn subscriber_commit_timing_preserves_ingress_across_reconciliation() -> Result<()> {
        tokio::task::LocalSet::new()
            .run_until(async {
                let baseline_header = header(500, B256::repeat_byte(0x49));
                let mut cache = setup_cache().await;
                cache.advance_block(&baseline_header)?;
                let runtime = AmmRuntime::spawn(
                    cache,
                    AdapterRegistry::new(),
                    AmmRuntimeBaseline::from_verified_header(1, baseline_header.clone())?,
                    AmmRuntimeConfig::default(),
                )?;
                let provider = ProviderBuilder::new().connect_mocked_client(Asserter::new());
                let subscriber = AlloySubscriber::new(
                    provider,
                    SubscriberMode::Polling,
                    SubscriberConfig::default(),
                );
                let (command_tx, command_rx) = mpsc::channel(4);
                let (state, _) = watch::channel(AmmSubscriberDriverState::Paused);
                let control = AmmSubscriberControl {
                    commands: command_tx,
                };
                let mut driver = AlloyAmmSubscriberDriver {
                    runtime: runtime.clone(),
                    subscriber,
                    initial_interests: Vec::new(),
                    commands: command_rx,
                    state,
                    paused: true,
                    interest_revision: 0,
                    owners: HashMap::new(),
                    pending: None,
                    next_transaction: 0,
                    max_addresses_per_get_logs: 256,
                    preconfirmation_rejection_policy: AmmPreconfirmationRejectionPolicy::FailDriver,
                    report_stop: true,
                    stop_requested: false,
                    stats: Arc::new(AmmSubscriberDriverCounters::default()),
                    canonical_lineage: initial_canonical_lineage(runtime.latest_snapshot().point()),
                    canonical_log_source: CanonicalLogSource::default(),
                    attested_log_coverage: None,
                    buffered_logs: BTreeMap::new(),
                    pending_headers: BTreeMap::new(),
                    highest_observed_block: None,
                };
                let attach = runtime.attach_subscriber_control(control);
                tokio::pin!(attach);
                tokio::select! {
                    result = &mut attach => result?,
                    command = driver.commands.recv() => {
                        driver.handle_control(command.expect("adoption command")).await?;
                        attach.await?;
                    }
                }

                let mut commits = runtime.subscribe_changes().await?;
                let source_ingress = Instant::now();
                tokio::time::sleep(Duration::from_millis(5)).await;
                driver
                    .reconcile_and_deliver_with_timing(
                        header(501, baseline_header.hash),
                        source_ingress,
                        Duration::ZERO,
                    )
                    .await?;
                let commit = commits.next_commit().await.expect("canonical commit");
                let timing = commit.timing().expect("canonical timing provenance");
                assert_eq!(timing.source_ingress(), source_ingress);
                assert!(timing.elapsed_to_commit() >= Duration::from_millis(5));
                assert!(timing.decoded_after_ingress() <= timing.ordered_after_ingress());
                assert!(timing.ordered_after_ingress() <= timing.transitioned_after_ingress());
                assert!(timing.transitioned_after_ingress() <= timing.committed_after_ingress());

                runtime.shutdown().await?;
                Ok(())
            })
            .await
    }

    struct EmptyAdapter;

    impl AmmAdapter for EmptyAdapter {
        fn protocol(&self) -> ProtocolId {
            ProtocolId::Custom("test.fence")
        }

        fn event_sources(&self, pool: &PoolRegistration) -> Vec<EventSource> {
            vec![EventSource::direct(
                pool.key.address().expect("test pool is address keyed"),
                vec![B256::repeat_byte(0x51)],
            )]
        }

        fn state_dependencies(&self, _pool: &PoolRegistration) -> PoolStateDependencies {
            PoolStateDependencies::default()
        }
    }

    async fn setup_cache() -> EvmCache {
        let provider = RootProvider::<AnyNetwork>::new(RpcClient::mocked(Asserter::new()));
        EvmCache::new(Arc::new(provider)).await
    }

    fn address_topic(address: Address) -> B256 {
        let mut word = [0_u8; 32];
        word[12..].copy_from_slice(address.as_slice());
        B256::from(word)
    }

    fn encoded_words(words: impl IntoIterator<Item = U256>) -> Bytes {
        let mut encoded = Vec::new();
        for word in words {
            encoded.extend_from_slice(&word.to_be_bytes::<32>());
        }
        encoded.into()
    }

    fn v2_account_proof(address: Address) -> EIP1186AccountProofResponse {
        EIP1186AccountProofResponse {
            address,
            balance: U256::ZERO,
            code_hash: uniswap_v2_pair_runtime_code_hash(),
            nonce: 1,
            storage_hash: B256::repeat_byte(0x77),
            account_proof: Vec::new(),
            storage_proof: Vec::new(),
        }
    }

    fn factory_batch(
        block_number: u64,
        parent_hash: B256,
        interest_revision: u64,
        factory: Address,
        token0: Address,
        token1: Address,
        pool: Address,
    ) -> Result<AmmCanonicalBatch> {
        let header = header(block_number, parent_hash);
        let block = BlockRef {
            number: block_number,
            hash: header.hash,
            parent_hash: Some(header.inner.parent_hash),
            timestamp: Some(header.inner.timestamp),
        };
        let mut data = [0_u8; 64];
        data[12..32].copy_from_slice(pool.as_slice());
        data[63] = 1;
        let log = PrimitiveLog::new_unchecked(
            factory,
            vec![
                keccak256("PairCreated(address,address,address,uint256)"),
                address_topic(token0),
                address_topic(token1),
            ],
            Bytes::copy_from_slice(&data),
        );
        let record = ReactiveInputRecord::new(
            ReactiveInput::Log(RpcLog {
                inner: log,
                block_hash: Some(block.hash),
                block_number: Some(block.number),
                transaction_hash: Some(B256::repeat_byte(0xe1)),
                transaction_index: Some(0),
                log_index: Some(0),
                ..RpcLog::default()
            }),
            ReactiveContext {
                chain_id: Some(1),
                source: InputSource::Synthetic,
                chain_status: ChainStatus::Included {
                    block,
                    confirmations: 0,
                },
                block: Some(block),
                transaction_index: Some(0),
                log_index: Some(0),
            },
        );
        Ok(AmmCanonicalBatch::from_verified_block(
            1,
            header,
            interest_revision,
            ReactiveInputBatch::new(vec![record]),
        )?)
    }

    #[tokio::test]
    async fn generic_owner_add_preserves_exact_handler_and_interests() -> Result<()> {
        use evm_fork_cache::reactive::{BlockInterest, HandlerId, ReactiveInterest};

        let (commands, mut receiver) = tokio::sync::mpsc::channel(1);
        let control = AmmSubscriberControl { commands };
        let owner = HandlerId::new("evm-amm-state.discovery.ethereum.factory");
        let interests = vec![ReactiveInterest::Blocks(BlockInterest::default())];
        let point = crate::adapters::AmmStatePoint::post_block(1, 500, B256::repeat_byte(0x50));
        let request = tokio::spawn({
            let control = control.clone();
            let owner = owner.clone();
            let interests = interests.clone();
            async move {
                control
                    .begin_add_owners(vec![AmmSubscriberOwnerPlan::new(owner, interests)], point)
                    .await
            }
        });

        let Some(SubscriberControlCommand::BeginAdd {
            plans,
            point: submitted_point,
            response,
        }) = receiver.recv().await
        else {
            panic!("generic owner addition must use the ordinary subscriber transaction")
        };
        assert_eq!(submitted_point, point);
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].owner(), &owner);
        assert_eq!(plans[0].interests().len(), interests.len());
        assert!(matches!(
            plans[0].interests(),
            [ReactiveInterest::Blocks(_)]
        ));
        response.send(Ok(SubscriberTransaction(7))).unwrap();
        assert_eq!(request.await??, SubscriberTransaction(7));
        Ok(())
    }

    #[tokio::test]
    async fn generic_owner_adoption_preserves_revision_and_exact_owner() -> Result<()> {
        use evm_fork_cache::reactive::{BlockInterest, HandlerId, ReactiveInterest};

        let (commands, mut receiver) = tokio::sync::mpsc::channel(1);
        let control = AmmSubscriberControl { commands };
        let owner = HandlerId::new("evm-amm-state.discovery.ethereum.initial-factory");
        let point = crate::adapters::AmmStatePoint::post_block(1, 500, B256::repeat_byte(0x50));
        let request = tokio::spawn({
            let control = control.clone();
            let owner = owner.clone();
            async move {
                control
                    .adopt_existing_owners(
                        vec![AmmSubscriberOwnerPlan::new(
                            owner,
                            vec![ReactiveInterest::Blocks(BlockInterest::default())],
                        )],
                        point,
                        9,
                    )
                    .await
            }
        });

        let Some(SubscriberControlCommand::AdoptExisting {
            plans,
            point: submitted_point,
            interest_revision,
            response,
        }) = receiver.recv().await
        else {
            panic!("generic owner adoption must use the ordinary subscriber transaction")
        };
        assert_eq!(submitted_point, point);
        assert_eq!(interest_revision, 9);
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].owner(), &owner);
        response.send(Ok(())).unwrap();
        request.await??;
        Ok(())
    }

    fn header(number: u64, parent_hash: B256) -> RpcHeader {
        RpcHeader::new(ConsensusHeader {
            parent_hash,
            number,
            timestamp: 1_700_000_000 + number,
            base_fee_per_gas: Some(100 + number),
            beneficiary: Address::repeat_byte(0xcb),
            gas_limit: 30_000_000,
            mix_hash: B256::repeat_byte(0xab),
            ..ConsensusHeader::default()
        })
    }

    fn alternate_header(number: u64, parent_hash: B256, label: &'static [u8]) -> RpcHeader {
        let mut inner = header(number, parent_hash).inner;
        inner.extra_data = Bytes::from_static(label);
        RpcHeader::new(inner)
    }

    fn registration(address: Address) -> PoolRegistration {
        PoolRegistration::new(PoolKey::Custom(CustomPoolKey::Address {
            protocol: "test.fence",
            address,
        }))
        .with_status(PoolStatus::Ready)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn attached_driver_is_the_only_canonical_origin_and_runtime_shutdown_never_awaits_it()
    -> Result<()> {
        tokio::task::LocalSet::new()
            .run_until(async {
                let baseline_header = header(500, B256::repeat_byte(0x49));
                let mut cache = setup_cache().await;
                cache.advance_block(&baseline_header)?;
                let runtime = AmmRuntime::spawn(
                    cache,
                    AdapterRegistry::new(),
                    AmmRuntimeBaseline::from_verified_header(1, baseline_header.clone())?,
                    AmmRuntimeConfig::default(),
                )?;
                let (commands, mut receiver) = tokio::sync::mpsc::channel(4);
                let control = AmmSubscriberControl { commands };
                let fake_driver = tokio::spawn(async move {
                    let Some(SubscriberControlCommand::AdoptExisting { response, .. }) =
                        receiver.recv().await
                    else {
                        panic!("actor must adopt existing owners first")
                    };
                    let _ = response.send(Ok(()));
                    std::future::pending::<()>().await;
                });
                runtime.attach_subscriber_control(control).await?;

                let next = header(501, baseline_header.hash);
                let direct = AmmCanonicalBatch::from_verified_block(
                    1,
                    next,
                    0,
                    ReactiveInputBatch::<Ethereum>::new(Vec::new()),
                )?;
                assert!(matches!(
                    runtime.ingest_batch(direct).await,
                    Err(AmmRuntimeCommandError::AttachedSubscriberOwnsCanonicalInput)
                ));
                tokio::time::timeout(std::time::Duration::from_millis(100), runtime.shutdown())
                    .await
                    .expect("runtime shutdown must not await a wedged driver")?;
                fake_driver.abort();
                Ok(())
            })
            .await
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn add_fence_services_an_inflight_driver_delivery_then_aborts_the_stale_install()
    -> Result<()> {
        tokio::task::LocalSet::new()
            .run_until(async {
                let baseline_header = header(500, B256::repeat_byte(0x49));
                let next_header = header(501, baseline_header.hash);
                let mut cache = setup_cache().await;
                cache.advance_block(&baseline_header)?;
                let mut registry = AdapterRegistry::new();
                registry.register_adapter(Arc::new(EmptyAdapter))?;
                let runtime = AmmRuntime::spawn(
                    cache,
                    registry,
                    AmmRuntimeBaseline::from_verified_header(1, baseline_header)?,
                    AmmRuntimeConfig::default(),
                )?;
                let baseline = runtime.latest_snapshot().point();
                let (commands, mut receiver) = tokio::sync::mpsc::channel(8);
                let control = AmmSubscriberControl { commands };
                let driver_runtime = runtime.clone();
                let fake_driver = tokio::spawn(async move {
                    while let Some(command) = receiver.recv().await {
                        match command {
                            SubscriberControlCommand::AdoptExisting { response, .. } => {
                                let _ = response.send(Ok(()));
                            }
                            SubscriberControlCommand::BeginAdd { response, .. } => {
                                let batch = AmmCanonicalBatch::from_verified_block(
                                    1,
                                    next_header.clone(),
                                    0,
                                    ReactiveInputBatch::<Ethereum>::new(Vec::new()),
                                )
                                .expect("fixture is coherent");
                                driver_runtime
                                    .ingest_subscriber_batch(batch)
                                    .await
                                    .expect("actor services driver delivery while fencing");
                                let _ = response.send(Ok(SubscriberTransaction(1)));
                            }
                            SubscriberControlCommand::Abort {
                                transaction,
                                response,
                            } => {
                                assert_eq!(transaction, SubscriberTransaction(1));
                                let _ = response.send(Ok(()));
                            }
                            SubscriberControlCommand::Shutdown { response, .. } => {
                                let _ = response.send(Ok(()));
                                break;
                            }
                            _ => panic!("unexpected fake-driver command"),
                        }
                    }
                });
                runtime.attach_subscriber_control(control).await?;

                let result = tokio::time::timeout(
                    std::time::Duration::from_millis(100),
                    runtime.install_prepared_pools(
                        vec![registration(Address::repeat_byte(0x55))],
                        baseline,
                    ),
                )
                .await
                .expect("lifecycle fence must not deadlock");
                assert!(
                    matches!(&result, Err(AmmRuntimeCommandError::StaleBaseline { .. })),
                    "unexpected fenced install result: {result:?}"
                );
                assert_eq!(runtime.latest_snapshot().point().block_number(), 501);
                assert_eq!(runtime.latest_snapshot().registry().pool_count(), 0);
                assert_eq!(runtime.interest_revision(), 0);
                runtime.shutdown().await?;
                fake_driver.await?;
                Ok(())
            })
            .await
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn successful_subscriber_add_publishes_the_exact_generation_live() -> Result<()> {
        tokio::task::LocalSet::new()
            .run_until(async {
                let baseline_header = header(500, B256::repeat_byte(0x49));
                let mut cache = setup_cache().await;
                cache.advance_block(&baseline_header)?;
                let mut registry = AdapterRegistry::new();
                registry.register_adapter(Arc::new(EmptyAdapter))?;
                let runtime = AmmRuntime::spawn(
                    cache,
                    registry,
                    AmmRuntimeBaseline::from_verified_header(1, baseline_header)?,
                    AmmRuntimeConfig::default(),
                )?;
                let baseline = runtime.latest_snapshot().point();
                let (commands, mut receiver) = tokio::sync::mpsc::channel(8);
                let control = AmmSubscriberControl { commands };
                let fake_driver = tokio::spawn(async move {
                    while let Some(command) = receiver.recv().await {
                        match command {
                            SubscriberControlCommand::AdoptExisting { response, .. } => {
                                let _ = response.send(Ok(()));
                            }
                            SubscriberControlCommand::BeginAdd { response, .. } => {
                                let _ = response.send(Ok(SubscriberTransaction(1)));
                            }
                            SubscriberControlCommand::Commit {
                                transaction,
                                interest_revision,
                                point,
                                response,
                            } => {
                                assert_eq!(transaction, SubscriberTransaction(1));
                                assert_eq!(interest_revision, 1);
                                assert_eq!(point, baseline);
                                let _ = response.send(Ok(()));
                            }
                            SubscriberControlCommand::Shutdown { response, .. } => {
                                let _ = response.send(Ok(()));
                                break;
                            }
                            _ => panic!("unexpected fake-driver command"),
                        }
                    }
                });
                runtime.attach_subscriber_control(control).await?;
                let mut events = runtime.subscribe_events();
                let pool = registration(Address::repeat_byte(0x56));
                runtime
                    .install_prepared_pools(vec![pool.clone()], baseline)
                    .await?;

                let snapshot = runtime.latest_snapshot();
                let instance = snapshot
                    .registry()
                    .pool_instance(&pool.key)
                    .expect("installed generation")
                    .clone();
                assert_eq!(
                    runtime.latest_status().pool_state(&instance),
                    Some(PoolRuntimeState::Live)
                );
                assert_eq!(snapshot.interest_revision(), 1);
                assert!(matches!(
                    events.next_event().await?.kind(),
                    AmmRuntimeEventKind::RegistrationAccepted { pool } if pool == &instance
                ));
                assert!(matches!(
                    events.next_event().await?.kind(),
                    AmmRuntimeEventKind::PoolLifecycleTransition {
                        pool,
                        from: PoolRuntimeState::Searchable,
                        to: PoolRuntimeState::Live,
                    } if pool == &instance
                ));
                assert!(matches!(
                    events.next_event().await?.kind(),
                    AmmRuntimeEventKind::StateCommitted { .. }
                ));
                runtime.shutdown().await?;
                fake_driver.await?;
                Ok(())
            })
            .await
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn attached_driver_removes_factory_only_pool_during_reorg_without_deadlock() -> Result<()>
    {
        tokio::task::LocalSet::new()
            .run_until(async {
                let baseline_header = header(500, B256::repeat_byte(0x49));
                let mut cache = setup_cache().await;
                cache.advance_block(&baseline_header)?;
                let factory = Address::repeat_byte(0xe2);
                let token0 = Address::repeat_byte(0xe3);
                let token1 = Address::repeat_byte(0xe4);
                let pool = Address::repeat_byte(0xe5);
                let mut registry = AdapterRegistry::new();
                registry.register_adapter(Arc::new(UniswapV2Adapter::default()))?;
                let discovery = Arc::new(PoolDiscovery::for_registry(
                    &registry,
                    FactoryConfig::default().with_uniswap_v2_factory(factory),
                ));
                let runtime = AmmRuntime::spawn(
                    cache,
                    registry,
                    AmmRuntimeBaseline::from_verified_header(1, baseline_header.clone())?,
                    AmmRuntimeConfig::default(),
                )?;
                let adapter = runtime
                    .latest_snapshot()
                    .registry()
                    .adapters()
                    .next()
                    .expect("V2 adapter generation")
                    .1
                    .clone();
                runtime
                    .add_factory_watcher(AmmFactoryWatcherRegistration::new(
                        DiscoveryOwnerKey::new("attached-reorg-v2"),
                        adapter,
                        discovery,
                    ))
                    .await?;

                let hydration = Asserter::new();
                hydration.push_success(&encoded_words([
                    U256::from_be_slice(token0.as_slice()),
                    U256::from_be_slice(token1.as_slice()),
                    U256::from(77) | (U256::from(88) << 112),
                ]));
                hydration.push_success(&v2_account_proof(pool));
                let worker_provider = RootProvider::<AnyNetwork>::new(RpcClient::mocked(hydration));
                let worker = runtime
                    .attach_cold_start_worker(worker_provider, AmmColdStartWorkerConfig::default())
                    .await?;
                runtime
                    .ingest_batch(factory_batch(
                        501,
                        baseline_header.hash,
                        runtime.interest_revision(),
                        factory,
                        token0,
                        token1,
                        pool,
                    )?)
                    .await?;
                tokio::time::timeout(std::time::Duration::from_secs(1), async {
                    while runtime
                        .latest_snapshot()
                        .registry()
                        .pool_instance(&PoolKey::UniswapV2(pool))
                        .is_none()
                    {
                        tokio::task::yield_now().await;
                    }
                })
                .await?;
                worker.shutdown();

                let reconciliation = Asserter::new();
                reconciliation.push_success(&U256::from(1)); // eth_chainId
                reconciliation.push_success(&U256::from(1)); // eth_newFilter
                reconciliation.push_success(&U256::from(2)); // eth_newFilter
                let canonical_block: Block = Block::empty(header(501, baseline_header.hash));
                reconciliation.push_success(&Some(canonical_block.clone()));
                reconciliation.push_success(&Some(canonical_block));
                reconciliation.push_success(&Vec::<RpcLog>::new());
                let provider = ProviderBuilder::new().connect_mocked_client(reconciliation);
                let subscriber = AlloySubscriber::new(
                    provider,
                    SubscriberMode::Polling,
                    SubscriberConfig::default(),
                );
                let (command_tx, command_rx) = mpsc::channel(8);
                let (state, _) = watch::channel(AmmSubscriberDriverState::Paused);
                let control = AmmSubscriberControl {
                    commands: command_tx,
                };
                let mut driver = AlloyAmmSubscriberDriver {
                    runtime: runtime.clone(),
                    subscriber,
                    initial_interests: Vec::new(),
                    commands: command_rx,
                    state,
                    paused: true,
                    interest_revision: 0,
                    owners: HashMap::new(),
                    pending: None,
                    next_transaction: 0,
                    max_addresses_per_get_logs: 256,
                    preconfirmation_rejection_policy: AmmPreconfirmationRejectionPolicy::FailDriver,
                    report_stop: true,
                    stop_requested: false,
                    stats: Arc::new(AmmSubscriberDriverCounters::default()),
                    canonical_lineage: initial_canonical_lineage(runtime.latest_snapshot().point()),
                    canonical_log_source: CanonicalLogSource::default(),
                    attested_log_coverage: None,
                    buffered_logs: BTreeMap::new(),
                    pending_headers: BTreeMap::new(),
                    highest_observed_block: None,
                };

                let attach = runtime.attach_subscriber_control(control);
                tokio::pin!(attach);
                tokio::select! {
                    result = &mut attach => result?,
                    command = driver.commands.recv() => {
                        driver.handle_control(command.expect("adoption command")).await?;
                        attach.await?;
                    }
                }
                let adopted_revision = runtime.interest_revision();
                assert_eq!(driver.interest_revision, adopted_revision);

                let mut replacement = header(501, baseline_header.hash).inner;
                replacement.extra_data = Bytes::from_static(b"attached-orphan");
                tokio::time::timeout(
                    std::time::Duration::from_millis(250),
                    driver.reconcile_and_deliver(RpcHeader::new(replacement)),
                )
                .await
                .expect("attached reorg cleanup must not deadlock")?;

                assert!(
                    runtime
                        .latest_snapshot()
                        .registry()
                        .pool_instance(&PoolKey::UniswapV2(pool))
                        .is_none()
                );
                assert_eq!(runtime.interest_revision(), adopted_revision + 1);
                assert!(matches!(
                    driver.state.borrow().clone(),
                    AmmSubscriberDriverState::Running { .. }
                ));
                runtime.shutdown().await?;
                Ok(())
            })
            .await
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn explicit_shutdown_during_inflight_delivery_is_graceful() -> Result<()> {
        tokio::task::LocalSet::new()
            .run_until(async {
                let baseline_header = header(500, B256::repeat_byte(0x49));
                let mut cache = setup_cache().await;
                cache.advance_block(&baseline_header)?;
                let runtime = AmmRuntime::spawn(
                    cache,
                    AdapterRegistry::new(),
                    AmmRuntimeBaseline::from_verified_header(1, baseline_header.clone())?,
                    AmmRuntimeConfig::default().with_critical_change_capacity(1),
                )?;
                let provider = ProviderBuilder::new().connect_mocked_client(Asserter::new());
                let subscriber = AlloySubscriber::new(
                    provider,
                    SubscriberMode::Polling,
                    SubscriberConfig::default(),
                );
                let (command_tx, command_rx) = mpsc::channel(4);
                let (state, _) = watch::channel(AmmSubscriberDriverState::Paused);
                let control = AmmSubscriberControl {
                    commands: command_tx,
                };
                let shutdown_control = control.clone();
                let mut driver = AlloyAmmSubscriberDriver {
                    runtime: runtime.clone(),
                    subscriber,
                    initial_interests: Vec::new(),
                    commands: command_rx,
                    state,
                    paused: true,
                    interest_revision: 0,
                    owners: HashMap::new(),
                    pending: None,
                    next_transaction: 0,
                    max_addresses_per_get_logs: 256,
                    preconfirmation_rejection_policy: AmmPreconfirmationRejectionPolicy::FailDriver,
                    report_stop: true,
                    stop_requested: false,
                    stats: Arc::new(AmmSubscriberDriverCounters::default()),
                    canonical_lineage: initial_canonical_lineage(runtime.latest_snapshot().point()),
                    canonical_log_source: CanonicalLogSource::default(),
                    attested_log_coverage: None,
                    buffered_logs: BTreeMap::new(),
                    pending_headers: BTreeMap::new(),
                    highest_observed_block: None,
                };
                let attach = runtime.attach_subscriber_control(control);
                tokio::pin!(attach);
                tokio::select! {
                    result = &mut attach => result?,
                    command = driver.commands.recv() => {
                        driver.handle_control(command.expect("adoption command")).await?;
                        attach.await?;
                    }
                }

                let _critical = runtime.subscribe_changes().await?;
                driver
                    .reconcile_and_deliver(header(501, baseline_header.hash))
                    .await?;
                let blocked_delivery = driver
                    .reconcile_and_deliver(header(502, header(501, baseline_header.hash).hash));
                let shutdown = shutdown_control.shutdown(true);
                let (delivery_result, shutdown_result) =
                    tokio::time::timeout(std::time::Duration::from_millis(250), async {
                        tokio::join!(blocked_delivery, shutdown)
                    })
                    .await
                    .expect("explicit shutdown must release an in-flight delivery");
                shutdown_result?;
                delivery_result?;

                runtime.shutdown().await?;
                Ok(())
            })
            .await
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn overtaking_reorg_delivers_every_replacement_block_from_the_common_ancestor()
    -> Result<()> {
        tokio::task::LocalSet::new()
            .run_until(async {
                let baseline = header(500, B256::repeat_byte(0x49));
                let old_501 = header(501, baseline.hash);
                let old_502 = header(502, old_501.hash);
                let replacement_501 = alternate_header(501, baseline.hash, b"replacement-501");
                let replacement_502 =
                    alternate_header(502, replacement_501.hash, b"replacement-502");
                let replacement_503 =
                    alternate_header(503, replacement_502.hash, b"replacement-503");

                let mut cache = setup_cache().await;
                cache.advance_block(&baseline)?;
                let runtime = AmmRuntime::spawn(
                    cache,
                    AdapterRegistry::new(),
                    AmmRuntimeBaseline::from_verified_header(1, baseline.clone())?,
                    AmmRuntimeConfig::default(),
                )?;
                let asserter = Asserter::new();
                let replacement_502_block: Block = Block::empty(replacement_502.clone());
                let replacement_501_block: Block = Block::empty(replacement_501.clone());
                asserter.push_success(&Some(replacement_502_block));
                asserter.push_success(&Some(replacement_501_block));
                let provider = ProviderBuilder::new().connect_mocked_client(asserter);
                let subscriber = AlloySubscriber::new(
                    provider,
                    SubscriberMode::Polling,
                    SubscriberConfig::default(),
                );
                let (_command_tx, command_rx) = mpsc::channel(4);
                let (state, _) = watch::channel(AmmSubscriberDriverState::Paused);
                let mut driver = AlloyAmmSubscriberDriver {
                    runtime: runtime.clone(),
                    subscriber,
                    initial_interests: Vec::new(),
                    commands: command_rx,
                    state,
                    paused: false,
                    interest_revision: 0,
                    owners: HashMap::new(),
                    pending: None,
                    next_transaction: 0,
                    max_addresses_per_get_logs: 256,
                    preconfirmation_rejection_policy: AmmPreconfirmationRejectionPolicy::FailDriver,
                    report_stop: true,
                    stop_requested: false,
                    stats: Arc::new(AmmSubscriberDriverCounters::default()),
                    canonical_lineage: initial_canonical_lineage(runtime.latest_snapshot().point()),
                    canonical_log_source: CanonicalLogSource::default(),
                    attested_log_coverage: None,
                    buffered_logs: BTreeMap::new(),
                    pending_headers: BTreeMap::new(),
                    highest_observed_block: None,
                };

                driver.deliver_through(old_501).await?;
                driver.deliver_through(old_502).await?;
                let mut changes = runtime.subscribe_changes().await?;
                assert_eq!(changes.snapshot().point().block_number(), 502);

                driver.deliver_through(replacement_503.clone()).await?;
                let mut replacement_points = Vec::new();
                for _ in 0..3 {
                    replacement_points.push(
                        tokio::time::timeout(
                            std::time::Duration::from_millis(250),
                            changes.next_commit(),
                        )
                        .await
                        .expect("every replacement block must be published")
                        .expect("runtime remains subscribed")
                        .snapshot()
                        .point(),
                    );
                }
                assert_eq!(
                    replacement_points
                        .iter()
                        .map(|point| (point.block_number(), point.block_hash()))
                        .collect::<Vec<_>>(),
                    vec![
                        (501, replacement_501.hash),
                        (502, replacement_502.hash),
                        (503, replacement_503.hash),
                    ]
                );
                assert_eq!(runtime.latest_snapshot().point(), replacement_points[2]);

                runtime.shutdown().await?;
                Ok(())
            })
            .await
    }

    #[test]
    fn complete_block_reconciliation_chunks_addresses_and_unions_topics() {
        let filters: Vec<_> = (0..600u64)
            .map(|index| {
                let mut bytes = [0u8; 20];
                bytes[12..].copy_from_slice(&index.to_be_bytes());
                Filter::new()
                    .address(Address::from(bytes))
                    .event_signature(B256::repeat_byte((index % 3) as u8))
                    .topic1(B256::repeat_byte(0xff))
            })
            .collect();

        let reconciled = reconciliation_filters(&filters, 256);
        assert_eq!(reconciled.len(), 3);
        assert_eq!(
            reconciled
                .iter()
                .map(|filter| filter.address.len())
                .sum::<usize>(),
            600
        );
        assert!(reconciled.iter().all(|filter| filter.topics[0].len() == 3));
        assert!(
            reconciled.iter().all(|filter| filter.topics[1].is_empty()),
            "indexed-topic constraints are broadened to avoid cross-filter false negatives"
        );
    }

    #[test]
    fn any_wildcard_filter_keeps_the_reconciliation_union_broad() {
        let filters = vec![
            Filter::new(),
            Filter::new()
                .address(Address::repeat_byte(0x11))
                .event_signature(B256::repeat_byte(0x22)),
        ];
        let reconciled = reconciliation_filters(&filters, 256);
        assert_eq!(reconciled.len(), 1);
        assert!(reconciled[0].address.is_empty());
        assert!(reconciled[0].topics[0].is_empty());
    }

    /// Phase 0 exit criterion, measured from inside the process: the WebSocket
    /// delivers canonical logs, the driver discards every one of them, and then
    /// re-fetches the same block's logs with a hash-pinned `eth_getLogs`.
    ///
    /// Both halves are asserted against the same two logs, so this pins the
    /// duplication itself rather than either side of it in isolation.
    #[tokio::test(flavor = "multi_thread")]
    async fn canonical_delivery_discards_stream_logs_then_refetches_them() -> Result<()> {
        tokio::task::LocalSet::new()
            .run_until(async {
                let pool = Address::repeat_byte(0x77);
                let topic = keccak256(b"Swap()");
                let baseline_header = header(500, B256::repeat_byte(0x49));
                let delivered = header(501, baseline_header.hash);
                let stream_logs = vec![
                    reconciled_log(pool, topic, &delivered, 0),
                    reconciled_log(pool, topic, &delivered, 1),
                ];

                let asserter = Asserter::new();
                // Stream install, chain identity, then the live log poll.
                asserter.push_success(&U256::from(1));
                asserter.push_success(&U256::from(1));
                asserter.push_success(&stream_logs);
                // The reconciliation fetch for the very same logs.
                asserter.push_success(&stream_logs);

                let mut cache = setup_cache().await;
                cache.advance_block(&baseline_header)?;
                let runtime = AmmRuntime::spawn(
                    cache,
                    AdapterRegistry::new(),
                    AmmRuntimeBaseline::from_verified_header(1, baseline_header.clone())?,
                    AmmRuntimeConfig::default(),
                )?;
                let provider = ProviderBuilder::new().connect_mocked_client(asserter.clone());
                let mut subscriber = AlloySubscriber::new(
                    provider,
                    SubscriberMode::Polling,
                    SubscriberConfig::default(),
                );
                subscriber
                    .register_interests(&[ReactiveInterest::Logs(LogInterest {
                        provider_filter: Filter::new().address(pool).event_signature(topic),
                        local_matcher: None,
                        route_key: None,
                    })])
                    .await?;

                let (command_tx, command_rx) = mpsc::channel(4);
                let (state, _) = watch::channel(AmmSubscriberDriverState::Paused);
                let control = AmmSubscriberControl {
                    commands: command_tx,
                };
                let stats = Arc::new(AmmSubscriberDriverCounters::default());
                let mut driver = AlloyAmmSubscriberDriver {
                    runtime: runtime.clone(),
                    subscriber,
                    initial_interests: Vec::new(),
                    commands: command_rx,
                    state,
                    paused: true,
                    interest_revision: 0,
                    owners: HashMap::new(),
                    pending: None,
                    next_transaction: 0,
                    max_addresses_per_get_logs: 256,
                    preconfirmation_rejection_policy: AmmPreconfirmationRejectionPolicy::FailDriver,
                    report_stop: true,
                    stop_requested: false,
                    stats: Arc::clone(&stats),
                    canonical_lineage: initial_canonical_lineage(runtime.latest_snapshot().point()),
                    canonical_log_source: CanonicalLogSource::default(),
                    attested_log_coverage: None,
                    buffered_logs: BTreeMap::new(),
                    pending_headers: BTreeMap::new(),
                    highest_observed_block: None,
                };
                let attach = runtime.attach_subscriber_control(control);
                tokio::pin!(attach);
                tokio::select! {
                    result = &mut attach => result?,
                    command = driver.commands.recv() => {
                        driver.handle_control(command.expect("adoption command")).await?;
                        attach.await?;
                    }
                }

                // Half one: the live stream hands the driver both logs for free.
                let batch = driver
                    .subscriber
                    .next_scoped_batch()
                    .await?
                    .expect("polling source must deliver its canonical logs");
                // This documents the `Reconcile` opt-out, which was the default
                // before subscription-sourced assembly landed.
                driver.canonical_log_source = CanonicalLogSource::Reconcile;
                assert_eq!(batch.records().len(), 2);
                driver.handle_batch(batch).await?;

                let after_stream = stats.snapshot();
                assert_eq!(
                    after_stream.subscription_logs_discarded(),
                    2,
                    "both delivered logs must be counted as discarded"
                );
                assert_eq!(after_stream.canonical_headers_ingested(), 0);
                assert_eq!(
                    after_stream.reconciliation_requests(),
                    0,
                    "a log-only batch must not trigger reconciliation on its own"
                );

                // Half two: the same logs are bought again, once, per block.
                let _commits = runtime.subscribe_changes().await?;
                driver.reconcile_and_deliver(delivered).await?;

                let after_reconcile = stats.snapshot();
                assert_eq!(
                    after_reconcile.reconciliation_requests(),
                    1,
                    "one hash-pinned eth_getLogs per delivered block per filter chunk"
                );
                assert_eq!(
                    after_reconcile.reconciliation_logs_fetched(),
                    after_reconcile.subscription_logs_discarded(),
                    "reconciliation re-fetches exactly the logs the stream already delivered"
                );
                assert_eq!(after_reconcile.canonical_blocks_delivered(), 1);
                assert_eq!(
                    after_reconcile.lineage_parent_requests(),
                    0,
                    "a straight-line successor needs no parent walk"
                );
                assert!(
                    asserter.read_q().is_empty(),
                    "the mocked transport must have served every queued response"
                );

                runtime.shutdown().await?;
                Ok(())
            })
            .await
    }

    /// A header observation must not be accepted as proof that an earlier
    /// block's log set closed.
    ///
    /// `newHeads` and `logs` are independent subscriptions. A header for 502
    /// says nothing about whether 501's logs have been delivered. Before the
    /// fix this sealed 501 carrying an empty log set and silently dropped the
    /// logs that arrived afterwards -- the exact silent loss this work exists
    /// to remove, reintroduced one layer up.
    ///
    /// The block here has a bloom that DOES admit the interest, so neither
    /// proof holds and the only correct outcome is to keep waiting.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_header_alone_never_seals_a_block_whose_logs_have_not_arrived() -> Result<()> {
        tokio::task::LocalSet::new()
            .run_until(async {
                let pool = Address::repeat_byte(0x77);
                let topic = keccak256(b"Swap()");
                let baseline_header = header(500, B256::repeat_byte(0x49));
                // The bloom admits the pool, so absence cannot be proven.
                let delivered = header_admitting(501, baseline_header.hash, pool);
                let block = BlockRef {
                    number: delivered.inner.number,
                    hash: delivered.hash,
                    parent_hash: Some(delivered.inner.parent_hash),
                    timestamp: Some(delivered.inner.timestamp),
                };

                let asserter = Asserter::new();
                let (mut driver, runtime, _stats) =
                    subscription_driver(asserter.clone(), baseline_header.clone()).await?;
                let _commits = runtime.subscribe_changes().await?;

                // Coverage is attested (fork-cache derives it from the header
                // stream), and a later HEADER has arrived. Neither is proof
                // that 501's logs were delivered.
                driver.note_log_coverage(&[ChainControl::LogCoverage(block)]);
                driver
                    .pending_headers
                    .insert(block.number, (delivered.clone(), Instant::now()));

                assert!(
                    driver
                        .subscription_seal_reason(block.number, &delivered)
                        .is_none(),
                    "an attested block whose logs have not arrived must not be sealable"
                );

                // A log for a LATER block is the sound proof, and only it flips
                // the decision.
                let successor = header(502, delivered.hash);
                driver.buffer_subscription_log(reconciled_log(pool, topic, &successor, 0))?;
                assert_eq!(
                    driver.subscription_seal_reason(block.number, &delivered),
                    Some(SealReason::Successor),
                    "a log observed on the log stream above 501 closes 501's set"
                );

                runtime.shutdown().await?;
                Ok(())
            })
            .await
    }

    /// The quiet-block path: a bloom that excludes every registered interest is
    /// a positive proof that no matching log exists, so the block seals with no
    /// provider request and without waiting for the straggler window. This is
    /// what keeps an idle pool off the provider now that a timer no longer
    /// authorises publishing.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_bloom_excluding_every_interest_seals_immediately() -> Result<()> {
        tokio::task::LocalSet::new()
            .run_until(async {
                let baseline_header = header(500, B256::repeat_byte(0x49));
                // Default bloom is empty, so it excludes the registered pool.
                let delivered = header(501, baseline_header.hash);
                let block = BlockRef {
                    number: delivered.inner.number,
                    hash: delivered.hash,
                    parent_hash: Some(delivered.inner.parent_hash),
                    timestamp: Some(delivered.inner.timestamp),
                };

                let asserter = Asserter::new();
                let (mut driver, runtime, stats) =
                    subscription_driver(asserter.clone(), baseline_header.clone()).await?;
                let _commits = runtime.subscribe_changes().await?;

                driver.note_log_coverage(&[ChainControl::LogCoverage(block)]);
                driver
                    .pending_headers
                    .insert(block.number, (delivered.clone(), Instant::now()));
                assert_eq!(
                    driver.subscription_seal_reason(block.number, &delivered),
                    Some(SealReason::BloomAbsent),
                    "an empty bloom proves there is no matching log to wait for"
                );

                driver
                    .drain_sealable(Instant::now(), Duration::ZERO)
                    .await?;
                let sealed = stats.snapshot();
                assert_eq!(sealed.blocks_sealed_from_subscription(), 1);
                assert_eq!(sealed.blocks_sealed_by_bloom_absence(), 1);
                assert_eq!(sealed.reconciliation_requests(), 0);
                assert!(
                    asserter.read_q().is_empty(),
                    "the quiet path must touch no provider"
                );

                runtime.shutdown().await?;
                Ok(())
            })
            .await
    }

    /// The headline property of Phase 3: a block whose logs the subscription
    /// already delivered, and whose completeness the subscriber attested, is
    /// submitted without touching the provider. The mocked transport is left
    /// with its queue untouched, which is the assertion — not an absence of
    /// mocks, but proof that none were consumed.
    #[tokio::test(flavor = "multi_thread")]
    async fn attested_block_is_sealed_from_the_subscription_with_zero_requests() -> Result<()> {
        tokio::task::LocalSet::new()
            .run_until(async {
                let pool = Address::repeat_byte(0x77);
                let topic = keccak256(b"Swap()");
                let baseline_header = header(500, B256::repeat_byte(0x49));
                let delivered = header(501, baseline_header.hash);
                let block = BlockRef {
                    number: delivered.inner.number,
                    hash: delivered.hash,
                    parent_hash: Some(delivered.inner.parent_hash),
                    timestamp: Some(delivered.inner.timestamp),
                };

                let asserter = Asserter::new();
                let (mut driver, runtime, stats) =
                    subscription_driver(asserter.clone(), baseline_header.clone()).await?;
                let _commits = runtime.subscribe_changes().await?;

                // The subscription delivers the block's logs and the subscriber
                // attests that nothing was lost at or below it.
                driver.note_log_coverage(&[ChainControl::LogCoverage(block)]);
                driver.buffer_subscription_log(reconciled_log(pool, topic, &delivered, 0))?;
                driver.buffer_subscription_log(reconciled_log(pool, topic, &delivered, 1))?;
                driver
                    .pending_headers
                    .insert(block.number, (delivered, Instant::now()));
                // A strictly later observation proves 501's log set is closed.
                driver.note_observed_block(502);

                driver
                    .drain_sealable(Instant::now(), Duration::ZERO)
                    .await?;

                let stats = stats.snapshot();
                assert_eq!(stats.blocks_sealed_from_subscription(), 1);
                assert_eq!(stats.blocks_sealed_by_successor(), 1);
                assert_eq!(stats.blocks_sealed_by_bloom_absence(), 0);
                assert_eq!(stats.subscription_logs_applied(), 2);
                assert_eq!(
                    stats.blocks_reconciled(),
                    0,
                    "an attested block must not be re-fetched"
                );
                assert_eq!(stats.canonical_blocks_delivered(), 1);
                assert!(
                    asserter.read_q().is_empty(),
                    "sealing from the subscription must issue no provider request"
                );

                runtime.shutdown().await?;
                Ok(())
            })
            .await
    }

    /// Without an attestation covering it, the delivered set is unproven, so the
    /// block still takes the hash-pinned path. This is what keeps correctness
    /// independent of whether the stream happened to be whole.
    #[tokio::test(flavor = "multi_thread")]
    async fn unattested_block_falls_back_to_reconciliation() -> Result<()> {
        tokio::task::LocalSet::new()
            .run_until(async {
                let pool = Address::repeat_byte(0x77);
                let topic = keccak256(b"Swap()");
                let baseline_header = header(500, B256::repeat_byte(0x49));
                let delivered = header(501, baseline_header.hash);

                let asserter = Asserter::new();
                let (mut driver, runtime, stats) =
                    subscription_driver(asserter.clone(), baseline_header.clone()).await?;
                // The reconciliation fetch the fallback is expected to issue.
                asserter.push_success(&vec![reconciled_log(pool, topic, &delivered, 0)]);
                let _commits = runtime.subscribe_changes().await?;

                // Logs arrive, but no attestation does.
                driver.buffer_subscription_log(reconciled_log(pool, topic, &delivered, 0))?;
                driver
                    .pending_headers
                    .insert(501, (delivered, Instant::now() - Duration::from_secs(1)));
                driver.note_observed_block(502);

                driver
                    .drain_sealable(Instant::now(), Duration::ZERO)
                    .await?;
                let stats = stats.snapshot();
                assert_eq!(
                    stats.blocks_sealed_from_subscription(),
                    0,
                    "an unattested block must never be sealed from the stream"
                );
                assert_eq!(stats.blocks_reconciled(), 1);
                assert_eq!(stats.reconciliation_requests(), 1);
                assert!(asserter.read_q().is_empty());

                runtime.shutdown().await?;
                Ok(())
            })
            .await
    }

    /// A quiet tail still makes progress with no successor observed -- but on
    /// the strength of the block's own empty bloom, not a timer. Renamed from
    /// `grace_window_seals_...` when the timer stopped authorising publication;
    /// the body always exercised the empty-bloom path, because the test header
    /// carries no accrued bloom.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_quiet_tail_seals_on_its_empty_bloom_not_on_a_timer() -> Result<()> {
        tokio::task::LocalSet::new()
            .run_until(async {
                let baseline_header = header(500, B256::repeat_byte(0x49));
                let delivered = header(501, baseline_header.hash);
                let block = BlockRef {
                    number: delivered.inner.number,
                    hash: delivered.hash,
                    parent_hash: Some(delivered.inner.parent_hash),
                    timestamp: Some(delivered.inner.timestamp),
                };

                let asserter = Asserter::new();
                let (mut driver, runtime, stats) =
                    subscription_driver(asserter.clone(), baseline_header.clone()).await?;
                let _commits = runtime.subscribe_changes().await?;

                driver.note_log_coverage(&[ChainControl::LogCoverage(block)]);
                // No later block observed; the window has already elapsed.
                driver
                    .pending_headers
                    .insert(501, (delivered, Instant::now() - Duration::from_secs(1)));

                driver
                    .drain_sealable(Instant::now(), Duration::ZERO)
                    .await?;

                let stats = stats.snapshot();
                assert_eq!(stats.blocks_sealed_by_bloom_absence(), 1);
                assert_eq!(stats.blocks_sealed_by_successor(), 0);
                assert_eq!(stats.blocks_reconciled(), 0);
                assert!(asserter.read_q().is_empty());

                runtime.shutdown().await?;
                Ok(())
            })
            .await
    }

    /// The second exclusion path: the bloom admits the interest's ADDRESS by
    /// false positive but carries none of its topics, so no matching log can
    /// exist and the block still seals without a request. This is the half of
    /// the test that recovers the busy-chain blocks an address-only check
    /// leaves unprovable.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_bloom_admitting_only_the_address_still_excludes_on_topics() -> Result<()> {
        tokio::task::LocalSet::new()
            .run_until(async {
                let pool = Address::repeat_byte(0x77);
                let baseline_header = header(500, B256::repeat_byte(0x49));
                // Address present, registered topic absent.
                let delivered = header_admitting(501, baseline_header.hash, pool);
                let block = BlockRef {
                    number: delivered.inner.number,
                    hash: delivered.hash,
                    parent_hash: Some(delivered.inner.parent_hash),
                    timestamp: Some(delivered.inner.timestamp),
                };

                let asserter = Asserter::new();
                // The interest declares a topic, which is what makes the second
                // exclusion path reachable at all.
                let (mut driver, runtime, stats) = subscription_driver_with_filter(
                    asserter.clone(),
                    baseline_header.clone(),
                    Filter::new()
                        .address(pool)
                        .event_signature(keccak256(b"Swap()")),
                )
                .await?;
                let _commits = runtime.subscribe_changes().await?;

                driver.note_log_coverage(&[ChainControl::LogCoverage(block)]);
                driver
                    .pending_headers
                    .insert(block.number, (delivered.clone(), Instant::now()));

                assert_eq!(
                    driver.subscription_seal_reason(block.number, &delivered),
                    Some(SealReason::BloomAbsent),
                    "an admitted address with no admitted topic still proves absence"
                );
                driver
                    .drain_sealable(Instant::now(), Duration::ZERO)
                    .await?;
                let sealed = stats.snapshot();
                assert_eq!(sealed.blocks_sealed_by_bloom_absence(), 1);
                assert_eq!(sealed.reconciliation_requests(), 0);
                assert!(asserter.read_q().is_empty(), "no provider request");

                runtime.shutdown().await?;
                Ok(())
            })
            .await
    }

    /// The behaviour change that makes sealing sound: when the bloom ADMITS an
    /// interest and the log stream has not moved past the block, an expired
    /// straggler window must reconcile rather than publish whatever happens to
    /// be buffered. Before the fix this published a partial set.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_expired_window_reconciles_rather_than_publishing_a_partial_buffer() -> Result<()> {
        tokio::task::LocalSet::new()
            .run_until(async {
                let pool = Address::repeat_byte(0x77);
                let topic = keccak256(b"Swap()");
                let baseline_header = header(500, B256::repeat_byte(0x49));
                // The bloom admits the pool, so absence cannot be proven and
                // the only sound proof would be a later log.
                let delivered = header_admitting(501, baseline_header.hash, pool);
                let block = BlockRef {
                    number: delivered.inner.number,
                    hash: delivered.hash,
                    parent_hash: Some(delivered.inner.parent_hash),
                    timestamp: Some(delivered.inner.timestamp),
                };

                let asserter = Asserter::new();
                let (mut driver, runtime, stats) =
                    subscription_driver(asserter.clone(), baseline_header.clone()).await?;
                let _commits = runtime.subscribe_changes().await?;
                // One reconciliation response for the fetch we expect it to make.
                asserter.push_success(&Vec::<RpcLog>::new());

                driver.note_log_coverage(&[ChainControl::LogCoverage(block)]);
                // Only a partial set has arrived -- log index 0 of two.
                driver.buffer_subscription_log(reconciled_log(pool, topic, &delivered, 0))?;
                driver.pending_headers.insert(
                    block.number,
                    (delivered, Instant::now() - Duration::from_secs(3600)),
                );

                driver
                    .drain_sealable(Instant::now(), Duration::ZERO)
                    .await?;

                let sealed = stats.snapshot();
                assert_eq!(
                    sealed.blocks_sealed_from_subscription(),
                    0,
                    "an expired window must not publish the partial buffer"
                );
                assert_eq!(
                    sealed.blocks_reconciled(),
                    1,
                    "it must fall back to a hash-pinned reconciliation instead"
                );
                assert!(
                    sealed.reconciliation_requests() >= 1,
                    "and that reconciliation must actually reach the provider"
                );

                runtime.shutdown().await?;
                Ok(())
            })
            .await
    }

    /// A block that is neither attested nor past its window waits, rather than
    /// being reconciled prematurely — otherwise the fetch would never actually
    /// be avoided when the header simply arrives before its logs.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_block_still_inside_its_window_waits_instead_of_reconciling() -> Result<()> {
        tokio::task::LocalSet::new()
            .run_until(async {
                let baseline_header = header(500, B256::repeat_byte(0x49));
                let delivered = header(501, baseline_header.hash);

                let asserter = Asserter::new();
                let (mut driver, runtime, stats) =
                    subscription_driver(asserter.clone(), baseline_header.clone()).await?;

                driver
                    .pending_headers
                    .insert(501, (delivered, Instant::now()));

                driver
                    .drain_sealable(Instant::now(), Duration::ZERO)
                    .await?;

                let stats = stats.snapshot();
                assert_eq!(stats.blocks_reconciled(), 0);
                assert_eq!(stats.blocks_sealed_from_subscription(), 0);
                assert!(
                    driver.pending_headers.contains_key(&501),
                    "the block must remain pending until its log set is provably closed"
                );
                assert!(driver.next_seal_deadline().is_some());
                assert!(asserter.read_q().is_empty());

                runtime.shutdown().await?;
                Ok(())
            })
            .await
    }

    /// `Reconcile` is the documented opt-out and must behave exactly as before:
    /// logs are discarded and every block is re-fetched.
    #[tokio::test(flavor = "multi_thread")]
    async fn reconcile_mode_still_discards_and_refetches() -> Result<()> {
        tokio::task::LocalSet::new()
            .run_until(async {
                let pool = Address::repeat_byte(0x77);
                let topic = keccak256(b"Swap()");
                let baseline_header = header(500, B256::repeat_byte(0x49));
                let delivered = header(501, baseline_header.hash);

                let asserter = Asserter::new();
                let (mut driver, runtime, stats) =
                    subscription_driver(asserter.clone(), baseline_header.clone()).await?;
                driver.canonical_log_source = CanonicalLogSource::Reconcile;
                asserter.push_success(&vec![reconciled_log(pool, topic, &delivered, 0)]);
                let _commits = runtime.subscribe_changes().await?;

                assert!(!driver.buffers_subscription_logs());
                assert!(driver.next_seal_deadline().is_none());

                driver
                    .deliver_through_with_timing(delivered, Instant::now(), Duration::ZERO)
                    .await?;

                let stats = stats.snapshot();
                assert_eq!(stats.reconciliation_requests(), 1);
                assert_eq!(stats.blocks_sealed_from_subscription(), 0);
                assert!(asserter.read_q().is_empty());

                runtime.shutdown().await?;
                Ok(())
            })
            .await
    }

    /// Driver wired for subscription-sourced assembly, attached to a live
    /// runtime, with its counters exposed for assertions.
    #[allow(clippy::type_complexity)]
    /// Harness with an explicit provider filter, so a test can exercise the
    /// topic-exclusion path that an address-only filter cannot reach.
    async fn subscription_driver_with_filter(
        asserter: Asserter,
        baseline_header: RpcHeader,
        provider_filter: Filter,
    ) -> Result<(
        AlloyAmmSubscriberDriver<impl alloy_provider::Provider<Ethereum> + Clone + 'static>,
        AmmRuntimeHandle,
        Arc<AmmSubscriberDriverCounters>,
    )> {
        subscription_driver_inner(asserter, baseline_header, provider_filter).await
    }

    async fn subscription_driver(
        asserter: Asserter,
        baseline_header: RpcHeader,
    ) -> Result<(
        AlloyAmmSubscriberDriver<impl alloy_provider::Provider<Ethereum> + Clone + 'static>,
        AmmRuntimeHandle,
        Arc<AmmSubscriberDriverCounters>,
    )> {
        subscription_driver_inner(
            asserter,
            baseline_header,
            Filter::new().address(Address::repeat_byte(0x77)),
        )
        .await
    }

    async fn subscription_driver_inner(
        asserter: Asserter,
        baseline_header: RpcHeader,
        provider_filter: Filter,
    ) -> Result<(
        AlloyAmmSubscriberDriver<impl alloy_provider::Provider<Ethereum> + Clone + 'static>,
        AmmRuntimeHandle,
        Arc<AmmSubscriberDriverCounters>,
    )> {
        // Registering an interest resolves chain identity; streams install
        // lazily on first poll, which these tests never reach. Seed it before
        // anything a test queues, because the asserter is FIFO.
        asserter.push_success(&U256::from(1));
        let mut cache = setup_cache().await;
        cache.advance_block(&baseline_header)?;
        let runtime = AmmRuntime::spawn(
            cache,
            AdapterRegistry::new(),
            AmmRuntimeBaseline::from_verified_header(1, baseline_header.clone())?,
            AmmRuntimeConfig::default(),
        )?;
        let provider = ProviderBuilder::new().connect_mocked_client(asserter);
        let mut subscriber = AlloySubscriber::new(
            provider,
            SubscriberMode::Polling,
            SubscriberConfig::default(),
        );
        // Reconciliation derives its filters from registered interests; without
        // one it would issue no request and the fallback would look free.
        subscriber
            .register_interests(&[ReactiveInterest::Logs(LogInterest {
                provider_filter,
                local_matcher: None,
                route_key: None,
            })])
            .await?;
        let (command_tx, command_rx) = mpsc::channel(4);
        let (state, _) = watch::channel(AmmSubscriberDriverState::Paused);
        let control = AmmSubscriberControl {
            commands: command_tx,
        };
        let stats = Arc::new(AmmSubscriberDriverCounters::default());
        let mut driver = AlloyAmmSubscriberDriver {
            runtime: runtime.clone(),
            subscriber,
            initial_interests: Vec::new(),
            commands: command_rx,
            state,
            paused: true,
            interest_revision: 0,
            owners: HashMap::new(),
            pending: None,
            next_transaction: 0,
            max_addresses_per_get_logs: 256,
            preconfirmation_rejection_policy: AmmPreconfirmationRejectionPolicy::FailDriver,
            report_stop: true,
            stop_requested: false,
            stats: Arc::clone(&stats),
            canonical_lineage: initial_canonical_lineage(runtime.latest_snapshot().point()),
            canonical_log_source: CanonicalLogSource::default(),
            attested_log_coverage: None,
            buffered_logs: BTreeMap::new(),
            pending_headers: BTreeMap::new(),
            highest_observed_block: None,
        };
        {
            let attach = runtime.attach_subscriber_control(control);
            tokio::pin!(attach);
            tokio::select! {
                result = &mut attach => result?,
                command = driver.commands.recv() => {
                    driver.handle_control(command.expect("adoption command")).await?;
                    attach.await?;
                }
            }
        }
        Ok((driver, runtime, stats))
    }

    /// A header whose `logsBloom` admits `address`, built before the hash is
    /// computed so the advertised hash stays consistent with the contents.
    fn header_admitting(number: u64, parent_hash: B256, address: Address) -> RpcHeader {
        let mut inner = ConsensusHeader {
            parent_hash,
            number,
            timestamp: 1_700_000_000 + number,
            base_fee_per_gas: Some(100 + number),
            beneficiary: Address::repeat_byte(0xcb),
            gas_limit: 30_000_000,
            mix_hash: B256::repeat_byte(0xab),
            ..ConsensusHeader::default()
        };
        inner
            .logs_bloom
            .accrue(alloy_primitives::BloomInput::Raw(address.as_slice()));
        RpcHeader::new(inner)
    }

    fn reconciled_log(address: Address, topic: B256, block: &RpcHeader, log_index: u64) -> RpcLog {
        RpcLog {
            inner: PrimitiveLog::new_unchecked(address, vec![topic], Bytes::new()),
            block_hash: Some(block.hash),
            block_number: Some(block.inner.number),
            block_timestamp: Some(block.inner.timestamp),
            transaction_hash: Some(B256::repeat_byte(0x30 + log_index as u8)),
            transaction_index: Some(log_index),
            log_index: Some(log_index),
            removed: false,
        }
    }
}
