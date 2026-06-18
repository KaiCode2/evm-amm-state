use alloy_primitives::{Address, Bytes, U256};
use anyhow::Result;
use evm_fork_cache::cache::EvmCache;
pub use evm_fork_cache::{PurgeScope, SlotChange, StateDiff, StateUpdate, StateView};
use revm::context::result::ExecutionResult;

/// Cache facade used by protocol adapters.
pub trait AdapterCache {
    fn cached_storage(&self, address: Address, slot: U256) -> Option<U256>;

    fn apply_updates(&mut self, updates: &[StateUpdate]) -> StateDiff;

    fn verify_slots(&mut self, slots: &[(Address, U256)]) -> Result<Vec<SlotChange>>;

    fn purge_storage(&mut self, address: Address) -> StateDiff;

    fn purge_slots(&mut self, address: Address, slots: &[U256]) -> StateDiff;

    fn read_storage_slot(&mut self, address: Address, slot: U256) -> Result<U256>;

    fn call_raw(
        &mut self,
        from: Address,
        to: Address,
        calldata: Bytes,
        commit: bool,
    ) -> Result<ExecutionResult>;
}

impl AdapterCache for EvmCache {
    fn cached_storage(&self, address: Address, slot: U256) -> Option<U256> {
        EvmCache::cached_storage_value(self, address, slot)
    }

    fn apply_updates(&mut self, updates: &[StateUpdate]) -> StateDiff {
        EvmCache::apply_updates(self, updates)
    }

    fn verify_slots(&mut self, slots: &[(Address, U256)]) -> Result<Vec<SlotChange>> {
        EvmCache::verify_slots(self, slots)
    }

    fn purge_storage(&mut self, address: Address) -> StateDiff {
        self.apply_updates(&[StateUpdate::purge(address, PurgeScope::AllStorage)])
    }

    fn purge_slots(&mut self, address: Address, slots: &[U256]) -> StateDiff {
        self.apply_updates(&[StateUpdate::purge(
            address,
            PurgeScope::Slots(slots.to_vec()),
        )])
    }

    fn read_storage_slot(&mut self, address: Address, slot: U256) -> Result<U256> {
        EvmCache::read_storage_slot(self, address, slot)
    }

    fn call_raw(
        &mut self,
        from: Address,
        to: Address,
        calldata: Bytes,
        commit: bool,
    ) -> Result<ExecutionResult> {
        EvmCache::call_raw(self, from, to, calldata, commit)
    }
}
