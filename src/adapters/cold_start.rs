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

use evm_fork_cache::cache::EvmCache;
use evm_fork_cache::cold_start::{
    ColdStartConfig, ColdStartError, ColdStartPlan, ColdStartResults, ColdStartRunReport,
    ColdStartStep,
};

use super::{
    AdapterRegistry, ColdStartOutcome, ColdStartPolicy, PoolRegistration, StateView,
    UnsupportedReason,
};

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
    fn initial_plan(&mut self, state: &dyn StateView) -> ColdStartPlan {
        self.0.initial_plan(state)
    }

    fn on_results(&mut self, results: &ColdStartResults, state: &dyn StateView) -> ColdStartStep {
        self.0.on_results(results, state)
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
            cache.run_cold_start(&mut bridge, ColdStartConfig::default())?
        };

        Ok(planner.finish(pool, &report))
    }
}
