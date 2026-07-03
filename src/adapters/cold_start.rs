//! Adapter cold-start adoption over [`EvmCache::run_cold_start`].
//!
//! This module bridges the crate-owned [`AmmAdapter`](super::AmmAdapter)
//! cold-start contract onto the upstream protocol-neutral
//! [`evm_fork_cache::cold_start`] driver. Each adapter builds an
//! [`AdapterColdStartPlanner`] (via
//! [`AmmAdapter::cold_start_planner`](super::AmmAdapter::cold_start_planner)),
//! [`AdapterRegistry::cold_start`] drives it through the bounded multi-round
//! loop, and the planner finalizes the pool's metadata/status into a
//! [`ColdStartOutcome`].
//!
//! `evm_fork_cache::cold_start` is available unconditionally here: the dependency
//! is declared with its default features (which enable the upstream `reactive`
//! feature that gates `cold_start`), so this module is always compiled rather
//! than behind a protocol flag.
//!
//! # Archive-miss classification
//!
//! The per-slot [`SlotFetch`](evm_fork_cache::cold_start::SlotFetch) replaces the
//! former `cached_storage(..).is_none()` proxy: a planner's `on_results` decides
//! a mandatory slot's verdict from `Value` / `Zero` / `FetchFailed`, so a genuine
//! on-chain zero and a transient archive miss become *distinguishable* repairs.

use alloy_primitives::{Address, Bytes, U256};
use evm_fork_cache::cache::EvmCache;
use evm_fork_cache::cold_start::ColdStartConfig;

use super::state::UpstreamStateView;
use super::{
    AdapterRegistry, CallOutcome, ColdStartOutcome, ColdStartPolicy, PoolRegistration, SlotChange,
    StateView, UnsupportedReason,
};

// ---------------------------------------------------------------------------
// Crate-owned mirrors of the `evm_fork_cache::cold_start` vocabulary.
//
// Each mirror keeps upstream's variant / field NAMES so planner call-sites read
// the same; the `From` conversions bridge to/from upstream at the driver seam.
// ---------------------------------------------------------------------------

/// A single round of cold-start work, declared by an
/// [`AdapterColdStartPlanner`].
///
/// Crate-owned mirror of [`evm_fork_cache::cold_start::ColdStartPlan`]. All four
/// phases are optional; an empty plan is a valid no-op round.
#[derive(Clone, Debug, Default)]
pub struct ColdStartPlan {
    /// Slots to authoritatively re-fetch, classify, and inject when changed.
    pub verify: Vec<(Address, U256)>,
    /// Slots to classify at the pinned block without injecting.
    pub probe: Vec<(Address, U256)>,
    /// Accounts to pre-seed into the cache before discovery.
    pub accounts: Vec<Address>,
    /// View-calls whose touched slots and accounts are captured.
    pub discover: Vec<ColdStartCall>,
}

impl From<ColdStartPlan> for evm_fork_cache::cold_start::ColdStartPlan {
    fn from(plan: ColdStartPlan) -> Self {
        evm_fork_cache::cold_start::ColdStartPlan {
            verify: plan.verify,
            probe: plan.probe,
            accounts: plan.accounts,
            discover: plan.discover.into_iter().map(Into::into).collect(),
            // Root-only account probes (0.2.0, Phase-8 root baseline) are not
            // yet part of the adapter planner vocabulary.
            probe_roots: Vec::new(),
        }
    }
}

/// A read-only view-call whose touched storage and accounts are captured during
/// the discover phase.
///
/// Crate-owned mirror of [`evm_fork_cache::cold_start::ColdStartCall`].
#[derive(Clone, Debug)]
pub struct ColdStartCall {
    /// Transaction sender.
    pub from: Address,
    /// Call target.
    pub to: Address,
    /// Calldata.
    pub calldata: Bytes,
    /// When set, filters captured slots and accounts to these addresses.
    pub restrict_to: Option<Vec<Address>>,
}

impl From<ColdStartCall> for evm_fork_cache::cold_start::ColdStartCall {
    fn from(call: ColdStartCall) -> Self {
        evm_fork_cache::cold_start::ColdStartCall {
            from: call.from,
            to: call.to,
            calldata: call.calldata,
            restrict_to: call.restrict_to,
        }
    }
}

/// The outcome of executing one [`ColdStartPlan`] round.
///
/// Crate-owned mirror of [`evm_fork_cache::cold_start::ColdStartResults`].
/// `fetched` / `probed` carry one [`SlotOutcome`] per declared verify / probe
/// slot; `verified` carries only the slots whose value changed; `discovered`
/// carries one [`ColdStartCallResult`] per discover call.
#[derive(Clone, Debug, Default)]
pub struct ColdStartResults {
    /// Slots whose value changed and were injected (one per change).
    pub verified: Vec<SlotChange>,
    /// One outcome per declared verify slot (`Value` / `Zero` / `FetchFailed`).
    pub fetched: Vec<SlotOutcome>,
    /// One outcome per declared probe slot (classified, not injected).
    pub probed: Vec<SlotOutcome>,
    /// One result per discover call.
    pub discovered: Vec<ColdStartCallResult>,
}

impl From<evm_fork_cache::cold_start::ColdStartResults> for ColdStartResults {
    fn from(results: evm_fork_cache::cold_start::ColdStartResults) -> Self {
        Self {
            verified: results.verified.into_iter().map(SlotChange::from).collect(),
            fetched: results.fetched.into_iter().map(SlotOutcome::from).collect(),
            probed: results.probed.into_iter().map(SlotOutcome::from).collect(),
            discovered: results
                .discovered
                .into_iter()
                .map(ColdStartCallResult::from)
                .collect(),
        }
    }
}

/// The result of one discover view-call: the classified EVM execution outcome and
/// the storage/account access list it touched.
///
/// Crate-owned mirror of [`evm_fork_cache::cold_start::ColdStartCallResult`].
#[derive(Clone, Debug)]
pub struct ColdStartCallResult {
    /// The classified outcome of the view-call.
    pub result: CallOutcome,
    /// The storage slots and accounts the call touched (after `restrict_to`).
    pub access: StorageAccessList,
}

impl From<evm_fork_cache::cold_start::ColdStartCallResult> for ColdStartCallResult {
    fn from(call: evm_fork_cache::cold_start::ColdStartCallResult) -> Self {
        Self {
            result: CallOutcome::from(call.result),
            access: StorageAccessList::from(call.access),
        }
    }
}

/// The storage slots and accounts a discover view-call touched.
///
/// Crate-owned mirror of `evm_fork_cache`'s `StorageAccessList` (the access-set
/// surface a discover call captures).
#[derive(Clone, Debug, Default)]
pub struct StorageAccessList {
    /// Accounts the call touched.
    pub accounts: Vec<Address>,
    /// Storage `(address, slot)` pairs the call touched.
    pub slots: Vec<(Address, U256)>,
}

impl From<evm_fork_cache::access_set::StorageAccessList> for StorageAccessList {
    fn from(access: evm_fork_cache::access_set::StorageAccessList) -> Self {
        Self {
            accounts: access.accounts.into_iter().collect(),
            slots: access.slots.into_iter().collect(),
        }
    }
}

/// The classified result of an individual slot fetch.
///
/// Crate-owned mirror of [`evm_fork_cache::cold_start::SlotFetch`].
/// `#[non_exhaustive]` — an open classification vocabulary.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SlotFetch {
    /// The slot was fetched and holds a non-zero value.
    Value(U256),
    /// The slot was fetched and holds a genuine on-chain zero.
    Zero,
    /// The fetcher returned an error for this slot; `reason` is its description.
    FetchFailed {
        /// Human-readable description of why the fetch failed.
        reason: String,
    },
    /// The slot was declared but never reached because the round short-circuited.
    NotAttempted,
}

impl From<evm_fork_cache::cold_start::SlotFetch> for SlotFetch {
    fn from(fetch: evm_fork_cache::cold_start::SlotFetch) -> Self {
        use evm_fork_cache::cold_start::SlotFetch as Upstream;
        match fetch {
            Upstream::Value(value) => SlotFetch::Value(value),
            Upstream::Zero => SlotFetch::Zero,
            Upstream::FetchFailed { reason } => SlotFetch::FetchFailed { reason },
            Upstream::NotAttempted => SlotFetch::NotAttempted,
        }
    }
}

/// The classified outcome of fetching a single storage slot.
///
/// Crate-owned mirror of `evm_fork_cache`'s `SlotOutcome`: produced for **every**
/// requested verify / probe slot (unlike [`SlotChange`], which records only
/// changed slots).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SlotOutcome {
    /// Contract whose storage slot was fetched.
    pub address: Address,
    /// Storage slot key.
    pub slot: U256,
    /// The classified result of fetching this slot.
    pub fetch: SlotFetch,
}

impl From<evm_fork_cache::cold_start::SlotOutcome> for SlotOutcome {
    fn from(outcome: evm_fork_cache::cold_start::SlotOutcome) -> Self {
        Self {
            address: outcome.address,
            slot: outcome.slot,
            fetch: outcome.fetch.into(),
        }
    }
}

/// The planner's decision after a round completes.
///
/// Crate-owned mirror of [`evm_fork_cache::cold_start::ColdStartStep`].
/// `#[non_exhaustive]` — an open control vocabulary.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum ColdStartStep {
    /// Stop the cold-start loop; the run succeeds.
    Done,
    /// Execute the carried plan as the next round.
    Continue(ColdStartPlan),
}

impl From<ColdStartStep> for evm_fork_cache::cold_start::ColdStartStep {
    fn from(step: ColdStartStep) -> Self {
        match step {
            ColdStartStep::Done => evm_fork_cache::cold_start::ColdStartStep::Done,
            ColdStartStep::Continue(plan) => {
                evm_fork_cache::cold_start::ColdStartStep::Continue(plan.into())
            }
        }
    }
}

/// Summary of a completed cold-start run.
///
/// Crate-owned mirror of [`evm_fork_cache::cold_start::ColdStartRunReport`],
/// carrying the accumulated per-run counters.
#[derive(Clone, Debug, Default)]
pub struct ColdStartRunReport {
    /// Number of rounds executed.
    pub rounds: usize,
    /// Total verify slots requested across all rounds.
    pub verified_slots: usize,
    /// Total slots that changed and were injected.
    pub changed_slots: usize,
    /// Total accounts touched by discover calls, summed across calls and rounds.
    pub discovered_accounts: usize,
    /// Total slots touched by discover calls, summed across calls and rounds.
    pub discovered_slots: usize,
    /// Total verify + probe slots whose fetch failed.
    pub failed_slots: usize,
}

impl From<evm_fork_cache::cold_start::ColdStartRunReport> for ColdStartRunReport {
    fn from(report: evm_fork_cache::cold_start::ColdStartRunReport) -> Self {
        Self {
            rounds: report.rounds,
            verified_slots: report.verified_slots,
            changed_slots: report.changed_slots,
            discovered_accounts: report.discovered_accounts,
            discovered_slots: report.discovered_slots,
            failed_slots: report.failed_slots,
        }
    }
}

/// A hard error that aborts a cold-start round or run.
///
/// Crate-owned mirror of [`evm_fork_cache::cold_start::ColdStartError`], so the
/// public surface does not leak the upstream error. `#[non_exhaustive]` — an open
/// error vocabulary.
#[derive(Debug)]
#[non_exhaustive]
pub enum ColdStartError {
    /// A round declared verify/probe slots but the cache has no storage batch
    /// fetcher configured.
    NoBatchFetcher,
    /// A round declared probe-roots accounts but the cache has no account
    /// proof fetcher configured.
    NoAccountProofFetcher,
    /// The cache holds pending code seeds but has no account-fields fetcher
    /// to verify them with (fires only for pending-bearing rounds).
    NoAccountFieldsFetcher,
    /// The planner kept returning `Continue` past `max_rounds` executed rounds.
    RoundBudgetExceeded {
        /// The configured maximum number of executed rounds.
        max_rounds: usize,
    },
    /// A composed fetch/call error, carrying the un-flattened cause. Downcast
    /// the payload (or walk [`source`](std::error::Error::source)) — e.g. to
    /// [`evm_fork_cache::CacheError`] — for typed handling.
    Fetch(Box<dyn std::error::Error + Send + Sync + 'static>),
}

impl std::fmt::Display for ColdStartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoBatchFetcher => write!(f, "cold-start requires a storage batch fetcher"),
            Self::NoAccountProofFetcher => {
                write!(f, "cold-start requires an account proof fetcher")
            }
            Self::NoAccountFieldsFetcher => {
                write!(
                    f,
                    "cold-start code-seed verification requires an account fields fetcher"
                )
            }
            Self::RoundBudgetExceeded { max_rounds } => {
                write!(f, "cold-start round budget exceeded ({max_rounds})")
            }
            Self::Fetch(err) => write!(f, "cold-start fetch error: {err}"),
        }
    }
}

impl std::error::Error for ColdStartError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Fetch(err) => Some(&**err as &(dyn std::error::Error + 'static)),
            _ => None,
        }
    }
}

impl From<evm_fork_cache::cold_start::ColdStartError> for ColdStartError {
    fn from(err: evm_fork_cache::cold_start::ColdStartError) -> Self {
        use evm_fork_cache::cold_start::ColdStartError as Upstream;
        match err {
            Upstream::NoBatchFetcher => ColdStartError::NoBatchFetcher,
            Upstream::NoAccountProofFetcher => ColdStartError::NoAccountProofFetcher,
            Upstream::NoAccountFieldsFetcher => ColdStartError::NoAccountFieldsFetcher,
            Upstream::RoundBudgetExceeded { max_rounds } => {
                ColdStartError::RoundBudgetExceeded { max_rounds }
            }
            Upstream::Fetch(cause) => ColdStartError::Fetch(Box::new(cause)),
        }
    }
}

/// An adapter cold-start planner.
///
/// Extends the upstream [`ColdStartPlanner`](evm_fork_cache::cold_start::ColdStartPlanner)
/// surface with a [`finish`](Self::finish) hook that, after the run, mutates the
/// pool (metadata + status) and maps the accumulated state into a
/// [`ColdStartOutcome`].
///
/// Implementations are `'static`: a planner owns its address/layout/policy and
/// any state accumulated across rounds, so it does not borrow the pool or cache
/// for the lifetime of the run.
pub trait AdapterColdStartPlanner {
    /// The first plan to execute, derived from the current cached `state`.
    fn initial_plan(&mut self, state: &dyn StateView) -> ColdStartPlan;

    /// Decide whether to continue (with a next plan) or finish, given the
    /// just-completed round's `results` and the post-injection `state` view.
    fn on_results(&mut self, results: &ColdStartResults, state: &dyn StateView) -> ColdStartStep;

    /// Finalize after the run: mutate `pool` (metadata + status) and return the
    /// outcome built from the planner's accumulated state and the run `report`.
    fn finish(
        &mut self,
        pool: &mut PoolRegistration,
        report: &ColdStartRunReport,
    ) -> ColdStartOutcome;
}

/// Forwards an [`AdapterColdStartPlanner`] to the upstream
/// [`ColdStartPlanner`](evm_fork_cache::cold_start::ColdStartPlanner) trait.
///
/// A newtype rather than a blanket impl so the upstream trait is implemented
/// without trait-upcasting `Box<dyn AdapterColdStartPlanner>` and without leaking
/// the upstream trait into the public adapter surface.
struct Bridge<'a>(&'a mut dyn AdapterColdStartPlanner);

impl evm_fork_cache::cold_start::ColdStartPlanner for Bridge<'_> {
    fn initial_plan(
        &mut self,
        state: &dyn evm_fork_cache::StateView,
    ) -> evm_fork_cache::cold_start::ColdStartPlan {
        self.0.initial_plan(&UpstreamStateView(state)).into()
    }

    fn on_results(
        &mut self,
        results: &evm_fork_cache::cold_start::ColdStartResults,
        state: &dyn evm_fork_cache::StateView,
    ) -> evm_fork_cache::cold_start::ColdStartStep {
        let results = ColdStartResults::from(results.clone());
        self.0
            .on_results(&results, &UpstreamStateView(state))
            .into()
    }
}

impl AdapterRegistry {
    /// Cold-start `pool` through its adapter's planner.
    ///
    /// Resolves the adapter for `pool.protocol()`, builds its
    /// [`AdapterColdStartPlanner`], and drives it through
    /// [`EvmCache::run_cold_start`] with [`ColdStartConfig::default`]. The planner
    /// then [`finish`](AdapterColdStartPlanner::finish)es into a
    /// [`ColdStartOutcome`], mutating `pool`'s metadata and status.
    ///
    /// A missing adapter or an unsupported pool maps to
    /// `Ok(ColdStartOutcome::Unsupported(..))`; an upstream
    /// [`ColdStartError`] (e.g. a missing batch fetcher) propagates as `Err`. A
    /// per-slot fetch failure is *not* a run error — it is classified as
    /// [`SlotFetch::FetchFailed`](evm_fork_cache::cold_start::SlotFetch::FetchFailed)
    /// and handled by the planner's `on_results`.
    pub fn cold_start(
        &self,
        pool: &mut PoolRegistration,
        cache: &mut EvmCache,
        policy: ColdStartPolicy,
    ) -> Result<ColdStartOutcome, ColdStartError> {
        let Some(adapter) = self.adapter(pool.protocol()) else {
            return Ok(ColdStartOutcome::Unsupported(UnsupportedReason::Protocol(
                pool.protocol(),
            )));
        };

        let mut planner = match adapter.cold_start_planner(pool, policy) {
            Ok(planner) => planner,
            Err(reason) => return Ok(ColdStartOutcome::Unsupported(reason)),
        };

        let report = {
            let mut bridge = Bridge(planner.as_mut());
            cache
                .run_cold_start(&mut bridge, ColdStartConfig::default())
                .map_err(ColdStartError::from)?
        };
        let report = ColdStartRunReport::from(report);

        Ok(planner.finish(pool, &report))
    }
}
