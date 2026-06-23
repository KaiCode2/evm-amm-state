use alloy_primitives::Log;

use super::cold_start::AdapterColdStartPlanner;
use super::{
    AdapterEvent, AdapterEventResult, AdapterRegistry, ColdStartPolicy, EventSource, PoolKey,
    PoolRegistration, ProtocolId, RepairAction, StateDiff, StateView, UnsupportedReason,
};

/// Protocol adapter contract for AMM-specific routing, cold-start, and decoding.
pub trait AmmAdapter: Send + Sync {
    fn protocol(&self) -> ProtocolId;

    /// Every protocol id this adapter serves.
    ///
    /// Defaults to `[self.protocol()]`. Override to claim a whole storage-layout
    /// family from a single adapter instance (e.g. the V3 family adapter serves
    /// `UniswapV3`, `PancakeV3`, and `Slipstream`). The registry registers the
    /// adapter `Arc` under every returned id; [`Self::protocol`] remains the
    /// primary/canonical id.
    fn protocols(&self) -> Vec<ProtocolId> {
        vec![self.protocol()]
    }

    fn event_sources(&self, pool: &PoolRegistration) -> Vec<EventSource> {
        pool.event_sources.clone()
    }

    fn route_log(&self, log: &Log, registry: &AdapterRegistry) -> Option<PoolKey> {
        registry.route_log_generic(log).map(|pool| pool.key.clone())
    }

    /// Build a cold-start planner for `pool` under `policy`.
    ///
    /// The returned [`AdapterColdStartPlanner`] declares the per-round slot work
    /// (verify/probe) for [`AdapterRegistry::cold_start`] to drive through
    /// [`EvmCache::run_cold_start`](evm_fork_cache::cache::EvmCache::run_cold_start),
    /// then finalizes the pool's metadata/status from the run results. This
    /// replaces the former imperative `cold_start`: the repair decision is now
    /// sourced from the per-slot
    /// [`SlotFetch`](evm_fork_cache::cold_start::SlotFetch) classification rather
    /// than a `cached_storage(..).is_none()` proxy.
    ///
    /// # Metadata contract: merge vs. preserve
    ///
    /// Adapters fall into two camps depending on where a pool's *immutable*
    /// metadata lives:
    ///
    /// - **Merge** (metadata at known storage slots): if an adapter can read its
    ///   immutable identity from predictable slots — e.g. Uniswap V2
    ///   `token0`/`token1` — it MERGES the decoded values into the existing
    ///   config metadata, decoded fields filling in the on-chain truth while
    ///   config-only fields (e.g. `fee_bps`) are preserved untouched.
    /// - **Preserve** (metadata not at predictable slots): if an adapter cannot
    ///   recover its identity from a fixed slot layout — e.g. V3
    ///   `token0`/`token1`/`fee`/`tick_spacing` — it PRESERVES the
    ///   config-supplied metadata unchanged and requires a resolvable storage
    ///   layout (returning [`UnsupportedReason::MissingMetadata`] when none can be
    ///   derived) rather than overwriting config with guesses.
    fn cold_start_planner(
        &self,
        _pool: &PoolRegistration,
        _policy: ColdStartPolicy,
    ) -> Result<Box<dyn AdapterColdStartPlanner>, UnsupportedReason> {
        Err(UnsupportedReason::Protocol(self.protocol()))
    }

    fn decode_event(
        &self,
        _pool: &PoolRegistration,
        _log: &Log,
        _view: &dyn StateView,
    ) -> AdapterEventResult {
        AdapterEventResult::ignored()
    }

    fn after_apply(
        &self,
        _pool: &PoolRegistration,
        _event: &AdapterEvent,
        _diff: &StateDiff,
    ) -> RepairAction {
        RepairAction::None
    }
}
