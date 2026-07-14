//! Bounded background provider-work scheduler for cold start, discovery, and repair.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use alloy_network::AnyNetwork;
use alloy_provider::Provider;
use tokio::sync::{mpsc, watch};
use tokio::task::{AbortHandle, Id, JoinSet};

use evm_fork_cache::cold_start::{
    AccountProofRoundFetcher, StorageRoundFetcher, StorageRoundRequest,
};
use evm_fork_cache::{StorageBatchConfig, StorageFetchStrategy};

use super::cold_start::{
    PreparedPoolFetchers, PreparedPoolJob, PreparedPoolStep, PreparedSnapshotState,
    ResumableColdStartConfig, prepare_fast_pool, prepare_verified_code_targets,
};
use super::{
    AdapterRegistrySnapshot, AmmPreparedStorage, AmmRuntimeCommandError, AmmRuntimeHandle,
    AmmStatePoint, AmmWorkClass, ColdStartPolicy, DiscoveryOwnerId, PoolDiscovery, PoolInstanceId,
    PoolRegistration, PreparedDiscoveryReads, RuntimeWorkId, TokenEdgeDiscoveryReport,
    TokenEdgeDiscoveryRequest,
};

/// Maximum number of consecutive quanta granted to the same highest-priority
/// class while a lower-priority class is waiting. This bounded priority budget
/// preserves normal strict priority while guaranteeing eventual service for
/// bootstrap and deferred work under a sustained high-priority load.
const PRIORITY_BURST_QUANTA: usize = 8;

/// Scheduling policy for one progressively published cold-start batch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AmmColdStartOptions {
    class: AmmWorkClass,
    policy: ColdStartPolicy,
}

impl Default for AmmColdStartOptions {
    fn default() -> Self {
        Self {
            class: AmmWorkClass::Bootstrap,
            policy: ColdStartPolicy::Eager,
        }
    }
}

impl AmmColdStartOptions {
    /// Select the scheduler service class.
    pub const fn with_class(mut self, class: AmmWorkClass) -> Self {
        self.class = class;
        self
    }

    /// Select adapter cold-start policy.
    pub const fn with_policy(mut self, policy: ColdStartPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Scheduler service class.
    pub const fn class(self) -> AmmWorkClass {
        self.class
    }

    /// Adapter cold-start policy.
    pub const fn policy(self) -> ColdStartPolicy {
        self.policy
    }
}

/// One accepted pending pool generation and its cold-start attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AmmScheduledPool {
    pool: PoolInstanceId,
    work: RuntimeWorkId,
}

impl AmmScheduledPool {
    pub(crate) const fn new(pool: PoolInstanceId, work: RuntimeWorkId) -> Self {
        Self { pool, work }
    }

    /// Reserved pool generation.
    pub const fn pool(&self) -> &PoolInstanceId {
        &self.pool
    }

    /// Generation-owned work attempt.
    pub const fn work(&self) -> &RuntimeWorkId {
        &self.work
    }
}

/// Bounded worker configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AmmColdStartWorkerConfig {
    queue_capacity: usize,
    max_concurrency: usize,
    storage_batch_config: StorageBatchConfig,
    storage_fetch_strategy: StorageFetchStrategy,
}

impl Default for AmmColdStartWorkerConfig {
    fn default() -> Self {
        Self {
            queue_capacity: 32,
            max_concurrency: 8,
            storage_batch_config: StorageBatchConfig::default(),
            storage_fetch_strategy: StorageFetchStrategy::default(),
        }
    }
}

impl AmmColdStartWorkerConfig {
    /// Bound accepted jobs across cold start, discovery, and deferred repair.
    pub const fn with_queue_capacity(mut self, capacity: usize) -> Self {
        self.queue_capacity = capacity;
        self
    }

    /// Bound simultaneous provider fetches.
    pub const fn with_max_concurrency(mut self, concurrency: usize) -> Self {
        self.max_concurrency = concurrency;
        self
    }

    /// Configure classic point-read batching and the fallback path used by
    /// bulk storage extraction.
    pub const fn with_storage_batch_config(mut self, config: StorageBatchConfig) -> Self {
        self.storage_batch_config = config;
        self
    }

    /// Configure provider storage extraction, including bulk-call request
    /// limits.
    pub const fn with_storage_fetch_strategy(mut self, strategy: StorageFetchStrategy) -> Self {
        self.storage_fetch_strategy = strategy;
        self
    }

    /// Shared accepted-job capacity.
    pub const fn queue_capacity(self) -> usize {
        self.queue_capacity
    }

    /// Provider concurrency ceiling.
    pub const fn max_concurrency(self) -> usize {
        self.max_concurrency
    }

    /// Point-read batch configuration used by every worker fetch path.
    pub const fn storage_batch_config(self) -> StorageBatchConfig {
        self.storage_batch_config
    }

    /// Storage extraction strategy used by every worker fetch path.
    pub const fn storage_fetch_strategy(self) -> StorageFetchStrategy {
        self.storage_fetch_strategy
    }
}

/// Recoverable worker state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum AmmColdStartWorkerState {
    /// Worker accepts and dispatches jobs.
    Running,
    /// Worker was stopped.
    Stopped,
}

/// Worker construction/control failure.
#[derive(Debug)]
#[non_exhaustive]
pub enum AmmColdStartWorkerError {
    /// A bounded capacity or concurrency was zero.
    ZeroCapacity,
    /// Worker queue is full.
    Full,
    /// Worker queue is closed.
    Closed,
    /// Runtime rejected worker attachment or work.
    Runtime(Box<AmmRuntimeCommandError>),
}

impl fmt::Display for AmmColdStartWorkerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroCapacity => write!(f, "cold-start worker capacities must be non-zero"),
            Self::Full => write!(f, "cold-start worker queue is full"),
            Self::Closed => write!(f, "cold-start worker is closed"),
            Self::Runtime(error) => write!(f, "cold-start runtime rejected work: {error}"),
        }
    }
}

impl std::error::Error for AmmColdStartWorkerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Runtime(error) => Some(error.as_ref()),
            _ => None,
        }
    }
}

impl From<AmmRuntimeCommandError> for AmmColdStartWorkerError {
    fn from(error: AmmRuntimeCommandError) -> Self {
        Self::Runtime(Box::new(error))
    }
}

/// Status and shutdown handle for the provider worker.
#[derive(Clone)]
pub struct AmmColdStartWorkerHandle {
    shutdown: watch::Sender<bool>,
    state: watch::Receiver<AmmColdStartWorkerState>,
}

impl AmmColdStartWorkerHandle {
    /// Latest worker state.
    pub fn latest_state(&self) -> AmmColdStartWorkerState {
        *self.state.borrow()
    }

    /// Subscribe to latest-value worker state.
    pub fn subscribe_state(&self) -> watch::Receiver<AmmColdStartWorkerState> {
        self.state.clone()
    }

    /// Stop new dispatch and make in-flight results cancellation-safe.
    pub fn shutdown(&self) {
        self.shutdown.send_replace(true);
    }
}

#[derive(Clone)]
pub(crate) struct AmmColdStartWorkerControl {
    commands: mpsc::UnboundedSender<WorkerCommand>,
    shutdown: watch::Sender<bool>,
    accepted_jobs: Arc<AtomicUsize>,
    queue_capacity: usize,
}

#[derive(Clone)]
struct AdmissionLease {
    accepted_jobs: Arc<AtomicUsize>,
    held: Arc<AtomicBool>,
}

impl AdmissionLease {
    fn new(accepted_jobs: Arc<AtomicUsize>) -> Self {
        Self {
            accepted_jobs,
            held: Arc::new(AtomicBool::new(true)),
        }
    }

    /// Release this job's shared-capacity reservation exactly once.
    fn release(&self) -> bool {
        if self.held.swap(false, Ordering::AcqRel) {
            self.accepted_jobs.fetch_sub(1, Ordering::AcqRel);
            true
        } else {
            false
        }
    }
}

impl AmmColdStartWorkerControl {
    pub(crate) fn shutdown_for_runtime(&self) {
        self.shutdown.send_replace(true);
    }

    pub(crate) fn submit(&self, jobs: Vec<AmmColdStartJob>) -> Result<(), AmmColdStartWorkerError> {
        if *self.shutdown.borrow() {
            return Err(AmmColdStartWorkerError::Closed);
        }
        if jobs.is_empty() {
            return Ok(());
        }
        self.submit_command(WorkerCommand::ColdStartBatch(jobs))
    }

    /// Atomically admit one connector-discovery quantum into the shared bound.
    pub(crate) fn submit_discovery(
        &self,
        job: AmmDiscoveryJob,
    ) -> Result<(), AmmColdStartWorkerError> {
        self.submit_command(WorkerCommand::Discovery(Box::new(job)))
    }

    /// Atomically admit one deferred slot-patch quantum into the shared bound.
    pub(crate) fn submit_slot_patch(
        &self,
        job: AmmSlotPatchJob,
    ) -> Result<(), AmmColdStartWorkerError> {
        self.submit_command(WorkerCommand::SlotPatch(Box::new(job)))
    }

    /// Promptly remove or abort one exact queued/in-flight work attempt.
    pub(crate) fn cancel(&self, work: RuntimeWorkId) -> Result<(), AmmColdStartWorkerError> {
        if *self.shutdown.borrow() {
            return Err(AmmColdStartWorkerError::Closed);
        }
        self.commands
            .send(WorkerCommand::Cancel(work))
            .map_err(|_| AmmColdStartWorkerError::Closed)
    }

    fn submit_command(&self, command: WorkerCommand) -> Result<(), AmmColdStartWorkerError> {
        let count = command.job_count();
        debug_assert!(
            count > 0,
            "empty cold-start batches return before admission"
        );
        if self
            .accepted_jobs
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |accepted| {
                accepted
                    .checked_add(count)
                    .filter(|next| *next <= self.queue_capacity)
            })
            .is_err()
        {
            return Err(AmmColdStartWorkerError::Full);
        }
        if *self.shutdown.borrow() {
            self.accepted_jobs.fetch_sub(count, Ordering::AcqRel);
            return Err(AmmColdStartWorkerError::Closed);
        }
        match self.commands.send(command) {
            Ok(()) => Ok(()),
            Err(_) => {
                self.accepted_jobs.fetch_sub(count, Ordering::AcqRel);
                Err(AmmColdStartWorkerError::Closed)
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AmmColdStartTarget {
    PendingRegistration,
    ActiveRefresh,
}

pub(crate) struct AmmColdStartJob {
    pub(crate) work: RuntimeWorkId,
    pub(crate) pool: PoolInstanceId,
    pub(crate) registration: PoolRegistration,
    pub(crate) baseline: AmmStatePoint,
    pub(crate) registry: Arc<AdapterRegistrySnapshot>,
    pub(crate) cache: Arc<evm_fork_cache::EvmSnapshot>,
    pub(crate) policy: ColdStartPolicy,
    pub(crate) class: AmmWorkClass,
    pub(crate) target: AmmColdStartTarget,
}

/// One exact-generation deferred slot-verification quantum.
pub(crate) struct AmmSlotPatchJob {
    pub(crate) work: RuntimeWorkId,
    pub(crate) pool: PoolInstanceId,
    pub(crate) baseline: AmmStatePoint,
    slots: Vec<(alloy_primitives::Address, alloy_primitives::U256)>,
    pub(crate) class: AmmWorkClass,
}

impl AmmSlotPatchJob {
    pub(crate) fn new(
        work: RuntimeWorkId,
        pool: PoolInstanceId,
        baseline: AmmStatePoint,
        slots: impl IntoIterator<Item = (alloy_primitives::Address, alloy_primitives::U256)>,
        class: AmmWorkClass,
    ) -> Self {
        let mut slots: Vec<_> = slots.into_iter().collect();
        slots.sort_unstable();
        slots.dedup();
        Self {
            work,
            pool,
            baseline,
            slots,
            class,
        }
    }
}

/// One exact-generation, exact-baseline connector discovery quantum.
pub(crate) struct AmmDiscoveryJob {
    pub(crate) work: RuntimeWorkId,
    pub(crate) owner: DiscoveryOwnerId,
    pub(crate) request: TokenEdgeDiscoveryRequest,
    pub(crate) prepared: PreparedDiscoveryReads,
    pub(crate) discovery: Arc<PoolDiscovery>,
    pub(crate) baseline: AmmStatePoint,
    pub(crate) class: AmmWorkClass,
}

enum WorkerCommand {
    ColdStartBatch(Vec<AmmColdStartJob>),
    Discovery(Box<AmmDiscoveryJob>),
    SlotPatch(Box<AmmSlotPatchJob>),
    Cancel(RuntimeWorkId),
}

impl WorkerCommand {
    fn job_count(&self) -> usize {
        match self {
            Self::ColdStartBatch(jobs) => jobs.len(),
            Self::Discovery(_) | Self::SlotPatch(_) => 1,
            Self::Cancel(_) => 0,
        }
    }
}

enum ColdStartExecution {
    New(Box<AmmColdStartJob>),
    Discovery(Box<AmmDiscoveryJob>),
    SlotPatch(Box<AmmSlotPatchJob>),
    Continuing {
        work: RuntimeWorkId,
        pool: PoolInstanceId,
        class: AmmWorkClass,
        target: AmmColdStartTarget,
        next_round: u64,
        prepared: Box<PreparedPoolJob>,
    },
}

impl ColdStartExecution {
    fn work(&self) -> &RuntimeWorkId {
        match self {
            Self::New(job) => &job.work,
            Self::Discovery(job) => &job.work,
            Self::SlotPatch(job) => &job.work,
            Self::Continuing { work, .. } => work,
        }
    }

    fn class(&self) -> AmmWorkClass {
        match self {
            Self::New(job) => job.class,
            Self::Discovery(job) => job.class,
            Self::SlotPatch(job) => job.class,
            Self::Continuing { class, .. } => *class,
        }
    }
}

struct AcceptedExecution {
    execution: ColdStartExecution,
    admission: AdmissionLease,
}

impl AcceptedExecution {
    fn new(execution: ColdStartExecution, admission: AdmissionLease) -> Self {
        Self {
            execution,
            admission,
        }
    }

    fn work(&self) -> &RuntimeWorkId {
        self.execution.work()
    }

    fn class(&self) -> AmmWorkClass {
        self.execution.class()
    }
}

struct PriorityQueue<T> {
    by_class: BTreeMap<AmmWorkClass, VecDeque<T>>,
    last_class: Option<AmmWorkClass>,
    consecutive: usize,
    aging_cursor: Option<AmmWorkClass>,
}

impl<T> PriorityQueue<T> {
    fn new() -> Self {
        Self {
            by_class: BTreeMap::new(),
            last_class: None,
            consecutive: 0,
            aging_cursor: None,
        }
    }

    fn push(&mut self, class: AmmWorkClass, job: T) {
        self.by_class.entry(class).or_default().push_back(job);
    }

    fn pop(&mut self) -> Option<T> {
        let highest = self
            .by_class
            .iter()
            .find_map(|(class, jobs)| (!jobs.is_empty()).then_some(*class))?;
        let selected =
            if self.last_class == Some(highest) && self.consecutive >= PRIORITY_BURST_QUANTA {
                let lower_bound = self
                    .aging_cursor
                    .filter(|cursor| *cursor > highest)
                    .unwrap_or(highest);
                let aged = self
                    .by_class
                    .range((
                        std::ops::Bound::Excluded(lower_bound),
                        std::ops::Bound::Unbounded,
                    ))
                    .find_map(|(class, jobs)| (!jobs.is_empty()).then_some(*class))
                    .or_else(|| {
                        self.by_class
                            .range((
                                std::ops::Bound::Excluded(highest),
                                std::ops::Bound::Unbounded,
                            ))
                            .find_map(|(class, jobs)| (!jobs.is_empty()).then_some(*class))
                    });
                if let Some(aged) = aged {
                    self.aging_cursor = Some(aged);
                    aged
                } else {
                    highest
                }
            } else {
                highest
            };
        if self.last_class == Some(selected) {
            self.consecutive += 1;
        } else {
            self.last_class = Some(selected);
            self.consecutive = 1;
        }
        self.by_class.get_mut(&selected)?.pop_front()
    }

    fn drain(&mut self) -> impl Iterator<Item = T> + '_ {
        self.by_class.values_mut().flat_map(|jobs| jobs.drain(..))
    }

    fn remove_where(&mut self, mut predicate: impl FnMut(&T) -> bool) -> Option<T> {
        for jobs in self.by_class.values_mut() {
            if let Some(index) = jobs.iter().position(&mut predicate) {
                return jobs.remove(index);
            }
        }
        None
    }
}

enum QuantumResult {
    Continue(Box<AcceptedExecution>),
    Finished,
}

fn enqueue_command(
    queue: &mut PriorityQueue<AcceptedExecution>,
    command: WorkerCommand,
    accepted_jobs: &Arc<AtomicUsize>,
) {
    match command {
        WorkerCommand::ColdStartBatch(jobs) => {
            for job in jobs {
                queue.push(
                    job.class,
                    AcceptedExecution::new(
                        ColdStartExecution::New(Box::new(job)),
                        AdmissionLease::new(Arc::clone(accepted_jobs)),
                    ),
                );
            }
        }
        WorkerCommand::Discovery(job) => {
            queue.push(
                job.class,
                AcceptedExecution::new(
                    ColdStartExecution::Discovery(job),
                    AdmissionLease::new(Arc::clone(accepted_jobs)),
                ),
            );
        }
        WorkerCommand::SlotPatch(job) => {
            queue.push(
                job.class,
                AcceptedExecution::new(
                    ColdStartExecution::SlotPatch(job),
                    AdmissionLease::new(Arc::clone(accepted_jobs)),
                ),
            );
        }
        WorkerCommand::Cancel(_) => {
            debug_assert!(false, "cancel commands are handled before queue admission");
        }
    }
}

fn remove_queued_work(
    queue: &mut PriorityQueue<AcceptedExecution>,
    work: &RuntimeWorkId,
) -> Option<AcceptedExecution> {
    queue.remove_where(|job| job.work() == work)
}

fn handle_worker_command(
    command: WorkerCommand,
    queued: &mut PriorityQueue<AcceptedExecution>,
    abort_by_work: &mut HashMap<RuntimeWorkId, (AbortHandle, AdmissionLease)>,
    externally_cancelled: &mut HashSet<Id>,
    accepted_jobs: &Arc<AtomicUsize>,
) {
    match command {
        WorkerCommand::Cancel(work) => {
            if let Some(queued) = remove_queued_work(queued, &work) {
                queued.admission.release();
            } else if let Some((abort, admission)) = abort_by_work.remove(&work) {
                externally_cancelled.insert(abort.id());
                admission.release();
                abort.abort();
            }
        }
        command => enqueue_command(queued, command, accepted_jobs),
    }
}

async fn cancel_command(runtime: &AmmRuntimeHandle, command: WorkerCommand) -> usize {
    let count = command.job_count();
    match command {
        WorkerCommand::ColdStartBatch(jobs) => {
            for job in jobs {
                cancel_job(runtime, job.work).await;
            }
        }
        WorkerCommand::Discovery(job) => cancel_job(runtime, job.work).await,
        WorkerCommand::SlotPatch(job) => cancel_job(runtime, job.work).await,
        WorkerCommand::Cancel(_) => {}
    }
    count
}

async fn cancel_job(runtime: &AmmRuntimeHandle, work: RuntimeWorkId) {
    if runtime.shutdown_requested() {
        // The actor no longer accepts control work after observing shutdown.
        // Its teardown owns dropping pending runtime state, so awaiting a
        // cancellation command here would deadlock worker draining.
        return;
    }
    // Cancellation is generation-owned. A stale result means that the runtime
    // already resolved this exact attempt, so there is nothing left to cancel.
    let _ = runtime.cancel_scheduled_work(work).await;
}

async fn fail_job(runtime: &AmmRuntimeHandle, work: RuntimeWorkId, error: impl fmt::Display) {
    let _ = runtime.fail_scheduled_work(work, error.to_string()).await;
}

async fn commit_prepared(
    runtime: &AmmRuntimeHandle,
    work: RuntimeWorkId,
    pool: PoolInstanceId,
    target: AmmColdStartTarget,
    prepared: super::AmmPreparedPoolState,
    admission: &AdmissionLease,
) {
    let prepared = prepared.with_schedule(work.clone(), pool);
    // Terminal actor commits may synchronously enqueue successor work (for
    // example, deferred reads produced by cold start). Make this completed
    // quantum's shared slot available before crossing that handoff boundary.
    admission.release();
    let committed = match target {
        AmmColdStartTarget::PendingRegistration => runtime.commit_scheduled_pool(prepared).await,
        AmmColdStartTarget::ActiveRefresh => runtime.commit_scheduled_refresh(prepared).await,
    };
    if let Err(error) = committed {
        fail_job(runtime, work, error).await;
    }
}

async fn run_discovery_quantum<P>(
    runtime: &AmmRuntimeHandle,
    provider: P,
    job: AmmDiscoveryJob,
    config: AmmColdStartWorkerConfig,
    admission: &AdmissionLease,
    shutdown: &mut watch::Receiver<bool>,
) where
    P: Provider<AnyNetwork> + Clone + Send + Sync + 'static,
{
    let started = tokio::select! {
        biased;
        _ = shutdown.changed() => {
            cancel_job(runtime, job.work).await;
            return;
        }
        result = runtime.mark_scheduled_work_started(job.work.clone()) => result,
    };
    if started.is_err() {
        cancel_job(runtime, job.work).await;
        return;
    }

    let provider = Arc::new(provider);
    let fetcher = StorageRoundFetcher::from_provider(
        provider,
        config.storage_batch_config,
        config.storage_fetch_strategy,
    );
    let request = StorageRoundRequest::new(
        job.baseline.block_hash(),
        job.prepared.reads().iter().copied(),
        std::iter::empty::<(alloy_primitives::Address, alloy_primitives::U256)>(),
    );
    let fetched = fetcher.fetch(&request);
    if *shutdown.borrow() {
        cancel_job(runtime, job.work).await;
        return;
    }
    let patch = match fetched {
        Ok(fetched) => fetched.into_patch(),
        Err(error) => {
            fail_job(runtime, job.work, error).await;
            return;
        }
    };
    let values = patch
        .values()
        .iter()
        .map(|value| ((value.address(), value.slot()), value.value()));
    let discovered = match job.discovery.assemble_prepared(&job.prepared, values) {
        Ok(discovered) => discovered,
        Err(error) => {
            // Missing or malformed results fail this exact work attempt. The
            // actor owns the owner-generation tombstone and late-result fence.
            fail_job(runtime, job.work, error).await;
            return;
        }
    };
    if let Err(error) = runtime
        .report_scheduled_round(job.work.clone(), 0, None)
        .await
    {
        fail_job(runtime, job.work, error).await;
        return;
    }
    let report = TokenEdgeDiscoveryReport::new(job.request, discovered);
    let work = job.work;
    // Discovery commit may enqueue newly discovered pool cold starts. Release
    // before handoff so a capacity-one worker can accept the successor.
    admission.release();
    if let Err(error) = runtime
        .commit_scheduled_discovery(work.clone(), job.owner, report)
        .await
    {
        fail_job(runtime, work, error).await;
    }
}

async fn run_slot_patch_quantum<P>(
    runtime: &AmmRuntimeHandle,
    provider: P,
    job: AmmSlotPatchJob,
    config: AmmColdStartWorkerConfig,
    admission: &AdmissionLease,
    shutdown: &mut watch::Receiver<bool>,
) where
    P: Provider<AnyNetwork> + Clone + Send + Sync + 'static,
{
    let started = tokio::select! {
        biased;
        _ = shutdown.changed() => {
            cancel_job(runtime, job.work).await;
            return;
        }
        result = runtime.mark_scheduled_work_started(job.work.clone()) => result,
    };
    if started.is_err() {
        cancel_job(runtime, job.work).await;
        return;
    }

    let provider = Arc::new(provider);
    let fetcher = StorageRoundFetcher::from_provider(
        provider,
        config.storage_batch_config,
        config.storage_fetch_strategy,
    );
    let request = StorageRoundRequest::new(
        job.baseline.block_hash(),
        job.slots.iter().copied(),
        std::iter::empty::<(alloy_primitives::Address, alloy_primitives::U256)>(),
    );
    let fetched = fetcher.fetch(&request);
    if *shutdown.borrow() {
        cancel_job(runtime, job.work).await;
        return;
    }
    let patch = match fetched {
        Ok(fetched)
            if fetched.patch().values().len() == job.slots.len()
                && fetched.patch().values().iter().all(|value| {
                    job.slots
                        .binary_search(&(value.address(), value.slot()))
                        .is_ok()
                }) =>
        {
            fetched.into_patch()
        }
        Ok(_) => {
            fail_job(
                runtime,
                job.work,
                "deferred slot verification did not return every requested value",
            )
            .await;
            return;
        }
        Err(error) => {
            fail_job(runtime, job.work, error).await;
            return;
        }
    };
    let storage = patch
        .values()
        .iter()
        .map(|value| AmmPreparedStorage::new(value.address(), value.slot(), value.value()))
        .collect();
    if let Err(error) = runtime
        .report_scheduled_round(job.work.clone(), 0, None)
        .await
    {
        fail_job(runtime, job.work, error).await;
        return;
    }
    let work = job.work;
    // A terminal repair/deferred commit can hand off more exact-owner work.
    admission.release();
    if let Err(error) = runtime
        .commit_scheduled_slot_patch(work.clone(), job.pool, job.baseline, storage)
        .await
    {
        fail_job(runtime, work, error).await;
    }
}

async fn run_cold_start_quantum<P>(
    runtime: AmmRuntimeHandle,
    provider: P,
    accepted: AcceptedExecution,
    config: AmmColdStartWorkerConfig,
    mut shutdown: watch::Receiver<bool>,
) -> QuantumResult
where
    P: Provider<AnyNetwork> + Clone + Send + Sync + 'static,
{
    let AcceptedExecution {
        execution,
        admission,
    } = accepted;
    if *shutdown.borrow() {
        cancel_job(&runtime, execution.work().clone()).await;
        return QuantumResult::Finished;
    }

    match execution {
        ColdStartExecution::New(job) => {
            let started = tokio::select! {
                biased;
                _ = shutdown.changed() => {
                    cancel_job(&runtime, job.work).await;
                    return QuantumResult::Finished;
                }
                result = runtime.mark_scheduled_work_started(job.work.clone()) => result,
            };
            if started.is_err() {
                // In particular, do not fetch or publish after external cancellation
                // made this exact work generation stale.
                cancel_job(&runtime, job.work).await;
                return QuantumResult::Finished;
            }

            let provider = Arc::new(provider);
            let account_fetcher = AccountProofRoundFetcher::from_provider(Arc::clone(&provider), 8);
            let fast = tokio::select! {
                biased;
                _ = shutdown.changed() => {
                    cancel_job(&runtime, job.work).await;
                    return QuantumResult::Finished;
                }
                result = prepare_fast_pool(
                    job.registry.registry(),
                    job.registration.clone(),
                    job.baseline,
                    provider.as_ref(),
                    Some(&account_fetcher),
                ) => result,
            };
            if *shutdown.borrow() {
                cancel_job(&runtime, job.work).await;
                return QuantumResult::Finished;
            }
            match fast {
                Ok(Some(prepared)) => {
                    if let Err(error) = runtime
                        .report_scheduled_round(job.work.clone(), 0, None)
                        .await
                    {
                        fail_job(&runtime, job.work, error).await;
                    } else {
                        commit_prepared(
                            &runtime, job.work, job.pool, job.target, prepared, &admission,
                        )
                        .await;
                    }
                    QuantumResult::Finished
                }
                Ok(None) => {
                    let adapter = match job.registry.registry().adapter(job.registration.protocol())
                    {
                        Some(adapter) => adapter.clone(),
                        None => {
                            fail_job(
                                &runtime,
                                job.work,
                                "prepared fallback has no registered adapter",
                            )
                            .await;
                            return QuantumResult::Finished;
                        }
                    };
                    let verified_accounts = match prepare_verified_code_targets(
                        adapter.as_ref(),
                        &job.registration,
                        job.baseline,
                        provider.as_ref(),
                    )
                    .await
                    {
                        Ok(accounts) => accounts,
                        Err(error) => {
                            fail_job(&runtime, job.work, error).await;
                            return QuantumResult::Finished;
                        }
                    };
                    let storage_fetcher = StorageRoundFetcher::from_provider(
                        Arc::clone(&provider),
                        config.storage_batch_config,
                        config.storage_fetch_strategy,
                    );
                    let base = Arc::new(PreparedSnapshotState::new(job.cache));
                    match PreparedPoolJob::new(
                        job.registry.registry(),
                        job.registration,
                        job.baseline,
                        job.policy,
                        base,
                        PreparedPoolFetchers::new(storage_fetcher, Some(account_fetcher))
                            .with_verified_accounts(verified_accounts),
                        ResumableColdStartConfig::default(),
                    ) {
                        Ok(mut prepared) => match prepared.step() {
                            Ok(PreparedPoolStep::Continue { .. }) => {
                                if let Err(error) = runtime
                                    .report_scheduled_round(job.work.clone(), 0, Some(1))
                                    .await
                                {
                                    fail_job(&runtime, job.work, error).await;
                                    QuantumResult::Finished
                                } else {
                                    QuantumResult::Continue(Box::new(AcceptedExecution::new(
                                        ColdStartExecution::Continuing {
                                            work: job.work,
                                            pool: job.pool,
                                            class: job.class,
                                            target: job.target,
                                            next_round: 1,
                                            prepared: Box::new(prepared),
                                        },
                                        admission,
                                    )))
                                }
                            }
                            Ok(PreparedPoolStep::Done(prepared)) => {
                                if let Err(error) = runtime
                                    .report_scheduled_round(job.work.clone(), 0, None)
                                    .await
                                {
                                    fail_job(&runtime, job.work, error).await;
                                } else {
                                    commit_prepared(
                                        &runtime, job.work, job.pool, job.target, *prepared,
                                        &admission,
                                    )
                                    .await;
                                }
                                QuantumResult::Finished
                            }
                            Err(error) => {
                                fail_job(&runtime, job.work, error).await;
                                QuantumResult::Finished
                            }
                        },
                        Err(error) => {
                            fail_job(&runtime, job.work, error).await;
                            QuantumResult::Finished
                        }
                    }
                }
                Err(error) => {
                    fail_job(&runtime, job.work, error).await;
                    QuantumResult::Finished
                }
            }
        }
        ColdStartExecution::Discovery(job) => {
            run_discovery_quantum(&runtime, provider, *job, config, &admission, &mut shutdown)
                .await;
            QuantumResult::Finished
        }
        ColdStartExecution::SlotPatch(job) => {
            run_slot_patch_quantum(&runtime, provider, *job, config, &admission, &mut shutdown)
                .await;
            QuantumResult::Finished
        }
        ColdStartExecution::Continuing {
            work,
            pool,
            class,
            target,
            next_round,
            mut prepared,
        } => {
            let begun = tokio::select! {
                biased;
                _ = shutdown.changed() => {
                    cancel_job(&runtime, work).await;
                    return QuantumResult::Finished;
                }
                result = runtime.begin_scheduled_round(work.clone(), next_round) => result,
            };
            if begun.is_err() {
                cancel_job(&runtime, work).await;
                return QuantumResult::Finished;
            }
            // `PreparedPoolJob::step` performs exactly one provider round. It
            // owns no canonical cache and retains only a compact replay
            // transcript, so yielding the returned continuation is safe.
            let step = prepared.step();
            if *shutdown.borrow() {
                cancel_job(&runtime, work).await;
                return QuantumResult::Finished;
            }
            match step {
                Ok(PreparedPoolStep::Continue { .. }) => {
                    let Some(following_round) = next_round.checked_add(1) else {
                        fail_job(&runtime, work, "cold-start round counter overflow").await;
                        return QuantumResult::Finished;
                    };
                    if let Err(error) = runtime
                        .report_scheduled_round(work.clone(), next_round, Some(following_round))
                        .await
                    {
                        fail_job(&runtime, work, error).await;
                        QuantumResult::Finished
                    } else {
                        QuantumResult::Continue(Box::new(AcceptedExecution::new(
                            ColdStartExecution::Continuing {
                                work,
                                pool,
                                class,
                                target,
                                next_round: following_round,
                                prepared,
                            },
                            admission,
                        )))
                    }
                }
                Ok(PreparedPoolStep::Done(prepared)) => {
                    if let Err(error) = runtime
                        .report_scheduled_round(work.clone(), next_round, None)
                        .await
                    {
                        fail_job(&runtime, work, error).await;
                    } else {
                        commit_prepared(&runtime, work, pool, target, *prepared, &admission).await;
                    }
                    QuantumResult::Finished
                }
                Err(error) => {
                    fail_job(&runtime, work, error).await;
                    QuantumResult::Finished
                }
            }
        }
    }
}

pub(crate) fn spawn_cold_start_worker<P>(
    runtime: AmmRuntimeHandle,
    provider: P,
    config: AmmColdStartWorkerConfig,
) -> Result<(AmmColdStartWorkerControl, AmmColdStartWorkerHandle), AmmColdStartWorkerError>
where
    P: Provider<AnyNetwork> + Clone + Send + Sync + 'static,
{
    if config.queue_capacity == 0 || config.max_concurrency == 0 {
        return Err(AmmColdStartWorkerError::ZeroCapacity);
    }
    let (commands, mut receiver) = mpsc::unbounded_channel();
    let (state_tx, state_rx) = watch::channel(AmmColdStartWorkerState::Running);
    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
    let task_shutdown = shutdown_tx.clone();
    let accepted_jobs = Arc::new(AtomicUsize::new(0));
    let worker_jobs = Arc::clone(&accepted_jobs);
    tokio::spawn(async move {
        let mut children = JoinSet::new();
        let mut in_flight = HashMap::new();
        let mut abort_by_work = HashMap::new();
        let mut externally_cancelled = HashSet::new();
        let mut queued = PriorityQueue::new();
        loop {
            if *shutdown_rx.borrow() {
                break;
            }
            // Fold every already-accepted batch into the priority queues before
            // choosing another quantum. Otherwise a just-completed continuation
            // could be redispatched while a higher-priority batch was already
            // buffered in the command channel.
            while let Ok(command) = receiver.try_recv() {
                handle_worker_command(
                    command,
                    &mut queued,
                    &mut abort_by_work,
                    &mut externally_cancelled,
                    &worker_jobs,
                );
            }
            while children.len() < config.max_concurrency {
                let Some(job) = queued.pop() else {
                    break;
                };
                let work = job.work().clone();
                let admission = job.admission.clone();
                let task = children.spawn(run_cold_start_quantum(
                    runtime.clone(),
                    provider.clone(),
                    job,
                    config,
                    task_shutdown.subscribe(),
                ));
                in_flight.insert(task.id(), (work.clone(), admission.clone()));
                abort_by_work.insert(work, (task, admission));
            }
            tokio::select! {
                biased;
                _ = shutdown_rx.changed() => break,
                result = children.join_next_with_id(), if !children.is_empty() => {
                    match result {
                        Some(Ok((id, QuantumResult::Continue(job)))) => {
                            if let Some((work, _)) = in_flight.remove(&id) {
                                abort_by_work.remove(&work);
                            }
                            if !externally_cancelled.remove(&id) {
                                let job = *job;
                                queued.push(job.class(), job);
                            }
                        }
                        Some(Ok((id, QuantumResult::Finished))) => {
                            if let Some((work, admission)) = in_flight.remove(&id) {
                                abort_by_work.remove(&work);
                                admission.release();
                            }
                            externally_cancelled.remove(&id);
                        }
                        Some(Err(error)) => {
                            if let Some((work, admission)) = in_flight.remove(&error.id()) {
                                abort_by_work.remove(&work);
                                if !externally_cancelled.remove(&error.id()) {
                                    cancel_job(&runtime, work).await;
                                }
                                admission.release();
                            }
                        }
                        None => {}
                    }
                }
                command = receiver.recv() => match command {
                    Some(command) => handle_worker_command(
                        command,
                        &mut queued,
                        &mut abort_by_work,
                        &mut externally_cancelled,
                        &worker_jobs,
                    ),
                    None => break,
                }
            }
        }

        receiver.close();
        task_shutdown.send_replace(true);
        while let Ok(command) = receiver.try_recv() {
            let cancelled = cancel_command(&runtime, command).await;
            worker_jobs.fetch_sub(cancelled, Ordering::AcqRel);
        }
        for job in queued.drain() {
            cancel_job(&runtime, job.work().clone()).await;
            job.admission.release();
        }
        while let Some(result) = children.join_next_with_id().await {
            match result {
                Ok((id, QuantumResult::Continue(job))) => {
                    if let Some((work, _)) = in_flight.remove(&id) {
                        abort_by_work.remove(&work);
                    }
                    if !externally_cancelled.remove(&id) {
                        cancel_job(&runtime, job.work().clone()).await;
                        job.admission.release();
                    }
                }
                Ok((id, QuantumResult::Finished)) => {
                    if let Some((work, admission)) = in_flight.remove(&id) {
                        abort_by_work.remove(&work);
                        admission.release();
                    }
                    externally_cancelled.remove(&id);
                }
                Err(error) => {
                    if let Some((work, admission)) = in_flight.remove(&error.id()) {
                        abort_by_work.remove(&work);
                        if !externally_cancelled.remove(&error.id()) {
                            cancel_job(&runtime, work).await;
                        }
                        admission.release();
                    }
                }
            }
        }
        state_tx.send_replace(AmmColdStartWorkerState::Stopped);
    });
    Ok((
        AmmColdStartWorkerControl {
            commands,
            shutdown: shutdown_tx.clone(),
            accepted_jobs,
            queue_capacity: config.queue_capacity,
        },
        AmmColdStartWorkerHandle {
            shutdown: shutdown_tx,
            state: state_rx,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_config_carries_provider_fetch_limits() {
        let strategy =
            evm_fork_cache::StorageFetchStrategy::BulkCall(evm_fork_cache::BulkCallConfig {
                max_slots_per_call: 25_000,
                max_slots_per_request: 25_000,
                ..evm_fork_cache::BulkCallConfig::default()
            });
        let batch = evm_fork_cache::StorageBatchConfig::new(200, 12);
        let config = AmmColdStartWorkerConfig::default()
            .with_storage_batch_config(batch)
            .with_storage_fetch_strategy(strategy);

        assert_eq!(config.storage_batch_config(), batch);
        assert_eq!(config.storage_fetch_strategy(), strategy);
    }

    fn discovery_job(id: u64) -> AmmDiscoveryJob {
        let owner = super::super::DiscoveryOwnerId::new(
            super::super::DiscoveryOwnerKey::new("test.connector-discovery"),
            super::super::DiscoveryGeneration::new(0),
        );
        let work = RuntimeWorkId::new(
            super::super::RuntimeOwnerId::Discovery(owner.clone()),
            super::super::WorkId::new(id),
        );
        let request = TokenEdgeDiscoveryRequest::new(
            alloy_primitives::Address::repeat_byte(0x11),
            [alloy_primitives::Address::repeat_byte(0x22)],
        );
        let discovery = Arc::new(PoolDiscovery::new(std::iter::empty()));
        let prepared = discovery
            .prepare_reads([request.query()])
            .expect("unscoped empty discovery registry is valid");
        AmmDiscoveryJob {
            work,
            owner,
            request,
            prepared,
            discovery,
            baseline: AmmStatePoint::post_block(1, 1, alloy_primitives::B256::repeat_byte(0x01)),
            class: AmmWorkClass::Focused,
        }
    }

    fn slot_patch_job(id: u64) -> AmmSlotPatchJob {
        let address = alloy_primitives::Address::repeat_byte(0x31);
        let pool = super::super::PoolInstanceId::new(
            super::super::PoolKey::UniswapV2(address),
            super::super::PoolGeneration::new(0),
        );
        let work = RuntimeWorkId::new(
            super::super::RuntimeOwnerId::Pool(pool.clone()),
            super::super::WorkId::new(id),
        );
        AmmSlotPatchJob::new(
            work,
            pool,
            AmmStatePoint::post_block(1, 1, alloy_primitives::B256::repeat_byte(0x01)),
            [(address, alloy_primitives::U256::from(1))],
            AmmWorkClass::Deferred,
        )
    }

    #[test]
    fn discovery_submit_atomically_reserves_one_shared_capacity_slot() {
        let (commands, mut receiver) = mpsc::unbounded_channel();
        let (shutdown, _) = watch::channel(false);
        let accepted_jobs = Arc::new(AtomicUsize::new(0));
        let control = AmmColdStartWorkerControl {
            commands,
            shutdown,
            accepted_jobs: Arc::clone(&accepted_jobs),
            queue_capacity: 1,
        };

        control
            .submit_discovery(discovery_job(0))
            .expect("first job fits the shared bound");
        assert!(matches!(
            control.submit_discovery(discovery_job(1)),
            Err(AmmColdStartWorkerError::Full)
        ));
        assert_eq!(accepted_jobs.load(Ordering::Acquire), 1);
        let command = receiver.try_recv().expect("accepted job was enqueued");
        assert!(matches!(command, WorkerCommand::Discovery(_)));
    }

    #[test]
    fn discovery_and_slot_patch_share_one_atomic_capacity_bound() {
        let (commands, _receiver) = mpsc::unbounded_channel();
        let (shutdown, _) = watch::channel(false);
        let accepted_jobs = Arc::new(AtomicUsize::new(0));
        let control = AmmColdStartWorkerControl {
            commands,
            shutdown,
            accepted_jobs: Arc::clone(&accepted_jobs),
            queue_capacity: 1,
        };

        control
            .submit_discovery(discovery_job(0))
            .expect("discovery occupies the shared slot");
        assert!(matches!(
            control.submit_slot_patch(slot_patch_job(1)),
            Err(AmmColdStartWorkerError::Full)
        ));
        assert_eq!(accepted_jobs.load(Ordering::Acquire), 1);
    }

    #[test]
    fn cancellation_control_bypasses_full_job_capacity() {
        let (commands, mut receiver) = mpsc::unbounded_channel();
        let (shutdown, _) = watch::channel(false);
        let accepted_jobs = Arc::new(AtomicUsize::new(0));
        let control = AmmColdStartWorkerControl {
            commands,
            shutdown,
            accepted_jobs,
            queue_capacity: 1,
        };
        let job = discovery_job(0);
        let work = job.work.clone();

        control.submit_discovery(job).expect("job fills capacity");
        control
            .cancel(work.clone())
            .expect("cancellation is not capacity bounded");

        assert!(matches!(
            receiver.try_recv(),
            Ok(WorkerCommand::Discovery(_))
        ));
        assert!(matches!(
            receiver.try_recv(),
            Ok(WorkerCommand::Cancel(cancelled)) if cancelled == work
        ));
    }

    #[test]
    fn discovery_commands_preserve_fifo_inside_the_existing_priority_class() {
        let first = discovery_job(0).work.clone();
        let second = discovery_job(1).work.clone();
        let accepted = Arc::new(AtomicUsize::new(2));
        let mut queue = PriorityQueue::new();
        enqueue_command(
            &mut queue,
            WorkerCommand::Discovery(Box::new(discovery_job(0))),
            &accepted,
        );
        enqueue_command(
            &mut queue,
            WorkerCommand::Discovery(Box::new(discovery_job(1))),
            &accepted,
        );

        assert_eq!(queue.pop().map(|job| job.work().clone()), Some(first));
        assert_eq!(queue.pop().map(|job| job.work().clone()), Some(second));
    }

    #[test]
    fn queued_cancellation_releases_shared_capacity_exactly_once() {
        let job = discovery_job(9);
        let work = job.work.clone();
        let accepted = Arc::new(AtomicUsize::new(1));
        let mut queue = PriorityQueue::new();
        enqueue_command(
            &mut queue,
            WorkerCommand::Discovery(Box::new(job)),
            &accepted,
        );
        let mut abort_by_work = HashMap::new();
        let mut externally_cancelled = HashSet::new();

        handle_worker_command(
            WorkerCommand::Cancel(work.clone()),
            &mut queue,
            &mut abort_by_work,
            &mut externally_cancelled,
            &accepted,
        );
        handle_worker_command(
            WorkerCommand::Cancel(work),
            &mut queue,
            &mut abort_by_work,
            &mut externally_cancelled,
            &accepted,
        );

        assert_eq!(accepted.load(Ordering::Acquire), 0);
        assert!(queue.pop().is_none());
    }

    #[tokio::test]
    async fn in_flight_cancellation_aborts_and_releases_capacity_exactly_once() {
        let work = discovery_job(10).work;
        let accepted = Arc::new(AtomicUsize::new(1));
        let mut queue = PriorityQueue::new();
        let task = tokio::spawn(std::future::pending::<()>());
        let task_id = task.id();
        let admission = AdmissionLease::new(Arc::clone(&accepted));
        let mut abort_by_work = HashMap::from([(work.clone(), (task.abort_handle(), admission))]);
        let mut externally_cancelled = HashSet::new();

        handle_worker_command(
            WorkerCommand::Cancel(work.clone()),
            &mut queue,
            &mut abort_by_work,
            &mut externally_cancelled,
            &accepted,
        );
        handle_worker_command(
            WorkerCommand::Cancel(work),
            &mut queue,
            &mut abort_by_work,
            &mut externally_cancelled,
            &accepted,
        );

        assert_eq!(accepted.load(Ordering::Acquire), 0);
        assert!(externally_cancelled.contains(&task_id));
        assert!(task.await.expect_err("task was aborted").is_cancelled());
    }

    #[test]
    fn terminal_handoff_releases_capacity_once_before_successor_admission() {
        let (commands, mut receiver) = mpsc::unbounded_channel();
        let (shutdown, _) = watch::channel(false);
        let accepted = Arc::new(AtomicUsize::new(1));
        let control = AmmColdStartWorkerControl {
            commands,
            shutdown,
            accepted_jobs: Arc::clone(&accepted),
            queue_capacity: 1,
        };
        let current = AdmissionLease::new(Arc::clone(&accepted));

        assert!(current.release());
        control
            .submit_discovery(discovery_job(11))
            .expect("capacity-one terminal handoff accepts its successor");
        assert!(!current.release());
        assert_eq!(accepted.load(Ordering::Acquire), 1);
        assert!(matches!(
            receiver.try_recv(),
            Ok(WorkerCommand::Discovery(_))
        ));
    }

    #[test]
    fn terminal_handoff_can_admit_deferred_work_at_capacity_one() {
        let (commands, mut receiver) = mpsc::unbounded_channel();
        let (shutdown, _) = watch::channel(false);
        let accepted = Arc::new(AtomicUsize::new(1));
        let control = AmmColdStartWorkerControl {
            commands,
            shutdown,
            accepted_jobs: Arc::clone(&accepted),
            queue_capacity: 1,
        };
        let current = AdmissionLease::new(Arc::clone(&accepted));

        assert!(current.release());
        control
            .submit_slot_patch(slot_patch_job(14))
            .expect("capacity-one handoff accepts deferred verification");

        assert_eq!(accepted.load(Ordering::Acquire), 1);
        assert!(matches!(
            receiver.try_recv(),
            Ok(WorkerCommand::SlotPatch(_))
        ));
    }

    #[tokio::test]
    async fn cancellation_after_handoff_does_not_release_successor_capacity() {
        let (commands, _receiver) = mpsc::unbounded_channel();
        let (shutdown, _) = watch::channel(false);
        let accepted = Arc::new(AtomicUsize::new(1));
        let control = AmmColdStartWorkerControl {
            commands,
            shutdown,
            accepted_jobs: Arc::clone(&accepted),
            queue_capacity: 1,
        };
        let work = discovery_job(12).work;
        let current = AdmissionLease::new(Arc::clone(&accepted));
        let task = tokio::spawn(std::future::pending::<()>());
        let task_id = task.id();
        let mut abort_by_work =
            HashMap::from([(work.clone(), (task.abort_handle(), current.clone()))]);
        let mut queue = PriorityQueue::new();
        let mut externally_cancelled = HashSet::new();

        assert!(current.release());
        control
            .submit_discovery(discovery_job(13))
            .expect("successor consumes the released slot");
        handle_worker_command(
            WorkerCommand::Cancel(work),
            &mut queue,
            &mut abort_by_work,
            &mut externally_cancelled,
            &accepted,
        );

        assert_eq!(accepted.load(Ordering::Acquire), 1);
        assert!(externally_cancelled.contains(&task_id));
        assert!(task.await.expect_err("task was aborted").is_cancelled());
    }

    #[test]
    fn slot_patch_job_normalizes_duplicate_targets() {
        let address = alloy_primitives::Address::repeat_byte(0x31);
        let first = alloy_primitives::U256::from(1);
        let second = alloy_primitives::U256::from(2);
        let owner = super::super::PoolInstanceId::new(
            super::super::PoolKey::UniswapV2(address),
            super::super::PoolGeneration::new(0),
        );
        let work = RuntimeWorkId::new(
            super::super::RuntimeOwnerId::Pool(owner.clone()),
            super::super::WorkId::new(3),
        );

        let job = AmmSlotPatchJob::new(
            work,
            owner,
            AmmStatePoint::post_block(1, 1, alloy_primitives::B256::repeat_byte(0x01)),
            [(address, second), (address, first), (address, second)],
            AmmWorkClass::Deferred,
        );

        assert_eq!(job.slots, vec![(address, first), (address, second)]);
    }

    #[test]
    fn focused_overtakes_queued_bootstrap_fifo() {
        let mut queue = PriorityQueue::new();
        queue.push(AmmWorkClass::Bootstrap, "bootstrap-one");
        queue.push(AmmWorkClass::Bootstrap, "bootstrap-two");
        assert_eq!(queue.pop(), Some("bootstrap-one"));

        queue.push(AmmWorkClass::Focused, "focused");
        assert_eq!(queue.pop(), Some("focused"));
        assert_eq!(queue.pop(), Some("bootstrap-two"));
    }

    #[test]
    fn continuing_lower_priority_quantum_yields_after_requeue() {
        let mut queue = PriorityQueue::new();
        queue.push(AmmWorkClass::Bootstrap, "bootstrap-round-zero");
        assert_eq!(queue.pop(), Some("bootstrap-round-zero"));

        // A continuation is appended only after its one-round quantum. Any
        // already accepted higher-priority work therefore dispatches first.
        queue.push(AmmWorkClass::Bootstrap, "bootstrap-round-one");
        queue.push(AmmWorkClass::Focused, "focused-round-zero");
        assert_eq!(queue.pop(), Some("focused-round-zero"));
        assert_eq!(queue.pop(), Some("bootstrap-round-one"));
    }

    #[test]
    fn priority_budget_eventually_serves_a_waiting_lower_class() {
        let mut queue = PriorityQueue::new();
        for quantum in 0..=PRIORITY_BURST_QUANTA {
            queue.push(AmmWorkClass::Focused, quantum);
        }
        queue.push(AmmWorkClass::Deferred, usize::MAX);

        for quantum in 0..PRIORITY_BURST_QUANTA {
            assert_eq!(queue.pop(), Some(quantum));
        }
        assert_eq!(queue.pop(), Some(usize::MAX));
    }

    #[test]
    fn priority_budget_rotates_across_multiple_waiting_lower_classes() {
        let mut queue = PriorityQueue::new();
        for quantum in 0..=(PRIORITY_BURST_QUANTA * 2) {
            queue.push(AmmWorkClass::Focused, quantum);
        }
        queue.push(AmmWorkClass::Bootstrap, usize::MAX - 1);
        queue.push(AmmWorkClass::Deferred, usize::MAX);

        for quantum in 0..PRIORITY_BURST_QUANTA {
            assert_eq!(queue.pop(), Some(quantum));
        }
        assert_eq!(queue.pop(), Some(usize::MAX - 1));
        for quantum in PRIORITY_BURST_QUANTA..(PRIORITY_BURST_QUANTA * 2) {
            assert_eq!(queue.pop(), Some(quantum));
        }
        assert_eq!(queue.pop(), Some(usize::MAX));
    }
}
