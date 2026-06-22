use alloy_primitives::Log;
use anyhow::Result;

use super::{
    AdapterCache, AdapterEvent, AdapterEventResult, AdapterRegistry, ColdStartOutcome,
    ColdStartPolicy, EventSource, PoolKey, PoolRegistration, ProtocolId, RepairAction, StateDiff,
    StateView, UnsupportedReason,
};

/// Protocol adapter contract for AMM-specific routing, cold-start, and decoding.
pub trait AmmAdapter: Send + Sync {
    fn protocol(&self) -> ProtocolId;

    fn event_sources(&self, pool: &PoolRegistration) -> Vec<EventSource> {
        pool.event_sources.clone()
    }

    fn route_log(&self, log: &Log, registry: &AdapterRegistry) -> Option<PoolKey> {
        registry.route_log_generic(log).map(|pool| pool.key.clone())
    }

    fn cold_start(
        &self,
        _pool: &mut PoolRegistration,
        _cache: &mut dyn AdapterCache,
        _policy: ColdStartPolicy,
    ) -> Result<ColdStartOutcome> {
        Ok(ColdStartOutcome::Unsupported(UnsupportedReason::Protocol(
            self.protocol(),
        )))
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
