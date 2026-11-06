use alloy_primitives::{Address, Bytes, U256};
use evm_fork_cache::access_set::StorageAccessList as UpstreamStorageAccessList;
use evm_fork_cache::cache::{EvmCache, EvmOverlay, TxConfig};

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

/// Transaction-environment context for a simulated call.
///
/// Every field is optional and defaults to the cache's historical behaviour,
/// so a default context is byte-for-byte the plain
/// [`call_raw`](AdapterCache::call_raw). Protocols whose fees depend on the
/// transaction — the Aero families, whose MEV-tax modules read `tx.gasprice`
/// against `block.basefee` — carry the candidate transaction's context here
/// (see [`SimConfig::tx_context`](super::SimConfig::tx_context)).
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct CallTxContext {
    /// `tx.gasprice` in wei. `None` keeps the cache's default (zero).
    pub gas_price: Option<u128>,
}

impl CallTxContext {
    /// A context with no overrides (the cache default).
    pub const fn new() -> Self {
        Self { gas_price: None }
    }

    /// Set `tx.gasprice` in wei.
    pub const fn with_gas_price(mut self, gas_price: u128) -> Self {
        self.gas_price = Some(gas_price);
        self
    }

    /// Whether this context changes anything relative to the cache default.
    pub const fn is_default(&self) -> bool {
        self.gas_price.is_none()
    }

    fn tx_config(&self) -> TxConfig {
        TxConfig {
            gas_price: self.gas_price,
            ..TxConfig::default()
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
    /// The cached value of `slot` at `address`, or `None` if not warmed.
    fn cached_storage(&self, address: Address, slot: U256) -> Option<U256>;

    /// Apply state updates to the cache, returning the resulting diff (including
    /// any updates skipped because their base slot was cold).
    fn apply_updates(&mut self, updates: &[StateUpdate]) -> StateDiff;

    /// Authoritatively re-fetch the given slots and inject any that changed,
    /// returning the changes.
    fn verify_slots(&mut self, slots: &[(Address, U256)]) -> Result<Vec<SlotChange>, CacheError>;

    /// Invalidate all cached storage for `address`, returning the diff.
    fn purge_storage(&mut self, address: Address) -> StateDiff;

    /// Invalidate the given `slots` of `address`, returning the diff.
    fn purge_slots(&mut self, address: Address, slots: &[U256]) -> StateDiff;

    /// Read one storage slot (from cache, or lazily from the backend).
    fn read_storage_slot(&mut self, address: Address, slot: U256) -> Result<U256, CacheError>;

    /// Read many storage slots, returning one value per input slot in the SAME
    /// order. The default loops [`read_storage_slot`](Self::read_storage_slot);
    /// cache backends that can fetch in bulk should override this to collapse N
    /// reads into one round-trip.
    fn read_storage_slots(&mut self, slots: &[(Address, U256)]) -> Result<Vec<U256>, CacheError> {
        slots
            .iter()
            .map(|(address, slot)| self.read_storage_slot(*address, *slot))
            .collect()
    }

    /// Execute a raw EVM call against the cached state. `commit = false` runs it
    /// read-only (the quote path); `true` persists the resulting state changes.
    fn call_raw(
        &mut self,
        from: Address,
        to: Address,
        calldata: Bytes,
        commit: bool,
    ) -> Result<CallOutcome, CacheError>;

    /// Execute a non-committing call and capture the exact accounts and storage
    /// slots touched by the EVM.
    ///
    /// The default preserves compatibility for custom cache facades by
    /// returning an empty access set. Live [`EvmCache`] overrides this with the
    /// upstream journal-backed tracer; read-set warming relies on that concrete
    /// implementation.
    fn call_raw_with_access_list(
        &mut self,
        from: Address,
        to: Address,
        calldata: Bytes,
    ) -> Result<(CallOutcome, UpstreamStorageAccessList), CacheError> {
        self.call_raw(from, to, calldata, false)
            .map(|outcome| (outcome, UpstreamStorageAccessList::default()))
    }

    /// Execute a raw EVM call with simulation-scoped runtime-code overrides.
    ///
    /// The default delegates to [`call_raw`](Self::call_raw), which is correct
    /// for live-backed caches that can lazily resolve the callee state. Snapshot
    /// adapters should override this when an execution helper needs to
    /// neutralize a state-independent external side effect without mutating the
    /// immutable base. Overrides must not survive the call.
    fn call_raw_with_code_overrides(
        &mut self,
        from: Address,
        to: Address,
        calldata: Bytes,
        code_overrides: &[(Address, Bytes)],
        commit: bool,
    ) -> Result<CallOutcome, CacheError> {
        let _ = code_overrides;
        self.call_raw(from, to, calldata, commit)
    }

    /// Execute a raw EVM call under an explicit transaction context
    /// ([`CallTxContext`]: `tx.gasprice`) with simulation-scoped runtime-code
    /// overrides.
    ///
    /// A default context delegates to
    /// [`call_raw_with_code_overrides`](Self::call_raw_with_code_overrides),
    /// so existing cache facades keep their behaviour. A non-default context
    /// **fails closed** on the default implementation: a quote that must run
    /// under a candidate's gas price (an Aero MEV-tax module reads it) is never
    /// silently executed at the cache's zero default. [`EvmCache`] and the
    /// crate's snapshot caches honour the context; a custom snapshot facade
    /// can delegate to [`overlay_call_raw_with_context`].
    fn call_raw_with_context(
        &mut self,
        from: Address,
        to: Address,
        calldata: Bytes,
        code_overrides: &[(Address, Bytes)],
        context: &CallTxContext,
        commit: bool,
    ) -> Result<CallOutcome, CacheError> {
        if context.is_default() {
            return self.call_raw_with_code_overrides(from, to, calldata, code_overrides, commit);
        }
        Err(CacheError::Backend(Box::new(std::io::Error::other(
            "this cache does not support a transaction context (tx.gasprice) for simulated calls",
        ))))
    }

    /// Traced counterpart to [`Self::call_raw_with_code_overrides`].
    ///
    /// Live-backed caches may execute against real code and ignore the temporary
    /// overrides, matching their existing override behavior. Snapshot-backed
    /// caches can override both code and tracing when needed.
    fn call_raw_with_code_overrides_and_access_list(
        &mut self,
        from: Address,
        to: Address,
        calldata: Bytes,
        code_overrides: &[(Address, Bytes)],
    ) -> Result<(CallOutcome, UpstreamStorageAccessList), CacheError> {
        self.call_raw_with_code_overrides(from, to, calldata, code_overrides, false)
            .map(|outcome| (outcome, UpstreamStorageAccessList::default()))
    }
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

    fn read_storage_slots(&mut self, slots: &[(Address, U256)]) -> Result<Vec<U256>, CacheError> {
        // No batch fetcher (e.g. `from_backend` with no provider): fall back to
        // the per-slot loop rather than failing outright.
        let Some(fetcher) = self.storage_batch_fetcher().cloned() else {
            return slots
                .iter()
                .map(|(address, slot)| {
                    EvmCache::read_storage_slot(self, *address, *slot).map_err(CacheError::from)
                })
                .collect();
        };

        // ONE round-trip for every slot, pinned to the cache's current block.
        // This is a read: results are correlated back to input order without
        // injecting them into the cache.
        let results = fetcher(slots.to_vec(), self.block());

        // The fetcher returns one tuple per requested slot but in an unspecified
        // order, so index by `(address, slot)` to restore input order.
        let mut by_slot: std::collections::HashMap<(Address, U256), U256> =
            std::collections::HashMap::with_capacity(results.len());
        for (address, slot, result) in results {
            let value = result.map_err(|err| CacheError::Backend(Box::new(err)))?;
            by_slot.insert((address, slot), value);
        }

        slots
            .iter()
            .map(|(address, slot)| {
                by_slot.get(&(*address, *slot)).copied().ok_or_else(|| {
                    CacheError::Backend(Box::from(format!(
                        "storage batch fetcher returned no value for slot ({address:?}, {slot})"
                    )))
                })
            })
            .collect()
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

    fn call_raw_with_access_list(
        &mut self,
        from: Address,
        to: Address,
        calldata: Bytes,
    ) -> Result<(CallOutcome, UpstreamStorageAccessList), CacheError> {
        EvmCache::call_raw_with_access_list(self, from, to, calldata)
            .map(|(outcome, accesses)| (CallOutcome::from(outcome), accesses))
            .map_err(CacheError::from)
    }

    /// Live-backed execution honours the transaction context through the
    /// cache's own [`TxConfig`] entry point. Code overrides are ignored, as in
    /// [`call_raw_with_code_overrides`](AdapterCache::call_raw_with_code_overrides):
    /// the real callee code and lazily resolved state execute.
    fn call_raw_with_context(
        &mut self,
        from: Address,
        to: Address,
        calldata: Bytes,
        _code_overrides: &[(Address, Bytes)],
        context: &CallTxContext,
        commit: bool,
    ) -> Result<CallOutcome, CacheError> {
        EvmCache::call_raw_with(self, from, to, calldata, commit, &context.tx_config())
            .map(CallOutcome::from)
            .map_err(CacheError::from)
    }

    fn call_raw_with_code_overrides_and_access_list(
        &mut self,
        from: Address,
        to: Address,
        calldata: Bytes,
        _code_overrides: &[(Address, Bytes)],
    ) -> Result<(CallOutcome, UpstreamStorageAccessList), CacheError> {
        // EvmCache's existing live-backed override behavior intentionally uses
        // the real provider-backed code. Preserve that behavior while tracing.
        EvmCache::call_raw_with_access_list(self, from, to, calldata)
            .map(|(outcome, accesses)| (CallOutcome::from(outcome), accesses))
            .map_err(CacheError::from)
    }
}

/// Execute a non-committing call on an immutable-snapshot [`EvmOverlay`] with
/// temporary runtime-code overrides **and** an explicit transaction context.
///
/// This is the building block a snapshot-backed [`AdapterCache`] uses to
/// implement [`AdapterCache::call_raw_with_context`]. The overlay's own
/// override entry point cannot carry a gas price, so the call is assembled
/// directly on the overlay's EVM: each override is journaled as a code change,
/// the transaction executes under `context`, and the journal checkpoint is
/// reverted afterwards so neither the override nor the call's writes survive.
/// Missing snapshot state is still recorded by the overlay.
pub fn overlay_call_raw_with_context(
    overlay: &mut EvmOverlay,
    from: Address,
    to: Address,
    calldata: Bytes,
    code_overrides: &[(Address, Bytes)],
    context: &CallTxContext,
) -> Result<CallOutcome, CacheError> {
    use revm::ExecuteEvm;
    use revm::context::TxEnv;
    use revm::context_interface::JournalTr;
    use revm::primitives::TxKind;
    use revm::state::Bytecode;

    if context.is_default() {
        return overlay
            .call_raw_with_code_overrides(from, to, calldata, code_overrides)
            .map(CallOutcome::from)
            .map_err(|error| CacheError::Backend(Box::new(error)));
    }
    let mut builder = TxEnv::builder()
        .caller(from)
        .kind(TxKind::Call(to))
        .data(calldata)
        .value(U256::ZERO);
    if let Some(gas_price) = context.gas_price {
        builder = builder.gas_price(gas_price);
    }
    let tx = builder
        .build()
        .map_err(|error| CacheError::Backend(Box::from(format!("tx env: {error:?}"))))?;
    let mut evm = overlay.build_evm();
    let checkpoint = evm.journaled_state.checkpoint();
    let result = (|| {
        for (address, code) in code_overrides {
            evm.journaled_state
                .load_account(*address)
                .map_err(|error| CacheError::Backend(Box::from(format!("{error:?}"))))?;
            evm.journaled_state
                .set_code(*address, Bytecode::new_raw(code.clone()));
        }
        evm.transact_one(tx)
            .map_err(|error| CacheError::Backend(Box::from(format!("{error:?}"))))
    })();
    evm.journaled_state.checkpoint_revert(checkpoint);
    result.map(CallOutcome::from)
}

/// Journal-backed access recorder for inspector-driven calls on the live
/// cache: captures the touched accounts, their runtime-code hashes and every
/// storage key the EVM journal holds when the outermost call frame ends,
/// which mirrors the cache's own access-list extraction.
#[derive(Default)]
pub(crate) struct JournalAccessRecorder {
    access: UpstreamStorageAccessList,
}

impl JournalAccessRecorder {
    pub(crate) fn into_access_list(self) -> UpstreamStorageAccessList {
        self.access
    }
}

impl<'a>
    revm::Inspector<
        revm::context::Context<
            revm::context::BlockEnv,
            revm::context::TxEnv,
            revm::context::CfgEnv,
            &'a mut evm_fork_cache::cache::ForkCacheDB,
            revm::context::Journal<&'a mut evm_fork_cache::cache::ForkCacheDB>,
            (),
        >,
    > for JournalAccessRecorder
{
    fn call_end(
        &mut self,
        context: &mut revm::context::Context<
            revm::context::BlockEnv,
            revm::context::TxEnv,
            revm::context::CfgEnv,
            &'a mut evm_fork_cache::cache::ForkCacheDB,
            revm::context::Journal<&'a mut evm_fork_cache::cache::ForkCacheDB>,
            (),
        >,
        _inputs: &revm::interpreter::CallInputs,
        _outcome: &mut revm::interpreter::CallOutcome,
    ) {
        // The journal is cumulative, so the last `call_end` (the outermost
        // frame) observes every touched account and slot of the transaction.
        let mut access = UpstreamStorageAccessList::default();
        for (address, account) in context.journaled_state.state.iter() {
            if !account.is_touched() {
                continue;
            }
            access.accounts.insert(*address);
            let code_hash = account.info.code_hash;
            if code_hash != alloy_primitives::B256::ZERO
                && code_hash != revm::primitives::KECCAK_EMPTY
            {
                access.code_hashes.insert(code_hash);
            }
            for slot in account.storage.keys() {
                access.slots.insert((*address, *slot));
            }
        }
        self.access = access;
    }
}

impl CallTxContext {
    pub(crate) fn upstream_tx_config(&self) -> TxConfig {
        self.tx_config()
    }
}
