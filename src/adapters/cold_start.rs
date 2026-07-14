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

#[cfg(feature = "live-runtime")]
use alloy_eips::BlockId;
#[cfg(feature = "live-runtime")]
use alloy_primitives::keccak256;
use alloy_primitives::{Address, B256, Bytes, U256};
use evm_fork_cache::CacheError as UpstreamCacheError;
#[cfg(feature = "live-runtime")]
use evm_fork_cache::bulk_storage::run_storage_program;
use evm_fork_cache::bulk_storage::{StorageProgram, run_storage_programs};
use evm_fork_cache::cache::{CodeSeedState, EvmCache};
use evm_fork_cache::cold_start::ColdStartConfig;
#[cfg(feature = "live-runtime")]
use evm_fork_cache::cold_start::{
    AccountCodeClaim, AccountProofOutcome, AccountProofRoundFetcher, AccountProofRoundRequest,
    PreparedAccountPatch, PreparedAccountValue, StorageRoundFetcher, StorageRoundRequest,
};
#[cfg(feature = "live-runtime")]
use std::collections::BTreeMap;
#[cfg(feature = "live-runtime")]
use std::sync::Arc;

use super::bytecode::AdapterCodeSeed;
use super::state::UpstreamStateView;
#[cfg(feature = "live-runtime")]
use super::storage::{V2_RESERVES_SLOT, V2_TOKEN0_SLOT, V2_TOKEN1_SLOT, decode_address_slot};
use super::storage_sync::{StorageSyncSpec, decode_storage_sync, storage_sync_spec_for_pool};
use super::{
    AdapterRegistry, CallOutcome, ColdStartOutcome, ColdStartPolicy, ColdStartReport,
    PoolRegistration, PoolStatus, SlotChange, StateView, UnsupportedReason,
};
#[cfg(feature = "live-runtime")]
use super::{
    AmmPreparedPoolState, AmmPreparedStorage, AmmStatePoint, ProtocolMetadata, UniswapV2Metadata,
};

#[cfg(feature = "uniswap-v3")]
use super::v3_sync::{V3SyncError, V3SyncSpec, decode_full_sync, full_sync_program};

// ---------------------------------------------------------------------------
// Crate-owned mirrors of the `evm_fork_cache::cold_start` vocabulary.
//
// Each mirror keeps upstream's variant / field NAMES so planner call-sites read
// the same; the `From` conversions bridge to/from upstream at the driver seam.
// ---------------------------------------------------------------------------

/// Verified-code-seed results surfaced through a [`ColdStartReport`].
///
/// Crate-owned mirror of [`evm_fork_cache::cache::CodeVerifyReport`], so the
/// public surface does not leak the upstream report. `verified` seeds are
/// confirmed against chain code; `mismatched` / `not_deployed` / `codeless`
/// seeds were contradicted and purged by upstream; `unverifiable` seeds could
/// not be checked (transport error / no sample) — the facade purges those too,
/// so the pool falls back to lazily fetching its real code.
///
/// [`ColdStartReport`]: super::ColdStartReport
///
/// `#[non_exhaustive]`: Construct via `Default` and field assignment.
#[non_exhaustive]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CodeSeedReport {
    /// Seeds confirmed against on-chain code (`CodeSeedState::Verified`).
    pub verified: Vec<Address>,
    /// Seeds contradicted by a differing on-chain code hash (purged upstream).
    pub mismatched: Vec<CodeSeedMismatch>,
    /// Seeded addresses with no code at the pinned block (purged upstream).
    pub not_deployed: Vec<Address>,
    /// Seeded addresses that exist but hold no code / are EOAs (purged upstream).
    pub codeless: Vec<Address>,
    /// Seeds whose verification could not complete, with the reason (purged by
    /// the facade so the pool never simulates over unverified code).
    pub unverifiable: Vec<(Address, String)>,
}

impl From<evm_fork_cache::cache::CodeVerifyReport> for CodeSeedReport {
    fn from(report: evm_fork_cache::cache::CodeVerifyReport) -> Self {
        Self {
            verified: report.verified,
            mismatched: report.mismatched.into_iter().map(Into::into).collect(),
            not_deployed: report.not_deployed,
            codeless: report.codeless,
            unverifiable: report.unverifiable,
        }
    }
}

/// One contradicted code-seed claim from verification.
///
/// Crate-owned mirror of [`evm_fork_cache::cache::CodeMismatch`].
///
/// `#[non_exhaustive]`: Construct via [`CodeSeedMismatch::new`].
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CodeSeedMismatch {
    /// The seeded address.
    pub address: Address,
    /// The hash the seed claimed (keccak256 of the seeded bytes).
    pub expected: B256,
    /// The on-chain `EXTCODEHASH` observed at the pinned block.
    pub actual: B256,
}

impl CodeSeedMismatch {
    /// A mismatch record: `address` claimed `expected`, the chain holds `actual`.
    pub fn new(address: Address, expected: B256, actual: B256) -> Self {
        Self {
            address,
            expected,
            actual,
        }
    }
}

impl From<evm_fork_cache::cache::CodeMismatch> for CodeSeedMismatch {
    fn from(mismatch: evm_fork_cache::cache::CodeMismatch) -> Self {
        Self {
            address: mismatch.address,
            expected: mismatch.expected,
            actual: mismatch.actual,
        }
    }
}

/// A single round of cold-start work, declared by an
/// [`AdapterColdStartPlanner`].
///
/// Crate-owned mirror of [`evm_fork_cache::cold_start::ColdStartPlan`]. All five
/// phases are optional; an empty plan is a valid no-op round.
///
/// `#[non_exhaustive]`: Construct via `Default` and field assignment, so future phases (e.g. the
/// upstream root-probe baseline) can land without breaking planner authors.
#[non_exhaustive]
#[derive(Clone, Debug, Default)]
pub struct ColdStartPlan {
    /// Slots to authoritatively re-fetch, classify, and inject when changed.
    pub verify: Vec<(Address, U256)>,
    /// Slots to classify at the pinned block without injecting.
    pub probe: Vec<(Address, U256)>,
    /// Accounts whose storage roots should be observed without injection.
    pub probe_roots: Vec<Address>,
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
            probe_roots: plan.probe_roots,
        }
    }
}

/// A read-only view-call whose touched storage and accounts are captured during
/// the discover phase.
///
/// Crate-owned mirror of [`evm_fork_cache::cold_start::ColdStartCall`].
///
/// `#[non_exhaustive]`: Construct via [`ColdStartCall::new`].
#[non_exhaustive]
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

impl ColdStartCall {
    /// A discover view-call from `from` to `to` with `calldata`, capturing every
    /// touched slot and account (no `restrict_to` filter).
    pub fn new(from: Address, to: Address, calldata: impl Into<Bytes>) -> Self {
        Self {
            from,
            to,
            calldata: calldata.into(),
            restrict_to: None,
        }
    }

    /// Filter the captured slots and accounts to `addresses`.
    pub fn with_restrict_to(mut self, addresses: impl IntoIterator<Item = Address>) -> Self {
        self.restrict_to = Some(addresses.into_iter().collect());
        self
    }
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
///
/// `#[non_exhaustive]`: Construct via `Default` and field assignment.
#[non_exhaustive]
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
///
/// `#[non_exhaustive]`: Construct via [`ColdStartCallResult::new`].
#[non_exhaustive]
#[derive(Clone, Debug)]
pub struct ColdStartCallResult {
    /// The classified outcome of the view-call.
    pub result: CallOutcome,
    /// The storage slots and accounts the call touched (after `restrict_to`).
    pub access: StorageAccessList,
}

impl ColdStartCallResult {
    /// A discover-call result from its classified outcome and access list.
    pub fn new(result: CallOutcome, access: StorageAccessList) -> Self {
        Self { result, access }
    }
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
///
/// `#[non_exhaustive]`: Construct via `Default` and field assignment.
#[non_exhaustive]
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
///
/// `#[non_exhaustive]`: Construct via [`SlotOutcome::new`].
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SlotOutcome {
    /// Contract whose storage slot was fetched.
    pub address: Address,
    /// Storage slot key.
    pub slot: U256,
    /// The classified result of fetching this slot.
    pub fetch: SlotFetch,
}

impl SlotOutcome {
    /// The classified outcome of fetching `slot` on `address`.
    pub fn new(address: Address, slot: U256, fetch: SlotFetch) -> Self {
        Self {
            address,
            slot,
            fetch,
        }
    }
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
///
/// `#[non_exhaustive]`: Construct via `Default` and field assignment.
#[non_exhaustive]
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

// ---------------------------------------------------------------------------
// One-shot bulk hydration (the fast multi-pool bootstrap default).
// ---------------------------------------------------------------------------

/// How to hydrate one pool's whole hot state in a **single** storage program
/// (`eth_call` with a code override), used by
/// [`cold_start_many`](AdapterRegistry::cold_start_many).
///
/// Two shapes cover every fast-eligible pool:
///
/// - `V3` — a concentrated-liquidity full sync ([`v3_sync`](super::v3_sync)),
///   which walks the entire tick bitmap in-program (only under the
///   `uniswap-v3` feature).
/// - `Flat` — a fixed / discovered flat read-set
///   ([`storage_sync`](super::storage_sync)) for V2, Solidly, and (once
///   discovered) Balancer/Curve.
enum HydrationKind {
    /// Concentrated-liquidity one-shot full sync for a V3-family pool.
    #[cfg(feature = "uniswap-v3")]
    V3 {
        /// The pool whose storage the program reads.
        pool: Address,
        /// The full-sync spec derived from the pool's storage layout.
        spec: V3SyncSpec,
    },
    /// Flat-slot one-shot sync for a pool with a known read-set.
    Flat {
        /// The flat read-set spec for the pool.
        spec: StorageSyncSpec,
    },
}

impl HydrationKind {
    /// The single storage program that hydrates the pool's whole hot state.
    fn program(&self) -> StorageProgram {
        match self {
            #[cfg(feature = "uniswap-v3")]
            HydrationKind::V3 { pool, spec } => full_sync_program(*pool, spec),
            HydrationKind::Flat { spec } => spec.program(),
        }
    }

    /// Decode `output`, inject the hydrated state into `cache`, and record what
    /// was loaded/changed for the pool's cold-start report. A decode failure is
    /// surfaced (un-flattened) so the caller can fall the pool back to the
    /// multi-round cold-start.
    fn decode_entries(&self, output: &Bytes) -> Result<Vec<(Address, U256, U256)>, HydrationError> {
        match self {
            #[cfg(feature = "uniswap-v3")]
            HydrationKind::V3 { pool, spec } => {
                let snapshot = decode_full_sync(spec, output).map_err(HydrationError::V3)?;
                Ok(snapshot
                    .storage_entries(spec)
                    .into_iter()
                    .map(|(slot, value)| (*pool, slot, value))
                    .collect())
            }
            HydrationKind::Flat { spec } => {
                let snapshot = decode_storage_sync(spec, output).map_err(HydrationError::Flat)?;
                Ok(snapshot.storage_entries())
            }
        }
    }

    fn apply(&self, cache: &mut EvmCache, output: &Bytes) -> Result<Hydrated, HydrationError> {
        let entries = self.decode_entries(output)?;
        Ok(inject_and_record(cache, entries))
    }
}

/// What a one-shot `HydrationKind::apply` wrote, surfaced into the fast
/// `cold_start_many` report.
struct Hydrated {
    /// Every `(address, slot)` the program loaded, in program order.
    verified: Vec<(Address, U256)>,
    /// The subset whose injected value differs from the prior cached value.
    changed: Vec<SlotChange>,
}

/// Peek prior values, inject the batch (layer-2 cold-prefetch, matching
/// `StorageSyncSnapshot::inject`), and record what was loaded/changed. Priors
/// are read through the non-faulting `EvmCache::cached_storage_value`, so the
/// bulk bootstrap path stays offline.
fn inject_and_record(cache: &mut EvmCache, entries: Vec<(Address, U256, U256)>) -> Hydrated {
    let verified: Vec<(Address, U256)> = entries.iter().map(|(a, s, _)| (*a, *s)).collect();
    let changed: Vec<SlotChange> = entries
        .iter()
        .filter_map(|(address, slot, value)| {
            let prior = cache
                .cached_storage_value(*address, *slot)
                .unwrap_or(U256::ZERO);
            (prior != *value).then(|| SlotChange::new(*address, *slot, prior, *value))
        })
        .collect();
    cache.inject_storage_batch(&entries);
    Hydrated { verified, changed }
}

/// Classify how — if at all — `pool` can be hydrated by a single one-shot
/// storage program.
///
/// - No pool address → `None`.
/// - A V3-family pool (`UniswapV3`/`PancakeV3`/`Slipstream`) that carries a
///   `storage_layout` → a [`HydrationKind::V3`] full sync (only when the
///   `uniswap-v3` feature is enabled).
/// - Otherwise, any pool whose flat read-set resolves via
///   [`storage_sync_spec_for_pool`] → a [`HydrationKind::Flat`] sync.
/// - Everything else → `None`.
///
/// [`supports_one_shot_hydration`] is exactly `hydration_kind(pool).is_some()`,
/// so classification and execution can never disagree.
fn hydration_kind(pool: &PoolRegistration) -> Option<HydrationKind> {
    let address = pool.key.address()?;

    #[cfg(feature = "uniswap-v3")]
    if let Some(spec) = v3_sync_spec(pool) {
        return Some(HydrationKind::V3 {
            pool: address,
            spec,
        });
    }
    // Without the `uniswap-v3` feature the V3 full-sync path is uncompiled, so
    // `storage_sync_spec_for_pool` (which rejects V3-family protocols) is the
    // only classifier; the address is validated above regardless.
    let _ = address;

    storage_sync_spec_for_pool(pool)
        .ok()
        .map(|spec| HydrationKind::Flat { spec })
}

/// Build the one-shot V3 full-sync spec for a V3-family pool, if it is eligible.
///
/// Eligibility (all required):
/// - the pool registered as a V3-family variant carrying an **explicit**
///   `storage_layout` (unlike [`layout_for`](super::storage::layout_for), this
///   does not derive a layout from `tick_spacing` alone), and
/// - that layout has a **positive** `tick_spacing` — `full_word_range` /
///   `v3_word_position` require it, so a non-positive spacing returns `None`
///   here and falls to the single-pool `cold_start` (which reports
///   `Unsupported`) rather than panicking in the fast path.
///
/// The **canonical Uniswap** spec (which bakes in Uniswap's fee-growth,
/// protocol-fees, and observation slot positions) is used only for genuine
/// Uniswap V3 pools. Other V3-family forks (PancakeSwap V3, Slipstream) use the
/// layout-only [`V3SyncSpec::core`] (slot0 + liquidity + the ticks/bitmap the
/// layout locates) so hydration never injects auxiliary state from unverified
/// slot positions. Extend a fork to the full spec once its layout is confirmed.
#[cfg(feature = "uniswap-v3")]
fn v3_sync_spec(pool: &PoolRegistration) -> Option<V3SyncSpec> {
    use super::ProtocolMetadata;
    let (metadata, family) = match &pool.metadata {
        ProtocolMetadata::UniswapV3(metadata) => (metadata, 0),
        ProtocolMetadata::PancakeV3(metadata) => (metadata, 1),
        ProtocolMetadata::Slipstream(metadata) => (metadata, 2),
        _ => return None,
    };
    let layout = metadata.storage_layout.filter(|l| l.tick_spacing > 0)?;
    Some(match family {
        0 => V3SyncSpec::uniswap(layout),
        1 => V3SyncSpec::pancake(layout),
        _ => V3SyncSpec::core(layout),
    })
}

/// Whether `pool` can be hydrated by a single one-shot storage program (the fast
/// multi-pool bootstrap path used by
/// [`cold_start_many`](AdapterRegistry::cold_start_many)).
///
/// Uniswap V2 (and any flat-read-set protocol) and V3-family pools that carry a
/// `storage_layout` qualify; a V3 pool without a layout, an addressless pool, or
/// a protocol with no persisted read-set (e.g. Curve before discovery) do not.
///
/// Defined as `hydration_kind(pool).is_some()`, so it always agrees with what
/// `cold_start_many` will actually attempt.
pub fn supports_one_shot_hydration(pool: &PoolRegistration) -> bool {
    hydration_kind(pool).is_some()
}

/// Whether `pool`'s registration metadata is already complete enough for the
/// one-shot fast path to finalize it `Ready` *without* the adapter planner's
/// [`finish`](AdapterColdStartPlanner::finish) (metadata merge + status
/// validation).
///
/// The fast path only warms the cache; a registration still missing identity
/// metadata — e.g. a Uniswap V2 pool without its `token0`/`token1`, which the
/// normal cold-start decodes from storage and merges — must fall back to the
/// multi-round [`cold_start`](AdapterRegistry::cold_start) so `finish` runs.
/// Registrations produced by factory discovery are already complete and stay on
/// the fast path.
///
/// For a V3-family pool `finish` *preserves* (never merges) metadata, so
/// completeness here means the fields a later `simulate_swap` needs (`fee`) plus
/// the layout the fast path already requires. A V3 fork with no fee tier (e.g. a
/// discovered Slipstream pool, whose `fee` is deliberately unset) therefore
/// forgoes the fast path and takes the normal `cold_start` — acceptable, since it
/// is discovery-only for quoting anyway. Balancer/Curve flat hydration only
/// applies once a discovered read-set exists, which itself is produced by a
/// prior discover→verify `cold_start` that already ran `finish`.
fn fast_metadata_complete(pool: &PoolRegistration) -> bool {
    use super::ProtocolMetadata;
    match &pool.metadata {
        ProtocolMetadata::UniswapV2(m) => m.token0.is_some() && m.token1.is_some(),
        ProtocolMetadata::UniswapV3(m)
        | ProtocolMetadata::PancakeV3(m)
        | ProtocolMetadata::Slipstream(m) => m.fee.is_some() && m.storage_layout.is_some(),
        // Solidly `finish` decodes+merges token0/token1 like V2, so require them
        // here too — otherwise the fast path would leave metadata tokens `None`
        // while the fallback populates them (an inconsistency for consumers that
        // read `PoolRegistration.metadata`).
        ProtocolMetadata::SolidlyV2(m) => {
            m.token0.is_some()
                && m.token1.is_some()
                && m.stable.is_some()
                && m.storage_layout.is_some()
        }
        ProtocolMetadata::BalancerV2(m) => m.vault.is_some() && !m.balance_slots.is_empty(),
        ProtocolMetadata::Curve(m) => !m.coins.is_empty() && !m.discovered_slots.is_empty(),
        // No known completeness contract → let the normal cold_start finalize it.
        ProtocolMetadata::Unknown | ProtocolMetadata::Custom(_) => false,
    }
}

/// A failure hydrating one pool from a one-shot storage program's output.
///
/// Used only to decide per-pool fallback inside
/// [`cold_start_many`](AdapterRegistry::cold_start_many); it is never returned
/// to the caller in this cycle. Keeps the upstream cause reachable through
/// [`source`](std::error::Error::source) — the cause is not stringified.
/// `#[non_exhaustive]` — an open error vocabulary.
#[derive(Debug)]
#[non_exhaustive]
pub enum HydrationError {
    /// The registration cannot use the one-shot worker path.
    Ineligible,
    /// Fetched state cannot produce the same ready registration as planner fallback.
    InvalidState(&'static str),
    /// The exact-hash storage program fetch failed.
    Fetch(Box<evm_fork_cache::StorageFetchError>),
    /// Decoding a V3 full-sync program's output failed.
    #[cfg(feature = "uniswap-v3")]
    V3(V3SyncError),
    /// Decoding a flat-slot sync program's output failed.
    Flat(super::storage_sync::StorageSyncError),
}

impl std::fmt::Display for HydrationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ineligible => write!(f, "pool is not eligible for one-shot hydration"),
            Self::InvalidState(message) => {
                write!(f, "one-shot state is not publishable: {message}")
            }
            Self::Fetch(_) => write!(f, "one-shot hydration provider fetch failed"),
            #[cfg(feature = "uniswap-v3")]
            Self::V3(_) => write!(f, "one-shot V3 hydration failed"),
            Self::Flat(_) => write!(f, "one-shot flat-slot hydration failed"),
        }
    }
}

impl std::error::Error for HydrationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Ineligible | Self::InvalidState(_) => None,
            Self::Fetch(err) => Some(err.as_ref()),
            #[cfg(feature = "uniswap-v3")]
            Self::V3(err) => Some(err as &(dyn std::error::Error + 'static)),
            Self::Flat(err) => Some(err as &(dyn std::error::Error + 'static)),
        }
    }
}

/// Fetch and decode one metadata-complete one-shot pool without mutating the actor cache.
#[cfg(feature = "live-runtime")]
#[allow(dead_code)]
pub(crate) async fn prepare_one_shot_pool<P>(
    registry: &AdapterRegistry,
    pool: PoolRegistration,
    baseline: AmmStatePoint,
    policy: ColdStartPolicy,
    provider: &P,
) -> Result<AmmPreparedPoolState, PreparedColdStartError>
where
    P: alloy_provider::Provider<alloy_network::AnyNetwork> + Clone + Send + Sync + 'static,
{
    prepare_pool_from_view(
        registry,
        pool,
        baseline,
        policy,
        Arc::new(EmptyPreparedState),
        Arc::new(provider.clone()),
        ResumableColdStartConfig::default(),
    )
    .await
}

/// Maximum work accepted from one resumable adapter planner.
#[cfg(feature = "live-runtime")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ResumableColdStartConfig {
    max_rounds: usize,
}

#[cfg(feature = "live-runtime")]
impl Default for ResumableColdStartConfig {
    fn default() -> Self {
        Self { max_rounds: 8 }
    }
}

/// Planner phases that need worker-side facilities beyond exact slot/proof reads.
#[cfg(feature = "live-runtime")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UnsupportedPreparedPhase {
    Accounts,
    Discover,
    ProbeRoots,
}

/// Failure to prepare one pool without mutating the canonical cache.
#[cfg(feature = "live-runtime")]
#[derive(Debug)]
pub(crate) enum PreparedColdStartError {
    Unsupported(UnsupportedReason),
    UnsupportedPhase(UnsupportedPreparedPhase),
    InvalidRoundBudget,
    RoundBudgetExceeded { max_rounds: usize },
    StorageFetch(evm_fork_cache::cold_start::StorageRoundFetchError),
    CodeSeed(super::bytecode::BytecodeTemplateError),
    CodeProofUnavailable,
    CodeProofFetch(evm_fork_cache::cold_start::AccountProofRoundFetchError),
    CodeProofMismatch { address: Address },
    CodeProofFailed { address: Address, reason: String },
    PlannerTranscriptDiverged,
    NotReady(Box<ColdStartOutcome>),
    Prepared(super::AmmPreparedStateError),
}

#[cfg(feature = "live-runtime")]
impl std::fmt::Display for PreparedColdStartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported(reason) => write!(f, "adapter cold-start is unsupported: {reason:?}"),
            Self::UnsupportedPhase(phase) => {
                write!(
                    f,
                    "background cold-start does not support the {phase:?} phase"
                )
            }
            Self::InvalidRoundBudget => {
                write!(f, "background cold-start max_rounds must be non-zero")
            }
            Self::RoundBudgetExceeded { max_rounds } => {
                write!(
                    f,
                    "background cold-start round budget exceeded ({max_rounds})"
                )
            }
            Self::StorageFetch(error) => write!(f, "background storage round failed: {error}"),
            Self::CodeSeed(error) => write!(f, "adapter code-seed rendering failed: {error}"),
            Self::CodeProofUnavailable => write!(
                f,
                "background code verification needs an account-proof fetcher"
            ),
            Self::CodeProofFetch(error) => {
                write!(f, "background account-proof round failed: {error}")
            }
            Self::CodeProofMismatch { address } => {
                write!(f, "runtime-code proof mismatch for {address}")
            }
            Self::CodeProofFailed { address, reason } => {
                write!(f, "runtime-code proof fetch failed for {address}: {reason}")
            }
            Self::PlannerTranscriptDiverged => {
                write!(f, "adapter planner diverged while replaying a prior round")
            }
            Self::NotReady(outcome) => {
                write!(
                    f,
                    "adapter cold-start did not produce a ready pool: {outcome:?}"
                )
            }
            Self::Prepared(error) => write!(f, "prepared pool is invalid: {error}"),
        }
    }
}

#[cfg(feature = "live-runtime")]
impl std::error::Error for PreparedColdStartError {}

#[cfg(feature = "live-runtime")]
struct WorkerStateView<'a> {
    base: &'a dyn StateView,
    storage: &'a BTreeMap<(Address, U256), U256>,
}

#[cfg(feature = "live-runtime")]
struct EmptyPreparedState;

#[cfg(feature = "live-runtime")]
impl StateView for EmptyPreparedState {
    fn storage(&self, _address: Address, _slot: U256) -> Option<U256> {
        None
    }
}

/// Adapter-facing immutable view over a queue-time EVM snapshot.
#[cfg(feature = "live-runtime")]
#[allow(dead_code)]
pub(crate) struct PreparedSnapshotState {
    snapshot: Arc<evm_fork_cache::EvmSnapshot>,
}

#[cfg(feature = "live-runtime")]
impl PreparedSnapshotState {
    #[allow(dead_code)]
    pub(crate) const fn new(snapshot: Arc<evm_fork_cache::EvmSnapshot>) -> Self {
        Self { snapshot }
    }
}

#[cfg(feature = "live-runtime")]
impl StateView for PreparedSnapshotState {
    fn storage(&self, address: Address, slot: U256) -> Option<U256> {
        self.snapshot.storage_value(address, slot)
    }
}

/// Attempt the single-program fast path for one pool.
///
/// Ineligible pools and storage-program/decode failures return `Ok(None)` so a
/// scheduler can construct a [`PreparedPoolJob`] and continue in bounded
/// rounds. Exact-hash code-proof failures remain hard errors.
#[cfg(feature = "live-runtime")]
pub(crate) async fn prepare_fast_pool<P>(
    registry: &AdapterRegistry,
    mut pool: PoolRegistration,
    baseline: AmmStatePoint,
    provider: &P,
    account_fetcher: Option<&AccountProofRoundFetcher>,
) -> Result<Option<AmmPreparedPoolState>, PreparedColdStartError>
where
    P: alloy_provider::Provider<alloy_network::AnyNetwork>,
{
    if registry.adapter(pool.protocol()).is_none() || !fast_metadata_complete(&pool) {
        return Ok(None);
    }
    let Some(kind) = hydration_kind(&pool) else {
        return Ok(None);
    };
    let fast = async {
        let output = run_storage_program(
            provider,
            BlockId::from((baseline.block_hash(), Some(true))),
            &kind.program(),
        )
        .await
        .map_err(|error| HydrationError::Fetch(Box::new(error)))?;
        let entries = kind.decode_entries(&output)?;
        finalize_prepared_registration(&mut pool, &entries)?;
        Ok::<_, HydrationError>(entries)
    }
    .await;
    let Ok(entries) = fast else {
        return Ok(None);
    };
    record_v3_prepared_slots(&mut pool, &entries);
    let adapter = registry.adapter(pool.protocol()).ok_or_else(|| {
        PreparedColdStartError::Unsupported(UnsupportedReason::Protocol(pool.protocol()))
    })?;
    let accounts = prepare_code_seeds(
        adapter.as_ref(),
        &pool,
        baseline,
        registry.code_seeding,
        account_fetcher,
    )?;
    let mut account_values = accounts.values().to_vec();
    account_values
        .extend(prepare_verified_code_targets(adapter.as_ref(), &pool, baseline, provider).await?);
    let accounts = PreparedAccountPatch::new(
        baseline.block_hash(),
        baseline.block_number(),
        account_values,
    );
    let storage = entries
        .into_iter()
        .map(|(address, slot, value)| AmmPreparedStorage::new(address, slot, value));
    pool.status = PoolStatus::Ready;
    let prepared = AmmPreparedPoolState::new(pool, baseline, storage)
        .map_err(PreparedColdStartError::Prepared)?
        .with_accounts(accounts);
    Ok(Some(prepared))
}

/// Fetch the real runtime for V3-family forks that cannot use the embedded
/// canonical Uniswap pool template. QuoterV2 calls the pool recursively, so an
/// immutable snapshot needs these bytes in addition to the pool's hydrated
/// storage. The proof and code are both pinned to the exact runtime baseline;
/// the actor revalidates the hash before installing the prepared patch.
#[cfg(feature = "live-runtime")]
async fn prepare_verified_code_target<P>(
    address: Address,
    baseline: AmmStatePoint,
    provider: &P,
) -> Result<PreparedAccountValue, PreparedColdStartError>
where
    P: alloy_provider::Provider<alloy_network::AnyNetwork>,
{
    let block = BlockId::from((baseline.block_hash(), Some(true)));
    let mut attempt = 0u32;
    let (code, proof) = loop {
        match tokio::try_join!(
            provider.get_code_at(address).block_id(block),
            provider.get_proof(address, Vec::new()).block_id(block),
        ) {
            Ok(result) => break result,
            Err(_) if attempt < 2 => {
                attempt += 1;
                tokio::time::sleep(std::time::Duration::from_millis(100 * (1 << attempt))).await;
            }
            Err(error) => {
                return Err(PreparedColdStartError::CodeProofFailed {
                    address,
                    reason: error.to_string(),
                });
            }
        }
    };
    if code.is_empty() {
        return Err(PreparedColdStartError::CodeProofFailed {
            address,
            reason: "pool account has no runtime code".to_owned(),
        });
    }
    let actual = keccak256(&code);
    if actual != proof.code_hash {
        return Err(PreparedColdStartError::CodeProofMismatch { address });
    }
    Ok(PreparedAccountValue::new(
        address,
        evm_fork_cache::AccountProof {
            storage_hash: proof.storage_hash,
            balance: proof.balance,
            nonce: proof.nonce,
            code_hash: proof.code_hash,
            slots: Vec::new(),
        },
        code,
    ))
}

#[cfg(feature = "live-runtime")]
pub(crate) async fn prepare_verified_code_targets<P>(
    adapter: &dyn super::AmmAdapter,
    pool: &PoolRegistration,
    baseline: AmmStatePoint,
    provider: &P,
) -> Result<Vec<PreparedAccountValue>, PreparedColdStartError>
where
    P: alloy_provider::Provider<alloy_network::AnyNetwork>,
{
    let mut values = Vec::new();
    for address in adapter.verified_code_targets(pool) {
        values.push(prepare_verified_code_target(address, baseline, provider).await?);
    }
    Ok(values)
}

#[cfg(feature = "live-runtime")]
fn record_v3_prepared_slots(pool: &mut PoolRegistration, entries: &[(Address, U256, U256)]) {
    let Some(address) = pool.key.address() else {
        return;
    };
    let mut slots = entries
        .iter()
        .filter_map(|(slot_address, slot, _)| (*slot_address == address).then_some(*slot))
        .collect::<Vec<_>>();
    slots.sort_unstable();
    slots.dedup();
    match &mut pool.metadata {
        ProtocolMetadata::UniswapV3(metadata)
        | ProtocolMetadata::PancakeV3(metadata)
        | ProtocolMetadata::Slipstream(metadata) => {
            metadata.warmed_slots = slots;
        }
        _ => {}
    }
}

/// Prepare a pool over an immutable snapshot view, preferring the one-shot
/// storage-program path and falling back to resumable adapter rounds.
#[cfg(feature = "live-runtime")]
pub(crate) async fn prepare_pool_from_view<P>(
    registry: &AdapterRegistry,
    pool: PoolRegistration,
    baseline: AmmStatePoint,
    policy: ColdStartPolicy,
    base: Arc<dyn StateView + Send + Sync>,
    provider: Arc<P>,
    config: ResumableColdStartConfig,
) -> Result<AmmPreparedPoolState, PreparedColdStartError>
where
    P: alloy_provider::Provider<alloy_network::AnyNetwork> + Send + Sync + 'static,
{
    let storage_fetcher = StorageRoundFetcher::from_provider(
        Arc::clone(&provider),
        evm_fork_cache::StorageBatchConfig::default(),
        evm_fork_cache::StorageFetchStrategy::default(),
    );
    let account_fetcher = AccountProofRoundFetcher::from_provider(Arc::clone(&provider), 8);

    if let Some(prepared) = prepare_fast_pool(
        registry,
        pool.clone(),
        baseline,
        provider.as_ref(),
        Some(&account_fetcher),
    )
    .await?
    {
        return Ok(prepared);
    }

    let adapter = registry.adapter(pool.protocol()).ok_or_else(|| {
        PreparedColdStartError::Unsupported(UnsupportedReason::Protocol(pool.protocol()))
    })?;
    let verified_accounts =
        prepare_verified_code_targets(adapter.as_ref(), &pool, baseline, provider.as_ref()).await?;
    prepare_resumable_pool(
        registry,
        pool,
        baseline,
        policy,
        base,
        PreparedPoolFetchers::new(storage_fetcher, Some(account_fetcher))
            .with_verified_accounts(verified_accounts),
        config,
    )
}

#[cfg(feature = "live-runtime")]
impl StateView for WorkerStateView<'_> {
    fn storage(&self, address: Address, slot: U256) -> Option<U256> {
        self.storage
            .get(&(address, slot))
            .copied()
            .or_else(|| self.base.storage(address, slot))
    }
}

#[derive(Clone)]
#[cfg(feature = "live-runtime")]
struct PreparedRound {
    plan: ColdStartPlan,
    results: ColdStartResults,
    storage: BTreeMap<(Address, U256), U256>,
}

/// Result of one bounded prepared cold-start quantum.
#[cfg(feature = "live-runtime")]
pub(crate) enum PreparedPoolStep {
    Continue {
        completed_rounds: usize,
        max_rounds: usize,
    },
    Done(Box<AmmPreparedPoolState>),
}

/// Provider handles used by one resumable pool job.
#[cfg(feature = "live-runtime")]
pub(crate) struct PreparedPoolFetchers {
    storage: StorageRoundFetcher,
    accounts: Option<AccountProofRoundFetcher>,
    verified_accounts: Vec<PreparedAccountValue>,
}

#[cfg(feature = "live-runtime")]
impl PreparedPoolFetchers {
    pub(crate) const fn new(
        storage: StorageRoundFetcher,
        accounts: Option<AccountProofRoundFetcher>,
    ) -> Self {
        Self {
            storage,
            accounts,
            verified_accounts: Vec::new(),
        }
    }

    pub(crate) fn with_verified_accounts(
        mut self,
        verified_accounts: Vec<PreparedAccountValue>,
    ) -> Self {
        self.verified_accounts = verified_accounts;
        self
    }
}

/// Worker-owned, `Send` resumable adapter cold-start state.
///
/// Adapter planners themselves are not `Send`. The job therefore persists a
/// compact transcript and reconstructs/replays the pure planner during each
/// synchronous [`step`](Self::step). No planner object crosses the scheduler's
/// await boundary, while every replay sees the exact overlay that followed that
/// historical round.
#[cfg(feature = "live-runtime")]
pub(crate) struct PreparedPoolJob {
    adapter: Arc<dyn super::AmmAdapter>,
    pool: PoolRegistration,
    baseline: AmmStatePoint,
    policy: ColdStartPolicy,
    base: Arc<dyn StateView + Send + Sync>,
    storage_fetcher: StorageRoundFetcher,
    account_patch: PreparedAccountPatch,
    config: ResumableColdStartConfig,
    rounds: Vec<PreparedRound>,
    report: ColdStartRunReport,
    accumulated: BTreeMap<(Address, U256), U256>,
}

#[cfg(feature = "live-runtime")]
impl PreparedPoolJob {
    pub(crate) fn new(
        registry: &AdapterRegistry,
        pool: PoolRegistration,
        baseline: AmmStatePoint,
        policy: ColdStartPolicy,
        base: Arc<dyn StateView + Send + Sync>,
        fetchers: PreparedPoolFetchers,
        config: ResumableColdStartConfig,
    ) -> Result<Self, PreparedColdStartError> {
        if config.max_rounds == 0 {
            return Err(PreparedColdStartError::InvalidRoundBudget);
        }
        let adapter = registry
            .adapter(pool.protocol())
            .ok_or_else(|| {
                PreparedColdStartError::Unsupported(UnsupportedReason::Protocol(pool.protocol()))
            })?
            .clone();
        // Validate planner construction before accepting the job. The planner
        // is intentionally dropped: a fresh instance is replayed per quantum.
        adapter
            .cold_start_planner(&pool, policy)
            .map_err(PreparedColdStartError::Unsupported)?;
        let account_patch = prepare_code_seeds(
            adapter.as_ref(),
            &pool,
            baseline,
            registry.code_seeding,
            fetchers.accounts.as_ref(),
        )?;
        let mut account_values = account_patch.values().to_vec();
        account_values.extend(fetchers.verified_accounts);
        let account_patch = PreparedAccountPatch::new(
            baseline.block_hash(),
            baseline.block_number(),
            account_values,
        );
        Ok(Self {
            adapter,
            pool,
            baseline,
            policy,
            base,
            storage_fetcher: fetchers.storage,
            account_patch,
            config,
            rounds: Vec::new(),
            report: ColdStartRunReport::default(),
            accumulated: BTreeMap::new(),
        })
    }

    /// Execute exactly one provider round and retain all continuation state.
    pub(crate) fn step(&mut self) -> Result<PreparedPoolStep, PreparedColdStartError> {
        if self.rounds.len() >= self.config.max_rounds {
            return Err(PreparedColdStartError::RoundBudgetExceeded {
                max_rounds: self.config.max_rounds,
            });
        }
        let mut planner = self
            .adapter
            .cold_start_planner(&self.pool, self.policy)
            .map_err(PreparedColdStartError::Unsupported)?;
        let empty_storage = BTreeMap::new();
        let empty = WorkerStateView {
            base: self.base.as_ref(),
            storage: &empty_storage,
        };
        let mut plan = planner.initial_plan(&empty);
        for historical in &self.rounds {
            if !same_prepared_plan(&plan, &historical.plan) {
                return Err(PreparedColdStartError::PlannerTranscriptDiverged);
            }
            let view = WorkerStateView {
                base: self.base.as_ref(),
                storage: &historical.storage,
            };
            plan = match planner.on_results(&historical.results, &view) {
                ColdStartStep::Continue(next) => next,
                ColdStartStep::Done => {
                    return Err(PreparedColdStartError::PlannerTranscriptDiverged);
                }
            };
        }
        reject_unsupported_phase(&plan)?;

        let request = StorageRoundRequest::new(
            self.baseline.block_hash(),
            plan.verify.iter().copied(),
            plan.probe.iter().copied(),
        );
        let (fetched, probed, patch) = self
            .storage_fetcher
            .fetch(&request)
            .map_err(PreparedColdStartError::StorageFetch)?
            .into_parts();
        let mut storage = self
            .rounds
            .last()
            .map(|round| round.storage.clone())
            .unwrap_or_default();
        let mut verified = Vec::new();
        for value in patch.values() {
            let address = value.address();
            let slot = value.slot();
            let new = value.value();
            let old = storage
                .get(&(address, slot))
                .copied()
                .or_else(|| self.base.storage(address, slot))
                .unwrap_or(U256::ZERO);
            if old != new {
                verified.push(SlotChange::new(address, slot, old, new));
            }
            storage.insert((address, slot), new);
            self.accumulated.insert((address, slot), new);
        }
        let results = ColdStartResults {
            verified,
            fetched: fetched.into_iter().map(Into::into).collect(),
            probed: probed.into_iter().map(Into::into).collect(),
            discovered: Vec::new(),
        };
        absorb_worker_round(&mut self.report, &plan, &results);
        self.rounds.push(PreparedRound {
            plan,
            results: results.clone(),
            storage: storage.clone(),
        });
        let view = WorkerStateView {
            base: self.base.as_ref(),
            storage: &storage,
        };
        match planner.on_results(&results, &view) {
            ColdStartStep::Continue(_) => Ok(PreparedPoolStep::Continue {
                completed_rounds: self.rounds.len(),
                max_rounds: self.config.max_rounds,
            }),
            ColdStartStep::Done => {
                let mut pool = self.pool.clone();
                let outcome = planner.finish(&mut pool, &self.report);
                let deferred = match outcome {
                    ColdStartOutcome::Ready(_) => Vec::new(),
                    ColdStartOutcome::ReadyWithDeferred(_, deferred) => deferred,
                    outcome => return Err(PreparedColdStartError::NotReady(Box::new(outcome))),
                };
                let prepared = AmmPreparedPoolState::new(
                    pool,
                    self.baseline,
                    self.accumulated.iter().map(|(&(address, slot), &value)| {
                        AmmPreparedStorage::new(address, slot, value)
                    }),
                )
                .map_err(PreparedColdStartError::Prepared)?
                .with_accounts(self.account_patch.clone())
                .with_deferred(deferred);
                Ok(PreparedPoolStep::Done(Box::new(prepared)))
            }
        }
    }
}

#[cfg(feature = "live-runtime")]
fn same_prepared_plan(left: &ColdStartPlan, right: &ColdStartPlan) -> bool {
    left.verify == right.verify
        && left.probe == right.probe
        && left.probe_roots == right.probe_roots
        && left.accounts == right.accounts
        && left.discover.len() == right.discover.len()
        && left
            .discover
            .iter()
            .zip(&right.discover)
            .all(|(left, right)| {
                left.from == right.from
                    && left.to == right.to
                    && left.calldata == right.calldata
                    && left.restrict_to == right.restrict_to
            })
}

#[cfg(feature = "live-runtime")]
fn reject_unsupported_phase(plan: &ColdStartPlan) -> Result<(), PreparedColdStartError> {
    if !plan.accounts.is_empty() {
        return Err(PreparedColdStartError::UnsupportedPhase(
            UnsupportedPreparedPhase::Accounts,
        ));
    }
    if !plan.discover.is_empty() {
        return Err(PreparedColdStartError::UnsupportedPhase(
            UnsupportedPreparedPhase::Discover,
        ));
    }
    if !plan.probe_roots.is_empty() {
        return Err(PreparedColdStartError::UnsupportedPhase(
            UnsupportedPreparedPhase::ProbeRoots,
        ));
    }
    Ok(())
}

/// Convenience driver used by the one-shot fallback and focused tests.
/// Scheduler integrations should own [`PreparedPoolJob`] and call one step per
/// scheduling quantum.
#[cfg(feature = "live-runtime")]
pub(crate) fn prepare_resumable_pool(
    registry: &AdapterRegistry,
    pool: PoolRegistration,
    baseline: AmmStatePoint,
    policy: ColdStartPolicy,
    base: Arc<dyn StateView + Send + Sync>,
    fetchers: PreparedPoolFetchers,
    config: ResumableColdStartConfig,
) -> Result<AmmPreparedPoolState, PreparedColdStartError> {
    let mut job = PreparedPoolJob::new(registry, pool, baseline, policy, base, fetchers, config)?;
    loop {
        match job.step()? {
            PreparedPoolStep::Continue {
                completed_rounds,
                max_rounds,
            } => debug_assert!(completed_rounds <= max_rounds),
            PreparedPoolStep::Done(prepared) => return Ok(*prepared),
        }
    }
}

#[cfg(feature = "live-runtime")]
fn absorb_worker_round(
    report: &mut ColdStartRunReport,
    plan: &ColdStartPlan,
    results: &ColdStartResults,
) {
    report.rounds += 1;
    report.verified_slots += plan.verify.len();
    report.changed_slots += results.verified.len();
    report.failed_slots += results
        .fetched
        .iter()
        .chain(&results.probed)
        .filter(|outcome| matches!(outcome.fetch, SlotFetch::FetchFailed { .. }))
        .count();
}

#[cfg(feature = "live-runtime")]
fn prepare_code_seeds(
    adapter: &dyn super::AmmAdapter,
    pool: &PoolRegistration,
    baseline: AmmStatePoint,
    enabled: bool,
    fetcher: Option<&AccountProofRoundFetcher>,
) -> Result<PreparedAccountPatch, PreparedColdStartError> {
    let seeds = if enabled {
        adapter
            .code_seeds(pool)
            .map_err(PreparedColdStartError::CodeSeed)?
    } else {
        Vec::new()
    };
    if seeds.is_empty() {
        return Ok(PreparedAccountPatch::new(
            baseline.block_hash(),
            baseline.block_number(),
            std::iter::empty(),
        ));
    }
    let fetcher = fetcher.ok_or(PreparedColdStartError::CodeProofUnavailable)?;
    let request = AccountProofRoundRequest::new(
        baseline.block_hash(),
        seeds
            .iter()
            .map(|seed| AccountCodeClaim::new(seed.address, seed.code_hash)),
    );
    let outcomes = fetcher
        .fetch(&request)
        .map_err(PreparedColdStartError::CodeProofFetch)?
        .into_outcomes();
    let mut values = Vec::with_capacity(seeds.len());
    for (seed, outcome) in seeds.into_iter().zip(outcomes) {
        match outcome {
            AccountProofOutcome::Verified { address, proof } => values.push(
                PreparedAccountValue::new(address, proof, seed.runtime_bytecode),
            ),
            AccountProofOutcome::Mismatch { address, .. } => {
                return Err(PreparedColdStartError::CodeProofMismatch { address });
            }
            AccountProofOutcome::FetchFailed { address, reason } => {
                return Err(PreparedColdStartError::CodeProofFailed { address, reason });
            }
            other => {
                return Err(PreparedColdStartError::CodeProofFailed {
                    address: other.address(),
                    reason: "unsupported account-proof outcome".to_string(),
                });
            }
        }
    }
    Ok(PreparedAccountPatch::new(
        baseline.block_hash(),
        baseline.block_number(),
        values,
    ))
}

#[cfg(feature = "live-runtime")]
fn finalize_prepared_registration(
    pool: &mut PoolRegistration,
    entries: &[(Address, U256, U256)],
) -> Result<(), HydrationError> {
    let ProtocolMetadata::UniswapV2(existing) = &pool.metadata else {
        return Ok(());
    };
    let address = pool.key.address().ok_or(HydrationError::Ineligible)?;
    let value = |slot| {
        entries
            .iter()
            .find_map(|(entry_address, entry_slot, value)| {
                (*entry_address == address && *entry_slot == slot).then_some(*value)
            })
            .ok_or(HydrationError::InvalidState("missing declared V2 slot"))
    };
    let token0 = decode_address_slot(value(V2_TOKEN0_SLOT)?);
    let token1 = decode_address_slot(value(V2_TOKEN1_SLOT)?);
    if token0.is_zero() || token1.is_zero() {
        return Err(HydrationError::InvalidState("V2 token address is zero"));
    }
    if value(V2_RESERVES_SLOT)?.is_zero() {
        return Err(HydrationError::InvalidState(
            "V2 reserves are degenerate zero",
        ));
    }
    pool.metadata = ProtocolMetadata::UniswapV2(UniswapV2Metadata {
        token0: Some(token0),
        token1: Some(token1),
        fee_bps: existing.fee_bps,
    });
    Ok(())
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

        // Seeding is a pure optimization over the lazy real-code fetch at first
        // simulate (see `sim.rs`), so an adapter render error is a safe skip
        // (no seeding for this pool), never a failure. `code_seeds` stays `None`
        // when nothing was seeded.
        let code_seed_report = if self.code_seeding && cache.account_fields_fetcher().is_some() {
            match adapter.code_seeds(pool) {
                Ok(seeds) if !seeds.is_empty() => Some(seed_and_verify(cache, seeds)),
                _ => None,
            }
        } else {
            None
        };

        let report = {
            let mut bridge = Bridge(planner.as_mut());
            cache
                .run_cold_start(&mut bridge, ColdStartConfig::default())
                .map_err(ColdStartError::from)?
        };
        let report = ColdStartRunReport::from(report);

        let outcome = planner.finish(pool, &report);
        Ok(attach_code_seeds(outcome, code_seed_report))
    }

    /// Cold-start many pools at once, making the high-performance one-shot
    /// storage load the **default** for multi-pool bootstrap.
    ///
    /// Where the per-pool [`cold_start`](Self::cold_start) runs a bounded
    /// multi-round planner for every pool — so requests scale with pool count —
    /// this path collapses the fast-eligible pools into a fixed number of
    /// phases:
    ///
    /// 1. **Classify.** A pool is `fast` when its adapter is registered,
    ///    [`supports_one_shot_hydration`] holds, **and** its registration is
    ///    already metadata-complete for its protocol (the fast path warms the
    ///    cache and marks `Ready` without running the planner's `finish`, so a
    ///    pool whose identity metadata still needs decoding/merging — e.g. a
    ///    bare Uniswap V2 registration without `token0`/`token1` — is left to
    ///    `fallback`). Everything else is `fallback`.
    /// 2. **Batched seed + verify.** Every fast pool's [`code_seeds`] are seeded
    ///    together (same skip rules as the single-pool path) and, if any are
    ///    pending, verified in **one** account-fields call — unverifiable seeds
    ///    are purged so no address is left `Pending`.
    /// 3. **Bundled hydration.** One [`run_storage_programs`] call hydrates all
    ///    fast pools (distinct targets bundle into a single multicall
    ///    `eth_call`). A pool whose program errored or failed to decode is moved
    ///    to the fallback set.
    /// 4. **Fallback.** Every fallback pool (originally classified plus any
    ///    fast-hydration failure) is finalized through the normal
    ///    [`cold_start`](Self::cold_start); its seeding is a no-op for anything
    ///    already `Verified` in step 2.
    /// 5. **Quote-target warming** (`Strict`/`Eager` only). The distinct
    ///    canonical quote entrypoints across all pools — the QuoterV2 / Router02
    ///    / Balancer vault each pool's [`quote_code_targets`] resolves against the
    ///    registry's [`sim_config`](Self::with_sim_config) — have their bytecode
    ///    fetched once so the first [`simulate_swap`] runs offline instead of a
    ///    lazy `eth_getCode`. V3-family forks whose pool runtime cannot be
    ///    reconstructed from the canonical Uniswap template (Pancake V3 and
    ///    Slipstream) also warm the pool account itself because the quoter calls
    ///    it recursively. Best-effort (an offline backend leaves the lazy fetch
    ///    in place); shared entrypoints still collapse to one fetch per family.
    ///
    /// Returns one [`ColdStartOutcome`] per input pool, in **input order**. An
    /// empty slice returns `Ok(vec![])` without touching `provider`.
    ///
    /// Fast hydration needs a live provider; over an empty/offline provider
    /// every `eth_call` errors and every fast pool gracefully falls back — never
    /// a panic. An upstream cold-start error from the fallback path (e.g. a
    /// missing batch fetcher) still propagates as `Err`.
    ///
    /// **Report detail.** A pool finalized on the fast (bundled) path returns a
    /// [`ColdStartReport`] carrying its `verified_slots` (every slot the bundled
    /// program loaded) and `changed_slots` (those whose value differed from the
    /// prior cache). Two fields the single-pool path fills stay empty: `applied`
    /// (the bundled path injects straight into the cache without materializing a
    /// `StateDiff`) and `code_seeds` (seed verification is batched across all
    /// fast pools, not attributed per pool). A pool that falls back to
    /// [`cold_start`](Self::cold_start) carries that method's full report. The
    /// step-5 quote-target warming is likewise batch-level (one fetch per
    /// distinct entrypoint, shared across pools) and so is not surfaced in any
    /// per-pool report.
    ///
    /// [`code_seeds`]: super::AmmAdapter::code_seeds
    /// [`quote_code_targets`]: PoolRegistration::quote_code_targets
    /// [`simulate_swap`]: super::AmmAdapter::simulate_swap
    pub async fn cold_start_many<P>(
        &self,
        pools: &mut [PoolRegistration],
        cache: &mut EvmCache,
        provider: &P,
        policy: ColdStartPolicy,
    ) -> Result<Vec<ColdStartOutcome>, ColdStartError>
    where
        P: alloy_provider::Provider<alloy_network::AnyNetwork>,
    {
        if pools.is_empty() {
            return Ok(Vec::new());
        }

        // Step 1: partition into fast (adapter present AND one-shot-eligible)
        // and fallback. `fast` carries the pool index and its hydration kind;
        // `is_fallback` marks every index not (yet) on the fast path.
        let mut fast: Vec<(usize, HydrationKind)> = Vec::new();
        let mut is_fallback = vec![true; pools.len()];
        for (index, pool) in pools.iter().enumerate() {
            if self.adapter(pool.protocol()).is_none() {
                continue;
            }
            // Fast-path only registrations that are already metadata-complete for
            // their protocol. The fast path warms the cache and finalizes `Ready`
            // WITHOUT running the adapter planner's `finish()` (metadata merge +
            // status validation), so a pool still missing identity metadata must
            // take the normal multi-round `cold_start` to be finalized correctly.
            if let Some(kind) = hydration_kind(pool)
                && fast_metadata_complete(pool)
            {
                is_fallback[index] = false;
                fast.push((index, kind));
            }
        }

        // Step 2: seed + verify every fast pool's code claims in one batch,
        // then a single verify pass. Mirrors the single-pool gating exactly.
        if self.code_seeding && cache.account_fields_fetcher().is_some() {
            let mut any_pending = false;
            for (index, _) in &fast {
                let pool = &pools[*index];
                if let Some(adapter) = self.adapter(pool.protocol()) {
                    // An adapter render error is a safe skip (no seeds for this
                    // pool), never a failure — matching the single-pool path.
                    if let Ok(seeds) = adapter.code_seeds(pool) {
                        any_pending |= seed_batch(cache, seeds);
                    }
                }
            }
            if any_pending {
                // The report is not surfaced here (fast outcomes leave
                // `code_seeds = None`); this call's job is to leave no address
                // `Pending`, purging any it cannot verify.
                let _ = verify_pending_seeds(cache);
            }
        }

        // Step 3: build one program per fast pool (stable order) and hydrate
        // them all in a single bundled call.
        let mut outcomes: Vec<Option<ColdStartOutcome>> = (0..pools.len()).map(|_| None).collect();
        if !fast.is_empty() {
            let programs: Vec<StorageProgram> =
                fast.iter().map(|(_, kind)| kind.program()).collect();
            let results = run_storage_programs(provider, cache.block(), &programs).await;

            for ((index, kind), result) in fast.iter().zip(results) {
                match result.map(|output| kind.apply(cache, &output)) {
                    Ok(Ok(hydrated)) => {
                        let pool = &mut pools[*index];
                        pool.status = PoolStatus::Ready;
                        let mut report = ColdStartReport::new(pool.key.clone(), policy);
                        report.status = PoolStatus::Ready;
                        report.verified_slots = hydrated.verified;
                        report.changed_slots = hydrated.changed;
                        outcomes[*index] = Some(ColdStartOutcome::Ready(report));
                    }
                    // Either the program call errored (offline / gas / transport)
                    // or its output failed to decode: fall this pool back.
                    _ => is_fallback[*index] = true,
                }
            }
        }

        // Step 4: finalize every fallback pool through the normal cold-start.
        for (index, pool) in pools.iter_mut().enumerate() {
            if is_fallback[index] {
                outcomes[index] = Some(self.cold_start(pool, cache, policy)?);
            }
        }

        // Step 5: under an eager policy, pre-warm the canonical quote
        // entrypoints' bytecode (QuoterV2 / Router02 / vault) so the first
        // `simulate_swap` against each pool runs offline instead of paying a lazy
        // `eth_getCode` on the hot path. Runs after fallback finalization so every
        // pool's metadata — and thus its resolved quote target — is final.
        // Best-effort: an offline backend or a target that carries no code simply
        // leaves the lazy first-quote fetch in place, never a cold-start failure
        // (mirroring code-seeding). `ensure_account` is idempotent and cached, so
        // this is one fetch per *distinct* address, shared across a whole family.
        if matches!(policy, ColdStartPolicy::Strict | ColdStartPolicy::Eager) {
            for target in self.collect_quote_code_targets(pools) {
                let _ = cache.ensure_account(target).await;
            }
        }

        // Step 6: every slot is now populated (fast success or fallback).
        Ok(outcomes
            .into_iter()
            .map(|outcome| outcome.expect("every pool is fast-hydrated or fell back"))
            .collect())
    }

    /// The distinct quote-target code addresses to pre-warm for `pools`, in
    /// first-seen order. Folds each pool's
    /// [`quote_code_targets`](PoolRegistration::quote_code_targets) (resolved
    /// against the registry's [`sim_config`](Self::with_sim_config)) and
    /// de-duplicates. Pancake V3 and Slipstream additionally contribute their
    /// own pool account because their real runtime cannot use the canonical
    /// Uniswap V3 code seed and QuoterV2 calls the pool recursively. Pools with
    /// no registered adapter are skipped (they cannot be quoted, so warming
    /// their target would be wasted).
    fn collect_quote_code_targets(&self, pools: &[PoolRegistration]) -> Vec<Address> {
        let mut targets: Vec<Address> = Vec::new();
        for pool in pools {
            if self.adapter(pool.protocol()).is_none() {
                continue;
            }
            for target in pool.quote_code_targets(&self.sim_config) {
                if !targets.contains(&target) {
                    targets.push(target);
                }
            }
            if let Some(adapter) = self.adapter(pool.protocol()) {
                for address in adapter.verified_code_targets(pool) {
                    if !targets.contains(&address) {
                        targets.push(address);
                    }
                }
            }
        }
        targets
    }
}

/// Attach the verified-code-seed results to the [`ColdStartReport`] carried by a
/// report-bearing outcome. `Unsupported` carries no report, so it is unchanged.
///
/// [`ColdStartReport`]: super::ColdStartReport
fn attach_code_seeds(
    outcome: ColdStartOutcome,
    code_seed_report: Option<CodeSeedReport>,
) -> ColdStartOutcome {
    match outcome {
        ColdStartOutcome::Ready(mut report) => {
            report.code_seeds = code_seed_report;
            ColdStartOutcome::Ready(report)
        }
        ColdStartOutcome::ReadyWithDeferred(mut report, deferred) => {
            report.code_seeds = code_seed_report;
            ColdStartOutcome::ReadyWithDeferred(report, deferred)
        }
        ColdStartOutcome::NeedsRepair(mut report, repair) => {
            report.code_seeds = code_seed_report;
            ColdStartOutcome::NeedsRepair(report, repair)
        }
        ColdStartOutcome::Unsupported(reason) => ColdStartOutcome::Unsupported(reason),
    }
}

/// Seed each adapter code claim, then verify — leaving every seeded address
/// either `Verified` or unmarked (never `Pending`).
///
/// Infallible: seeding is an optimization over the lazy real-code fetch, so a
/// conflict, an empty seed, an unverifiable seed, or a verify error all degrade
/// to "purge / skip and fall back to lazy real code" rather than an error.
///
/// The two phases are factored into [`seed_batch`] and [`verify_pending_seeds`]
/// so the batched [`cold_start_many`](AdapterRegistry::cold_start_many) path can
/// seed many pools' claims and then verify them all in **one** call.
fn seed_and_verify(cache: &mut EvmCache, seeds: Vec<AdapterCodeSeed>) -> CodeSeedReport {
    let any_pending = seed_batch(cache, seeds);
    if !any_pending {
        return CodeSeedReport::default();
    }
    verify_pending_seeds(cache)
}

/// Seed a batch of adapter code claims into `cache`, applying the same skip
/// rules the single-pool path uses (skip a `Verified` same-hash claim, never
/// overwrite an `Etched` claim, treat an identical `Pending` claim as already
/// queued; catch `CodeSeedConflict`/`CodeSeedEmpty`/any other seed error as a
/// safe skip). Returns whether any address is now `Pending` and so needs a
/// [`verify_pending_seeds`] pass.
fn seed_batch(cache: &mut EvmCache, seeds: impl IntoIterator<Item = AdapterCodeSeed>) -> bool {
    let mut any_pending = false;
    for seed in seeds {
        match cache.code_seed_state(&seed.address) {
            // Already confirmed with the same hash: never re-seed / re-verify.
            Some(CodeSeedState::Verified { code_hash, .. }) if *code_hash == seed.code_hash => {
                continue;
            }
            // A deliberate local etch is authoritative: never overwrite it.
            Some(CodeSeedState::Etched { .. }) => continue,
            // Idempotent: an identical pending claim is already queued to verify.
            Some(CodeSeedState::Pending { code_hash }) if *code_hash == seed.code_hash => {
                any_pending = true;
                continue;
            }
            _ => {}
        }

        match cache.seed_account_code(seed.address, seed.runtime_bytecode) {
            Ok(_) => {
                // A warm-cache same-hash match marks `Verified` immediately; only
                // a genuinely new claim lands `Pending` and needs verification.
                if matches!(
                    cache.code_seed_state(&seed.address),
                    Some(CodeSeedState::Pending { .. })
                ) {
                    any_pending = true;
                }
            }
            // The cache already holds authoritative RPC-origin code with a
            // different hash, or the seed was empty: skip (existing code wins).
            Err(UpstreamCacheError::CodeSeedConflict { .. })
            | Err(UpstreamCacheError::CodeSeedEmpty { .. }) => {}
            // Any other seeding error is non-fatal too: skip, fall back to lazy.
            Err(_) => {}
        }
    }
    any_pending
}

/// Verify every pending code seed in `cache` in one call, purging any address
/// that could not be confirmed so no unverified bytes are left behind, and
/// return the resulting [`CodeSeedReport`].
///
/// Call only when [`seed_batch`] reported at least one pending seed.
fn verify_pending_seeds(cache: &mut EvmCache) -> CodeSeedReport {
    match cache.verify_code_seeds() {
        Ok(report) => {
            // Upstream leaves an unverifiable seed `Pending`; purging it prevents
            // simulating over unverified code (it falls back to lazy real code).
            for (address, _reason) in &report.unverifiable {
                cache.purge_account(*address);
            }
            CodeSeedReport::from(report)
        }
        Err(_) => {
            // Verification could not run: purge every still-pending seed so no
            // unverified bytes remain, and report nothing.
            for address in cache.pending_code_seeds() {
                cache.purge_account(address);
            }
            CodeSeedReport::default()
        }
    }
}

#[cfg(all(test, feature = "uniswap-v3"))]
mod tests {
    use super::*;
    use crate::adapters::ConcentratedLiquidityAdapter;
    use crate::adapters::storage::V3StorageLayout;
    use crate::adapters::types::{
        PoolKey, PoolRegistration, ProtocolMetadata, UniswapV2Metadata, V3Metadata,
    };
    use crate::adapters::v3_sync::V3SyncSpec;
    use alloy_primitives::Address;
    use std::sync::Arc;

    /// A genuine Uniswap V3 pool takes the canonical full spec (fee-growth,
    /// protocol-fees, and observation slots baked in).
    #[test]
    fn uniswap_v3_uses_the_canonical_full_spec() {
        let layout = V3StorageLayout::uniswap(10);
        let pool = PoolRegistration::new(PoolKey::UniswapV3(Address::repeat_byte(0x11)))
            .with_metadata(ProtocolMetadata::UniswapV3(
                V3Metadata::default()
                    .with_fee(500)
                    .with_storage_layout(layout),
            ));
        assert_eq!(v3_sync_spec(&pool), Some(V3SyncSpec::uniswap(layout)));
    }

    /// PancakeSwap V3 uses its verified shifted full layout; Slipstream remains
    /// on the layout-only `core` spec until its extra slots are verified.
    #[test]
    fn pancake_uses_its_full_spec_while_slipstream_uses_core() {
        let pancake_layout = V3StorageLayout::pancake(10);
        let pancake = PoolRegistration::new(PoolKey::PancakeV3(Address::repeat_byte(0x22)))
            .with_metadata(ProtocolMetadata::PancakeV3(
                V3Metadata::default()
                    .with_fee(2500)
                    .with_storage_layout(pancake_layout),
            ));
        assert_eq!(
            v3_sync_spec(&pancake),
            Some(V3SyncSpec::pancake(pancake_layout))
        );

        let slip_layout = V3StorageLayout::slipstream(100);
        let slip = PoolRegistration::new(PoolKey::Slipstream(Address::repeat_byte(0x33)))
            .with_metadata(ProtocolMetadata::Slipstream(
                V3Metadata::default().with_storage_layout(slip_layout),
            ));
        assert_eq!(v3_sync_spec(&slip), Some(V3SyncSpec::core(slip_layout)));
    }

    /// A non-positive tick spacing would panic in `full_word_range` /
    /// `v3_word_position`; `v3_sync_spec` must return `None` so the pool falls
    /// back to the single-pool `cold_start` (Unsupported) instead of panicking in
    /// the `cold_start_many` fast path.
    #[test]
    fn non_positive_tick_spacing_is_not_fast_eligible() {
        let layout = V3StorageLayout::uniswap(0);
        let pool = PoolRegistration::new(PoolKey::UniswapV3(Address::repeat_byte(0x44)))
            .with_metadata(ProtocolMetadata::UniswapV3(
                V3Metadata::default()
                    .with_fee(500)
                    .with_storage_layout(layout),
            ));
        assert_eq!(v3_sync_spec(&pool), None);
        assert!(!supports_one_shot_hydration(&pool));
    }

    /// The eager warm step's target collection: the distinct quote-code targets
    /// across the pools, resolved via each pool's metadata. A quoter shared by two
    /// V3 pools collapses to one entry, a distinct quoter adds a second
    /// (order-stable), a fork whose runtime cannot be reconstructed also adds
    /// its pool account, and a pool whose protocol has no registered adapter
    /// contributes nothing (it cannot be quoted, so warming its target is wasted).
    #[test]
    fn collect_quote_code_targets_dedupes_and_skips_unadaptered_pools() {
        let (q1, q2) = (Address::repeat_byte(0xd1), Address::repeat_byte(0xd2));
        let mut registry = AdapterRegistry::new().with_code_seeding(false);
        registry
            .register_adapter(Arc::new(ConcentratedLiquidityAdapter::default()))
            .expect("register the V3-family adapter");

        let v3 = |tag: u8, quoter: Address| {
            PoolRegistration::new(PoolKey::UniswapV3(Address::repeat_byte(tag))).with_metadata(
                ProtocolMetadata::UniswapV3(V3Metadata::default().with_quoter(quoter)),
            )
        };
        let pancake_pool = Address::repeat_byte(0x05);
        let pools = vec![
            v3(0x01, q1),
            v3(0x02, q1), // same quoter -> deduped away
            v3(0x03, q2), // distinct quoter -> a second, later entry
            PoolRegistration::new(PoolKey::PancakeV3(pancake_pool)).with_metadata(
                ProtocolMetadata::PancakeV3(V3Metadata::default().with_quoter(q1)),
            ),
            // A V2 pool with real metadata but NO registered V2 adapter: skipped.
            PoolRegistration::new(PoolKey::UniswapV2(Address::repeat_byte(0x04)))
                .with_metadata(ProtocolMetadata::UniswapV2(UniswapV2Metadata::default())),
        ];

        assert_eq!(
            registry.collect_quote_code_targets(&pools),
            vec![q1, q2, pancake_pool]
        );
    }
}

#[cfg(all(test, feature = "live-runtime", feature = "uniswap-v2"))]
mod hydration_tests {
    use super::*;
    use crate::adapters::{
        AmmAdapter, CustomPoolKey, DeferredWork, PoolKey, ProtocolId, ProtocolMetadata,
        UniswapV2Adapter, UniswapV2Metadata,
    };
    use alloy_primitives::{Address, B256};
    use alloy_provider::RootProvider;
    use alloy_provider::network::AnyNetwork;
    use alloy_rpc_client::RpcClient;
    use alloy_transport::mock::Asserter;
    use evm_fork_cache::StorageFetchError;
    use evm_fork_cache::cold_start::StorageRoundFetcher;
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    async fn mock_cache() -> EvmCache {
        let provider = RootProvider::<AnyNetwork>::new(RpcClient::mocked(Asserter::new()));
        EvmCache::new(Arc::new(provider)).await
    }

    #[test]
    fn prepared_pool_job_can_cross_scheduler_await_boundaries() {
        fn assert_send<T: Send>() {}
        assert_send::<PreparedPoolJob>();
    }

    /// `inject_and_record` (the fast `cold_start_many` hydration path) reports
    /// every loaded slot in `verified`, only the slots whose value actually
    /// differs from the prior cache in `changed`, and injects the values.
    #[tokio::test(flavor = "multi_thread")]
    async fn inject_and_record_reports_loaded_and_changed_slots() {
        let pool = Address::repeat_byte(0x51);
        let unchanged = U256::from(1);
        let changed = U256::from(2);
        let fresh = U256::from(3);
        let value = U256::from(0xabc_u64);

        let mut cache = mock_cache().await;
        // Pre-seed two slots: one re-injected with the SAME value (no change),
        // one overwritten with a different value (a change).
        cache.inject_storage_batch(&[
            (pool, unchanged, value),
            (pool, changed, U256::from(10_u64)),
        ]);

        let entries = vec![
            (pool, unchanged, value),            // equals prior -> not "changed"
            (pool, changed, U256::from(20_u64)), // differs from prior -> "changed"
            (pool, fresh, value),                // cold prior (0) -> "changed"
        ];
        let hydrated = inject_and_record(&mut cache, entries);

        // Every loaded slot is verified, in program order.
        assert_eq!(
            hydrated.verified,
            vec![(pool, unchanged), (pool, changed), (pool, fresh)]
        );
        // Only the two that differ from their prior value are recorded as changed.
        assert_eq!(hydrated.changed.len(), 2);
        assert!(hydrated.changed.iter().any(|c| c.slot == changed
            && c.old == U256::from(10_u64)
            && c.new == U256::from(20_u64)));
        assert!(
            hydrated
                .changed
                .iter()
                .any(|c| c.slot == fresh && c.old == U256::ZERO && c.new == value)
        );
        // The fresh values landed in the cache.
        assert_eq!(
            cache.cached_storage_value(pool, changed),
            Some(U256::from(20_u64))
        );
        assert_eq!(cache.cached_storage_value(pool, fresh), Some(value));
    }

    struct EmptyState;

    impl StateView for EmptyState {
        fn storage(&self, _address: Address, _slot: U256) -> Option<U256> {
            None
        }
    }

    /// A registration that is deliberately ineligible for one-shot hydration
    /// still becomes a hash-pinned prepared artifact through its adapter's
    /// planner, without touching an `EvmCache`.
    #[test]
    fn resumable_fallback_prepares_an_incomplete_v2_registration() {
        let pool_address = Address::repeat_byte(0x61);
        let token0 = Address::repeat_byte(0xa0);
        let token1 = Address::repeat_byte(0xa1);
        let reserves = U256::from(7_u64) | (U256::from(11_u64) << 112);
        let baseline = AmmStatePoint::post_block(1, 100, B256::repeat_byte(0x42));
        let values = std::collections::HashMap::from([
            (V2_TOKEN0_SLOT, U256::from_be_slice(token0.as_slice())),
            (V2_TOKEN1_SLOT, U256::from_be_slice(token1.as_slice())),
            (V2_RESERVES_SLOT, reserves),
        ]);
        let fetcher = StorageRoundFetcher::new(Arc::new(move |requests, block| {
            assert_eq!(block, BlockId::from((baseline.block_hash(), Some(true))));
            requests
                .into_iter()
                .map(|(address, slot)| {
                    (
                        address,
                        slot,
                        Ok(*values.get(&slot).expect("planner requested a V2 slot")),
                    )
                })
                .collect()
        }));
        let mut registry = AdapterRegistry::new().with_code_seeding(false);
        registry
            .register_adapter(Arc::new(UniswapV2Adapter::default()))
            .unwrap();
        let pool = PoolRegistration::new(PoolKey::UniswapV2(pool_address)).with_metadata(
            ProtocolMetadata::UniswapV2(UniswapV2Metadata::default().with_fee_bps(30)),
        );

        let prepared = prepare_resumable_pool(
            &registry,
            pool,
            baseline,
            ColdStartPolicy::Eager,
            Arc::new(EmptyState),
            PreparedPoolFetchers::new(fetcher, None),
            ResumableColdStartConfig::default(),
        )
        .expect("the slot-only fallback should prepare the pool");

        assert_eq!(prepared.baseline(), baseline);
        assert_eq!(prepared.registration().status, PoolStatus::Ready);
        assert_eq!(prepared.registration().tokens(), Some(vec![token0, token1]));
        assert_eq!(prepared.storage().len(), 3);
        assert!(prepared.storage().iter().any(|entry| {
            entry.address() == pool_address
                && entry.slot() == V2_RESERVES_SLOT
                && entry.value() == reserves
        }));
    }

    #[test]
    fn resumable_lazy_completion_preserves_deferred_work() {
        let pool_address = Address::repeat_byte(0x64);
        let baseline = AmmStatePoint::post_block(1, 102, B256::repeat_byte(0x44));
        let reserves = U256::from(7_u64) | (U256::from(11_u64) << 112);
        let fetcher = StorageRoundFetcher::new(Arc::new(move |requests, block| {
            assert_eq!(block, BlockId::from((baseline.block_hash(), Some(true))));
            requests
                .into_iter()
                .map(|(address, slot)| {
                    assert_eq!(slot, V2_RESERVES_SLOT);
                    (address, slot, Ok(reserves))
                })
                .collect()
        }));
        let mut registry = AdapterRegistry::new().with_code_seeding(false);
        registry
            .register_adapter(Arc::new(UniswapV2Adapter::default()))
            .unwrap();
        let pool = PoolRegistration::new(PoolKey::UniswapV2(pool_address)).with_metadata(
            ProtocolMetadata::UniswapV2(UniswapV2Metadata::default().with_fee_bps(30)),
        );

        let prepared = prepare_resumable_pool(
            &registry,
            pool,
            baseline,
            ColdStartPolicy::Lazy,
            Arc::new(EmptyState),
            PreparedPoolFetchers::new(fetcher, None),
            ResumableColdStartConfig::default(),
        )
        .expect("the lazy fallback should prepare a searchable pool");

        let expected = vec![DeferredWork::VerifySlots(vec![
            (pool_address, V2_TOKEN0_SLOT),
            (pool_address, V2_TOKEN1_SLOT),
        ])];
        assert_eq!(prepared.deferred(), expected);
        let (_, _, _, _, deferred, _) = prepared.into_parts();
        assert_eq!(deferred, expected);
    }

    /// Code seeds are not merely copied into a worker artifact: each one must
    /// match an exact-hash account proof before the pool can finish.
    #[test]
    fn resumable_fallback_pairs_verified_code_with_its_exact_hash_proof() {
        let pool_address = Address::repeat_byte(0x62);
        let token0 = Address::repeat_byte(0xb0);
        let token1 = Address::repeat_byte(0xb1);
        let baseline = AmmStatePoint::post_block(1, 101, B256::repeat_byte(0x43));
        let values = HashMap::from([
            (V2_TOKEN0_SLOT, U256::from_be_slice(token0.as_slice())),
            (V2_TOKEN1_SLOT, U256::from_be_slice(token1.as_slice())),
            (V2_RESERVES_SLOT, U256::from(1) | (U256::from(2) << 112)),
        ]);
        let storage_fetcher = StorageRoundFetcher::new(Arc::new(move |requests, block| {
            assert_eq!(block, BlockId::from((baseline.block_hash(), Some(true))));
            requests
                .into_iter()
                .map(|(address, slot)| (address, slot, Ok(values[&slot])))
                .collect()
        }));
        let expected_code_hash = crate::adapters::bytecode::uniswap_v2_pair_runtime_code_hash();
        let account_fetcher = AccountProofRoundFetcher::new(Arc::new(move |requests, block| {
            assert_eq!(block, BlockId::from((baseline.block_hash(), Some(true))));
            requests
                .into_iter()
                .map(|(address, slots)| {
                    assert!(slots.is_empty());
                    (
                        address,
                        Ok(evm_fork_cache::AccountProof {
                            storage_hash: B256::repeat_byte(0x91),
                            balance: U256::ZERO,
                            nonce: 1,
                            code_hash: expected_code_hash,
                            slots: Vec::new(),
                        }),
                    )
                })
                .collect()
        }));
        let mut registry = AdapterRegistry::new();
        registry
            .register_adapter(Arc::new(UniswapV2Adapter::default()))
            .unwrap();
        let pool = PoolRegistration::new(PoolKey::UniswapV2(pool_address))
            .with_metadata(ProtocolMetadata::UniswapV2(UniswapV2Metadata::default()));

        let mut job = PreparedPoolJob::new(
            &registry,
            pool,
            baseline,
            ColdStartPolicy::Eager,
            Arc::new(EmptyState),
            PreparedPoolFetchers::new(storage_fetcher, Some(account_fetcher)),
            ResumableColdStartConfig::default(),
        )
        .expect("the exact-hash proof should verify the adapter seed");
        let prepared = match job.step().expect("the V2 planner is one round") {
            PreparedPoolStep::Done(prepared) => prepared,
            PreparedPoolStep::Continue { .. } => panic!("V2 should finish in one round"),
        };
        let accounts = prepared
            .accounts()
            .expect("verified code patch is attached");
        assert_eq!(accounts.block_hash(), baseline.block_hash());
        assert_eq!(accounts.verified_at_block(), baseline.block_number());
        assert_eq!(accounts.values().len(), 1);
        assert_eq!(accounts.values()[0].address(), pool_address);
        assert_eq!(accounts.values()[0].proof().code_hash, expected_code_hash);
        assert!(!accounts.values()[0].code().is_empty());
    }

    #[test]
    fn prepared_round_rejects_unavailable_phases_explicitly() {
        let address = Address::repeat_byte(0x63);
        let cases = [
            (
                ColdStartPlan {
                    accounts: vec![address],
                    ..Default::default()
                },
                UnsupportedPreparedPhase::Accounts,
            ),
            (
                ColdStartPlan {
                    discover: vec![ColdStartCall::new(address, address, Bytes::new())],
                    ..Default::default()
                },
                UnsupportedPreparedPhase::Discover,
            ),
            (
                ColdStartPlan {
                    probe_roots: vec![address],
                    ..Default::default()
                },
                UnsupportedPreparedPhase::ProbeRoots,
            ),
        ];
        for (plan, expected) in cases {
            assert!(matches!(
                reject_unsupported_phase(&plan),
                Err(PreparedColdStartError::UnsupportedPhase(actual)) if actual == expected
            ));
        }
    }

    struct MapState(HashMap<(Address, U256), U256>);

    impl StateView for MapState {
        fn storage(&self, address: Address, slot: U256) -> Option<U256> {
            self.0.get(&(address, slot)).copied()
        }
    }

    struct TwoRoundAdapter {
        address: Address,
        first: U256,
        second: U256,
        probe: U256,
    }

    impl AmmAdapter for TwoRoundAdapter {
        fn protocol(&self) -> ProtocolId {
            ProtocolId::Custom("prepared-two-round")
        }

        fn cold_start_planner(
            &self,
            _pool: &PoolRegistration,
            policy: ColdStartPolicy,
        ) -> Result<Box<dyn AdapterColdStartPlanner>, UnsupportedReason> {
            Ok(Box::new(TwoRoundPlanner {
                address: self.address,
                first: self.first,
                second: self.second,
                probe: self.probe,
                policy,
                phase: 0,
                equivalent: true,
            }))
        }
    }

    struct TwoRoundPlanner {
        address: Address,
        first: U256,
        second: U256,
        probe: U256,
        policy: ColdStartPolicy,
        phase: u8,
        equivalent: bool,
    }

    impl AdapterColdStartPlanner for TwoRoundPlanner {
        fn initial_plan(&mut self, state: &dyn StateView) -> ColdStartPlan {
            self.equivalent &= state.storage(self.address, U256::ZERO) == Some(U256::from(9));
            ColdStartPlan {
                verify: vec![(self.address, self.first)],
                probe: vec![(self.address, self.probe)],
                ..Default::default()
            }
        }

        fn on_results(
            &mut self,
            results: &ColdStartResults,
            state: &dyn StateView,
        ) -> ColdStartStep {
            self.phase += 1;
            if self.phase == 1 {
                self.equivalent &= matches!(
                    results
                        .probed
                        .iter()
                        .find(|outcome| outcome.slot == self.probe)
                        .map(|outcome| &outcome.fetch),
                    Some(SlotFetch::FetchFailed { .. })
                );
                self.equivalent &= state.storage(self.address, self.first) == Some(U256::from(5));
                return ColdStartStep::Continue(ColdStartPlan {
                    verify: vec![(self.address, self.second)],
                    ..Default::default()
                });
            }
            self.equivalent &= state.storage(self.address, self.first) == Some(U256::from(5));
            self.equivalent &= state.storage(self.address, self.second) == Some(U256::from(7));
            ColdStartStep::Done
        }

        fn finish(
            &mut self,
            pool: &mut PoolRegistration,
            report: &ColdStartRunReport,
        ) -> ColdStartOutcome {
            if !self.equivalent || report.rounds != 2 || report.failed_slots != 1 {
                return ColdStartOutcome::Unsupported(UnsupportedReason::Custom(
                    "worker planner state diverged".to_string(),
                ));
            }
            pool.status = PoolStatus::Ready;
            let mut report = ColdStartReport::new(pool.key.clone(), self.policy);
            report.status = PoolStatus::Ready;
            ColdStartOutcome::Ready(report)
        }
    }

    /// Verify values are layered between rounds while probe failures remain
    /// observational, matching the canonical driver's planner contract.
    #[test]
    fn resumable_rounds_preserve_probe_outcomes_and_overlay_visibility() {
        let address = Address::repeat_byte(0x71);
        let first = U256::from(1);
        let second = U256::from(2);
        let probe = U256::from(3);
        let baseline = AmmStatePoint::post_block(1, 200, B256::repeat_byte(0x52));
        let calls = Arc::new(AtomicUsize::new(0));
        let fetch_calls = Arc::clone(&calls);
        let fetcher = StorageRoundFetcher::new(Arc::new(move |requests, block| {
            assert_eq!(block, BlockId::from((baseline.block_hash(), Some(true))));
            fetch_calls.fetch_add(1, Ordering::SeqCst);
            requests
                .into_iter()
                .map(|(request_address, slot)| {
                    let result = if slot == first {
                        Ok(U256::from(5))
                    } else if slot == second {
                        Ok(U256::from(7))
                    } else {
                        Err(StorageFetchError::custom("archive miss"))
                    };
                    (request_address, slot, result)
                })
                .collect()
        }));
        let mut registry = AdapterRegistry::new().with_code_seeding(false);
        registry
            .register_adapter(Arc::new(TwoRoundAdapter {
                address,
                first,
                second,
                probe,
            }))
            .unwrap();
        let pool = PoolRegistration::new(PoolKey::Custom(CustomPoolKey::Address {
            protocol: "prepared-two-round",
            address,
        }));
        let base = MapState(HashMap::from([((address, U256::ZERO), U256::from(9))]));

        let mut job = PreparedPoolJob::new(
            &registry,
            pool,
            baseline,
            ColdStartPolicy::Eager,
            Arc::new(base),
            PreparedPoolFetchers::new(fetcher, None),
            ResumableColdStartConfig::default(),
        )
        .expect("the resumable job should be accepted");

        let first_step = job.step().expect("first quantum should succeed");
        assert!(matches!(
            first_step,
            PreparedPoolStep::Continue {
                completed_rounds: 1,
                ..
            }
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let prepared = match job.step().expect("second quantum should succeed") {
            PreparedPoolStep::Done(prepared) => prepared,
            PreparedPoolStep::Continue { .. } => panic!("planner should finish after round two"),
        };

        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(prepared.storage().len(), 2);
        assert!(prepared.storage().iter().any(|entry| entry.slot() == first));
        assert!(
            prepared
                .storage()
                .iter()
                .any(|entry| entry.slot() == second)
        );
        assert!(!prepared.storage().iter().any(|entry| entry.slot() == probe));

        let budget_calls = Arc::new(AtomicUsize::new(0));
        let observed_budget_calls = Arc::clone(&budget_calls);
        let bounded_fetcher = StorageRoundFetcher::new(Arc::new(move |requests, _block| {
            observed_budget_calls.fetch_add(1, Ordering::SeqCst);
            requests
                .into_iter()
                .map(|(request_address, slot)| (request_address, slot, Ok(U256::from(5))))
                .collect()
        }));
        let bounded_pool = PoolRegistration::new(PoolKey::Custom(CustomPoolKey::Address {
            protocol: "prepared-two-round",
            address,
        }));
        let mut bounded = PreparedPoolJob::new(
            &registry,
            bounded_pool,
            baseline,
            ColdStartPolicy::Eager,
            Arc::new(MapState(HashMap::from([(
                (address, U256::ZERO),
                U256::from(9),
            )]))),
            PreparedPoolFetchers::new(bounded_fetcher, None),
            ResumableColdStartConfig { max_rounds: 1 },
        )
        .expect("the one-round budget is valid");
        assert!(matches!(
            bounded.step(),
            Ok(PreparedPoolStep::Continue { .. })
        ));
        assert!(matches!(
            bounded.step(),
            Err(PreparedColdStartError::RoundBudgetExceeded { max_rounds: 1 })
        ));
        assert_eq!(budget_calls.load(Ordering::SeqCst), 1);
    }
}
