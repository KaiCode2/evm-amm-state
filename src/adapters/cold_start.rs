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

use alloy_primitives::{Address, B256, Bytes, U256};
use evm_fork_cache::CacheError as UpstreamCacheError;
use evm_fork_cache::bulk_storage::{StorageProgram, run_storage_programs};
use evm_fork_cache::cache::{CodeSeedState, EvmCache};
use evm_fork_cache::cold_start::ColdStartConfig;

use super::bytecode::AdapterCodeSeed;
use super::state::UpstreamStateView;
use super::storage_sync::{StorageSyncSpec, decode_storage_sync, storage_sync_spec_for_pool};
use super::{
    AdapterRegistry, CallOutcome, ColdStartOutcome, ColdStartPolicy, ColdStartReport,
    PoolRegistration, PoolStatus, SlotChange, StateView, UnsupportedReason,
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CodeSeedMismatch {
    /// The seeded address.
    pub address: Address,
    /// The hash the seed claimed (keccak256 of the seeded bytes).
    pub expected: B256,
    /// The on-chain `EXTCODEHASH` observed at the pinned block.
    pub actual: B256,
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

    /// Decode `output` and inject the hydrated state into `cache`, returning how
    /// many slots were written. A decode failure is surfaced (un-flattened) so
    /// the caller can fall the pool back to the multi-round cold-start.
    fn apply(&self, cache: &mut EvmCache, output: &Bytes) -> Result<usize, HydrationError> {
        match self {
            #[cfg(feature = "uniswap-v3")]
            HydrationKind::V3 { pool, spec } => {
                let snapshot = decode_full_sync(spec, output).map_err(HydrationError::V3)?;
                Ok(snapshot.inject(cache, *pool, spec))
            }
            HydrationKind::Flat { spec } => {
                let snapshot = decode_storage_sync(spec, output).map_err(HydrationError::Flat)?;
                Ok(snapshot.inject(cache))
            }
        }
    }
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
    let (metadata, canonical_uniswap) = match &pool.metadata {
        ProtocolMetadata::UniswapV3(metadata) => (metadata, true),
        ProtocolMetadata::PancakeV3(metadata) | ProtocolMetadata::Slipstream(metadata) => {
            (metadata, false)
        }
        _ => return None,
    };
    let layout = metadata.storage_layout.filter(|l| l.tick_spacing > 0)?;
    Some(if canonical_uniswap {
        V3SyncSpec::uniswap(layout)
    } else {
        V3SyncSpec::core(layout)
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
    /// Decoding a V3 full-sync program's output failed.
    #[cfg(feature = "uniswap-v3")]
    V3(V3SyncError),
    /// Decoding a flat-slot sync program's output failed.
    Flat(super::storage_sync::StorageSyncError),
}

impl std::fmt::Display for HydrationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            #[cfg(feature = "uniswap-v3")]
            Self::V3(_) => write!(f, "one-shot V3 hydration failed"),
            Self::Flat(_) => write!(f, "one-shot flat-slot hydration failed"),
        }
    }
}

impl std::error::Error for HydrationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            #[cfg(feature = "uniswap-v3")]
            Self::V3(err) => Some(err as &(dyn std::error::Error + 'static)),
            Self::Flat(err) => Some(err as &(dyn std::error::Error + 'static)),
        }
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
    ///
    /// Returns one [`ColdStartOutcome`] per input pool, in **input order**. An
    /// empty slice returns `Ok(vec![])` without touching `provider`.
    ///
    /// Fast hydration needs a live provider; over an empty/offline provider
    /// every `eth_call` errors and every fast pool gracefully falls back — never
    /// a panic. An upstream cold-start error from the fallback path (e.g. a
    /// missing batch fetcher) still propagates as `Err`.
    ///
    /// [`code_seeds`]: super::AmmAdapter::code_seeds
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
                    Ok(Ok(_slots)) => {
                        let pool = &mut pools[*index];
                        pool.status = PoolStatus::Ready;
                        let mut report = ColdStartReport::new(pool.key.clone(), policy);
                        report.status = PoolStatus::Ready;
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

        // Step 5: every slot is now populated (fast success or fallback).
        Ok(outcomes
            .into_iter()
            .map(|outcome| outcome.expect("every pool is fast-hydrated or fell back"))
            .collect())
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
    use crate::adapters::storage::V3StorageLayout;
    use crate::adapters::types::{PoolKey, PoolRegistration, ProtocolMetadata, V3Metadata};
    use crate::adapters::v3_sync::V3SyncSpec;
    use alloy_primitives::Address;

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

    /// PancakeSwap V3 and Slipstream use the layout-only `core` spec (slot0 +
    /// liquidity + ticks) — their extra static/observation slots are not verified,
    /// so hydration must not inject Uniswap's positions for them.
    #[test]
    fn pancake_and_slipstream_use_the_core_spec_until_verified() {
        let pancake_layout = V3StorageLayout::pancake(10);
        let pancake = PoolRegistration::new(PoolKey::PancakeV3(Address::repeat_byte(0x22)))
            .with_metadata(ProtocolMetadata::PancakeV3(
                V3Metadata::default()
                    .with_fee(2500)
                    .with_storage_layout(pancake_layout),
            ));
        assert_eq!(
            v3_sync_spec(&pancake),
            Some(V3SyncSpec::core(pancake_layout))
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
}
