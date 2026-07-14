//! Alloy-specific subscriber owner and complete-block driver.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;

use alloy_network::{Ethereum, primitives::BlockResponse as _};
use alloy_provider::Provider;
use alloy_rpc_types_eth::{Filter, Header as RpcHeader, Log as RpcLog};
use evm_fork_cache::reactive::{
    AlloySubscriber, BlockInterest, BlockRef, ChainStatus, EventSubscriber, HandlerId, InputSource,
    ReactiveContext, ReactiveInput, ReactiveInputBatch, ReactiveInputRecord, ReactiveInterest,
    SubscriberDriverPoll, SubscriberMode, SubscriberOwnerEpoch, SubscriberOwnerError,
    SubscriberOwnerStart, SubscriberOwnerState,
};
use tokio::sync::{mpsc, oneshot, watch};

use super::{
    AmmCanonicalBatch, AmmCanonicalBatchError, AmmPoolSubscriptionPlan, AmmRuntimeCommandError,
    AmmRuntimeHandle, AmmStatePoint,
};

/// Configuration for the Alloy subscriber driver.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AmmSubscriberDriverConfig {
    control_capacity: usize,
    max_addresses_per_get_logs: usize,
}

impl Default for AmmSubscriberDriverConfig {
    fn default() -> Self {
        Self {
            control_capacity: 32,
            max_addresses_per_get_logs: 256,
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

/// Public status/shutdown handle for an attached subscriber task.
#[derive(Clone)]
pub struct AmmSubscriberDriverHandle {
    control: AmmSubscriberControl,
    state: watch::Receiver<AmmSubscriberDriverState>,
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
    mut subscriber: AlloySubscriber<P, Ethereum>,
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
    let mut base_interests = subscriber.registered_interests().to_vec();
    if !base_interests
        .iter()
        .any(|interest| matches!(interest, ReactiveInterest::Blocks(_)))
    {
        base_interests.push(ReactiveInterest::Blocks(BlockInterest::default()));
    }
    subscriber.register_interests(&base_interests)?;

    let (command_tx, command_rx) = mpsc::channel(config.control_capacity);
    let (state_tx, state_rx) = watch::channel(AmmSubscriberDriverState::Paused);
    let canonical_lineage = initial_canonical_lineage(runtime.latest_snapshot().point());
    let control = AmmSubscriberControl {
        commands: command_tx,
    };
    let actor = AlloyAmmSubscriberDriver {
        runtime,
        subscriber,
        commands: command_rx,
        state: state_tx,
        paused: true,
        interest_revision: 0,
        owners: HashMap::new(),
        pending: None,
        next_transaction: 0,
        max_addresses_per_get_logs: config.max_addresses_per_get_logs,
        report_stop: true,
        stop_requested: false,
        canonical_lineage,
    };
    tokio::spawn(actor.run());
    Ok((
        control.clone(),
        AmmSubscriberDriverHandle {
            control,
            state: state_rx,
        },
    ))
}

struct AlloyAmmSubscriberDriver<P> {
    runtime: AmmRuntimeHandle,
    subscriber: AlloySubscriber<P, Ethereum>,
    commands: mpsc::Receiver<SubscriberControlCommand>,
    state: watch::Sender<AmmSubscriberDriverState>,
    paused: bool,
    interest_revision: u64,
    owners: HashMap<HandlerId, SubscriberOwnerEpoch>,
    pending: Option<PendingSubscriberTransaction>,
    next_transaction: u64,
    max_addresses_per_get_logs: usize,
    report_stop: bool,
    stop_requested: bool,
    canonical_lineage: BTreeMap<u64, alloy_primitives::B256>,
}

const RETAINED_CANONICAL_LINEAGE: usize = 65;

fn initial_canonical_lineage(point: AmmStatePoint) -> BTreeMap<u64, alloy_primitives::B256> {
    BTreeMap::from([(point.block_number(), point.block_hash())])
}

impl<P> AlloyAmmSubscriberDriver<P>
where
    P: Provider<Ethereum> + Clone + Send + Sync + 'static,
{
    async fn run(mut self) {
        let result = self.run_inner().await;
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

            let mut control = Box::pin(self.commands.recv());
            let outcome = self
                .subscriber
                .next_scoped_batch_or(control.as_mut())
                .await?;
            drop(control);
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
            SubscriberOwnerStart::PostBlock(block.clone()),
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
                SubscriberOwnerStart::PostBlock(block.clone()),
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
        let mut headers = Vec::new();
        for scoped in batch.into_records() {
            if !scoped.scope().is_canonical() {
                return Err(AmmSubscriberDriverError::OwnerCatchupRequiresStaging);
            }
            match scoped.into_record().input {
                ReactiveInput::BlockHeader(header) => headers.push(header),
                ReactiveInput::Log(_)
                | ReactiveInput::FullBlock(_)
                | ReactiveInput::PendingTxHash(_)
                | ReactiveInput::PendingTx(_) => {}
            }
        }
        headers.sort_by_key(|header| header.inner.number);
        for header in headers {
            self.deliver_through(header).await?;
        }
        Ok(())
    }

    async fn deliver_through(&mut self, header: RpcHeader) -> Result<(), AmmSubscriberDriverError> {
        let current = self.runtime.latest_snapshot().point();
        if header.inner.number == current.block_number() && header.hash == current.block_hash() {
            return Ok(());
        }
        for header in self.delivery_lineage(header).await? {
            let number = header.inner.number;
            let hash = header.hash;
            self.reconcile_and_deliver(header).await?;
            self.record_canonical_block(number, hash);
        }
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

    async fn reconcile_and_deliver(
        &mut self,
        header: RpcHeader,
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
            for log in self
                .subscriber
                .provider()
                .get_logs(&filter.at_block_hash(block.hash))
                .await
                .map_err(|error| AmmSubscriberDriverError::Provider(error.to_string()))?
            {
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
                        block: block.clone(),
                        confirmations: 0,
                    },
                    block: Some(block.clone()),
                    transaction_index: log.transaction_index,
                    log_index: log.log_index,
                };
                ReactiveInputRecord::new(ReactiveInput::Log(log), context)
            })
            .collect();
        let batch = AmmCanonicalBatch::from_verified_block(
            point.chain_id(),
            header,
            self.interest_revision,
            ReactiveInputBatch::new(records),
        )?;
        self.ingest_while_servicing_controls(batch).await?;
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
        AlloySubscriber, BlockRef, ChainStatus, InputSource, ReactiveContext, ReactiveInput,
        ReactiveInputBatch, ReactiveInputRecord, SubscriberConfig, SubscriberMode,
    };
    use tokio::sync::{mpsc, watch};

    use super::{
        AlloyAmmSubscriberDriver, AmmSubscriberControl, AmmSubscriberDriverState,
        AmmSubscriberOwnerPlan, SubscriberControlCommand, SubscriberTransaction,
        initial_canonical_lineage, reconciliation_filters,
    };
    use crate::adapters::{
        AdapterRegistry, AmmAdapter, AmmCanonicalBatch, AmmColdStartWorkerConfig,
        AmmFactoryWatcherRegistration, AmmRuntime, AmmRuntimeBaseline, AmmRuntimeCommandError,
        AmmRuntimeConfig, AmmRuntimeEventKind, CustomPoolKey, DiscoveryOwnerKey, EventSource,
        FactoryConfig, PoolDiscovery, PoolKey, PoolRegistration, PoolRuntimeState,
        PoolStateDependencies, PoolStatus, ProtocolId, UniswapV2Adapter,
        uniswap_v2_pair_runtime_code_hash,
    };

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
                    block: block.clone(),
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
                reconciliation.push_success(&U256::from(1));
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
                    commands: command_rx,
                    state,
                    paused: true,
                    interest_revision: 0,
                    owners: HashMap::new(),
                    pending: None,
                    next_transaction: 0,
                    max_addresses_per_get_logs: 256,
                    report_stop: true,
                    stop_requested: false,
                    canonical_lineage: initial_canonical_lineage(runtime.latest_snapshot().point()),
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
                    commands: command_rx,
                    state,
                    paused: true,
                    interest_revision: 0,
                    owners: HashMap::new(),
                    pending: None,
                    next_transaction: 0,
                    max_addresses_per_get_logs: 256,
                    report_stop: true,
                    stop_requested: false,
                    canonical_lineage: initial_canonical_lineage(runtime.latest_snapshot().point()),
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
                    commands: command_rx,
                    state,
                    paused: false,
                    interest_revision: 0,
                    owners: HashMap::new(),
                    pending: None,
                    next_transaction: 0,
                    max_addresses_per_get_logs: 256,
                    report_stop: true,
                    stop_requested: false,
                    canonical_lineage: initial_canonical_lineage(runtime.latest_snapshot().point()),
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
}
