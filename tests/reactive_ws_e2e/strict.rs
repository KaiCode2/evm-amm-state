//! Provider-disconnected quote cache, matching the existing campaign facade.
use alloy_primitives::{Address, Bytes, U256};
use evm_amm_state::adapters::{
    AdapterCache, CacheError, CallOutcome, CallTxContext, SlotChange, StateDiff, StateUpdate,
    StateView, overlay_call_raw_with_context,
};
use evm_fork_cache::cache::{EvmOverlay, EvmSnapshot};
use std::sync::Arc;

pub(super) struct StrictCache {
    snapshot: Arc<EvmSnapshot>,
    pub(super) overlay: EvmOverlay,
}

impl StrictCache {
    pub(super) fn new(snapshot: Arc<EvmSnapshot>) -> Self {
        Self {
            overlay: EvmOverlay::new(Arc::clone(&snapshot), None),
            snapshot,
        }
    }
}

impl StateView for StrictCache {
    fn storage(&self, address: Address, slot: U256) -> Option<U256> {
        self.snapshot.storage_value(address, slot)
    }
}

fn strict_error(what: &str) -> CacheError {
    CacheError::Backend(Box::new(std::io::Error::other(format!(
        "{what} is unavailable on the strict V2 soak cache"
    ))))
}

impl AdapterCache for StrictCache {
    fn cached_storage(&self, address: Address, slot: U256) -> Option<U256> {
        self.snapshot.storage_value(address, slot)
    }

    fn apply_updates(&mut self, _updates: &[StateUpdate]) -> StateDiff {
        StateDiff::default()
    }

    fn verify_slots(&mut self, _slots: &[(Address, U256)]) -> Result<Vec<SlotChange>, CacheError> {
        Err(strict_error("verify_slots"))
    }

    fn purge_storage(&mut self, _address: Address) -> StateDiff {
        StateDiff::default()
    }

    fn purge_slots(&mut self, _address: Address, _slots: &[U256]) -> StateDiff {
        StateDiff::default()
    }

    fn read_storage_slot(&mut self, address: Address, slot: U256) -> Result<U256, CacheError> {
        self.snapshot
            .storage_value(address, slot)
            .ok_or_else(|| strict_error("lazy storage read"))
    }

    fn call_raw(
        &mut self,
        from: Address,
        to: Address,
        calldata: Bytes,
        _commit: bool,
    ) -> Result<CallOutcome, CacheError> {
        self.overlay
            .call_raw(from, to, calldata)
            .map(CallOutcome::from)
            .map_err(|error| CacheError::Backend(Box::new(error)))
    }

    fn call_raw_with_code_overrides(
        &mut self,
        from: Address,
        to: Address,
        calldata: Bytes,
        code_overrides: &[(Address, Bytes)],
        _commit: bool,
    ) -> Result<CallOutcome, CacheError> {
        self.overlay
            .call_raw_with_code_overrides(from, to, calldata, code_overrides)
            .map(CallOutcome::from)
            .map_err(|error| CacheError::Backend(Box::new(error)))
    }

    fn call_raw_with_context(
        &mut self,
        from: Address,
        to: Address,
        calldata: Bytes,
        code_overrides: &[(Address, Bytes)],
        context: &CallTxContext,
        _commit: bool,
    ) -> Result<CallOutcome, CacheError> {
        overlay_call_raw_with_context(
            &mut self.overlay,
            from,
            to,
            calldata,
            code_overrides,
            context,
        )
    }
}
