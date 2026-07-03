use alloy_primitives::{Address, Bytes, U256};
use evm_fork_cache::cache::EvmCache;

pub use super::state::{
    PurgeScope, SkippedDelta, SkippedMask, SlotChange, SlotDelta, StateDiff, StateUpdate,
    StateView, UpstreamStateView,
};

/// Outcome of a raw EVM call executed via [`AdapterCache::call_raw`].
///
/// Crate-owned mirror of the underlying execution result, so the public surface
/// does not leak `revm`'s `ExecutionResult`.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CallOutcome {
    /// Successful execution with `output` return data.
    Success {
        /// The call's return data.
        output: Bytes,
        /// Gas consumed by the call.
        gas_used: u64,
    },
    /// The call reverted with `output` revert data.
    Revert {
        /// The revert return data.
        output: Bytes,
        /// Gas consumed before the revert.
        gas_used: u64,
    },
    /// The call halted (out-of-gas, invalid opcode, etc.).
    Halt {
        /// A human-readable description of the halt reason.
        reason: String,
    },
}

impl CallOutcome {
    /// The success return data, or `None` if the call reverted or halted.
    pub fn into_success_output(self) -> Option<Bytes> {
        match self {
            Self::Success { output, .. } => Some(output),
            Self::Revert { .. } | Self::Halt { .. } => None,
        }
    }

    /// The success return data by reference, or `None` if the call reverted or
    /// halted.
    pub fn output(&self) -> Option<&Bytes> {
        match self {
            Self::Success { output, .. } => Some(output),
            Self::Revert { .. } | Self::Halt { .. } => None,
        }
    }

    /// Whether the call succeeded.
    pub fn is_success(&self) -> bool {
        matches!(self, Self::Success { .. })
    }
}

impl From<revm::context::result::ExecutionResult> for CallOutcome {
    fn from(result: revm::context::result::ExecutionResult) -> Self {
        use revm::context::result::ExecutionResult;
        match result {
            ExecutionResult::Success {
                output, gas_used, ..
            } => Self::Success {
                output: output.into_data(),
                gas_used,
            },
            ExecutionResult::Revert { gas_used, output } => Self::Revert { output, gas_used },
            ExecutionResult::Halt { reason, .. } => Self::Halt {
                reason: format!("{reason:?}"),
            },
        }
    }
}

/// Error from a fallible [`AdapterCache`] operation.
///
/// Crate-owned mirror of the underlying host/backend failure, so the public
/// surface does not leak the upstream error type.
#[non_exhaustive]
#[derive(Debug)]
pub enum CacheError {
    /// A host / backend / execution error from the underlying cache, carrying
    /// the un-flattened cause. Downcast the payload (or walk
    /// [`source`](std::error::Error::source)) — e.g. to
    /// [`evm_fork_cache::CacheError`] — for typed handling.
    Backend(Box<dyn std::error::Error + Send + Sync + 'static>),
}

impl std::fmt::Display for CacheError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Backend(err) => write!(f, "cache backend error: {err}"),
        }
    }
}

impl std::error::Error for CacheError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Backend(err) => Some(&**err as &(dyn std::error::Error + 'static)),
        }
    }
}

impl From<evm_fork_cache::CacheError> for CacheError {
    fn from(err: evm_fork_cache::CacheError) -> Self {
        Self::Backend(Box::new(err))
    }
}

/// Cache facade used by protocol adapters.
pub trait AdapterCache: StateView {
    fn cached_storage(&self, address: Address, slot: U256) -> Option<U256>;

    fn apply_updates(&mut self, updates: &[StateUpdate]) -> StateDiff;

    fn verify_slots(&mut self, slots: &[(Address, U256)]) -> Result<Vec<SlotChange>, CacheError>;

    fn purge_storage(&mut self, address: Address) -> StateDiff;

    fn purge_slots(&mut self, address: Address, slots: &[U256]) -> StateDiff;

    fn read_storage_slot(&mut self, address: Address, slot: U256) -> Result<U256, CacheError>;

    fn call_raw(
        &mut self,
        from: Address,
        to: Address,
        calldata: Bytes,
        commit: bool,
    ) -> Result<CallOutcome, CacheError>;
}

/// Read-only crate-owned [`StateView`] over the cache, delegating to the
/// upstream inherent `storage`.
impl StateView for EvmCache {
    fn storage(&self, address: Address, slot: U256) -> Option<U256> {
        evm_fork_cache::StateView::storage(self, address, slot)
    }
}

impl AdapterCache for EvmCache {
    fn cached_storage(&self, address: Address, slot: U256) -> Option<U256> {
        EvmCache::cached_storage_value(self, address, slot)
    }

    fn apply_updates(&mut self, updates: &[StateUpdate]) -> StateDiff {
        let upstream: Vec<evm_fork_cache::StateUpdate> =
            updates.iter().cloned().map(Into::into).collect();
        EvmCache::apply_updates(self, &upstream).into()
    }

    fn verify_slots(&mut self, slots: &[(Address, U256)]) -> Result<Vec<SlotChange>, CacheError> {
        EvmCache::verify_slots(self, slots)
            .map(|changes| changes.into_iter().map(SlotChange::from).collect())
            .map_err(CacheError::from)
    }

    fn purge_storage(&mut self, address: Address) -> StateDiff {
        AdapterCache::apply_updates(self, &[StateUpdate::purge(address, PurgeScope::AllStorage)])
    }

    fn purge_slots(&mut self, address: Address, slots: &[U256]) -> StateDiff {
        AdapterCache::apply_updates(
            self,
            &[StateUpdate::purge(
                address,
                PurgeScope::Slots(slots.to_vec()),
            )],
        )
    }

    fn read_storage_slot(&mut self, address: Address, slot: U256) -> Result<U256, CacheError> {
        EvmCache::read_storage_slot(self, address, slot).map_err(CacheError::from)
    }

    fn call_raw(
        &mut self,
        from: Address,
        to: Address,
        calldata: Bytes,
        commit: bool,
    ) -> Result<CallOutcome, CacheError> {
        EvmCache::call_raw(self, from, to, calldata, commit)
            .map(CallOutcome::from)
            .map_err(CacheError::from)
    }
}
