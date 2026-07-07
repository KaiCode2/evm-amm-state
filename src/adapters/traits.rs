use alloy_primitives::{Address, Log, U256};

use super::bytecode::{AdapterCodeSeed, BytecodeTemplateError};
use super::cold_start::AdapterColdStartPlanner;
use super::factory::{FactoryConfig, PoolFactory};
use super::sim::{SimConfig, SimError, SwapQuote};
use super::{
    AdapterCache, AdapterEvent, AdapterEventResult, AdapterRegistry, ColdStartPolicy, EventSource,
    PoolKey, PoolRegistration, ProtocolId, RepairAction, StateDiff, StateView, UnsupportedReason,
};

/// Protocol adapter contract for AMM-specific routing, cold-start, and decoding.
pub trait AmmAdapter: Send + Sync {
    /// The adapter's primary/canonical protocol id.
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

    /// The log sources to subscribe/route for `pool`. Defaults to the pool's own
    /// stored `event_sources`; override to derive them from adapter knowledge.
    fn event_sources(&self, pool: &PoolRegistration) -> Vec<EventSource> {
        pool.event_sources.clone()
    }

    /// Route a log to the pool key it belongs to. Defaults to the registry's
    /// generic emitter/topic routing; override for adapter-defined routing.
    fn route_log(&self, log: &Log, registry: &AdapterRegistry) -> Option<PoolKey> {
        registry.route_log_generic(log).map(|pool| pool.key.clone())
    }

    /// Build factory drivers backed by `config`, if this adapter supports
    /// declarative pool discovery. Defaults to none so third-party adapters and
    /// protocols without discovery support are unaffected.
    fn pool_factories(&self, _config: &FactoryConfig) -> Vec<Box<dyn PoolFactory>> {
        Vec::new()
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

    /// Return canonical runtime bytecode seeds that should be written into
    /// `EvmCache` before the cold-start run. The cache verifies these seeds
    /// against on-chain code hashes during its `verify_code` phase.
    ///
    /// A pool that is simply not seedable (wrong pool-key shape, missing or
    /// incomplete metadata, wrong protocol) returns `Ok(vec![])`. Only a
    /// genuine template render failure returns `Err(BytecodeTemplateError)`;
    /// the facade treats that as a safe skip (no seeding for that pool).
    fn code_seeds(
        &self,
        _pool: &PoolRegistration,
    ) -> Result<Vec<AdapterCodeSeed>, BytecodeTemplateError> {
        Ok(Vec::new())
    }

    /// Decode a routed log into a semantic event with its cache updates.
    /// Defaults to [`AdapterEventResult::ignored`]; a malformed watched log
    /// should return [`AdapterEventResult::error`] (it is isolated per-log, not
    /// batch-fatal), never panic.
    fn decode_event(
        &self,
        _pool: &PoolRegistration,
        _log: &Log,
        _view: &dyn StateView,
    ) -> AdapterEventResult {
        AdapterEventResult::ignored()
    }

    /// Follow-up repair after `event`'s updates were applied, given the resulting
    /// `diff` (e.g. re-verify slots that were skipped because they were cold).
    /// Defaults to [`RepairAction::None`].
    fn after_apply(
        &self,
        _pool: &PoolRegistration,
        _event: &AdapterEvent,
        _diff: &StateDiff,
    ) -> RepairAction {
        RepairAction::None
    }

    /// Simulate `amount_in` of `token_in` swapped to `token_out` for `pool`,
    /// returning the protocol's canonical `amount_out`.
    ///
    /// The implementation builds the protocol's canonical *quote* calldata and
    /// runs it via [`AdapterCache::call_raw`] with `from = ZERO`,
    /// `to = <quote target>`, `commit = false` against the cold-start snapshot,
    /// then decodes `amount_out` from the [`ExecutionResult`] output. The
    /// deployed contract bytecode does the AMM math — there is no `amm-math` /
    /// `LocalAMM` / hand-rolled math here. A revert/halt maps to
    /// [`SimError::Reverted`].
    ///
    /// Quote targets are resolved from `config` (Uniswap V3 `QuoterV2`, Uniswap
    /// V2 `Router02`) with the Balancer vault taken from the pool's metadata.
    ///
    /// [`ExecutionResult`]: revm::context::result::ExecutionResult
    ///
    /// Defaults to [`SimError::Unsupported`] for protocols without a quote impl.
    fn simulate_swap(
        &self,
        _pool: &PoolRegistration,
        _cache: &mut dyn AdapterCache,
        _token_in: Address,
        _token_out: Address,
        _amount_in: U256,
        _config: &SimConfig,
    ) -> Result<SwapQuote, SimError> {
        Err(SimError::Unsupported(self.protocol()))
    }
}
