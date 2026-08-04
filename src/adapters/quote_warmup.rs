//! Representative quote read-set warming and offline readiness validation.

use std::collections::HashSet;
use std::fmt;
use std::sync::Arc;

use alloy_primitives::{Address, B256, Bytes, U256};
use evm_fork_cache::ReadSetHydrationFailure;
use evm_fork_cache::access_set::StorageAccessList as UpstreamStorageAccessList;
use evm_fork_cache::cache::{EvmCache, EvmOverlay, EvmSnapshot, MissingState};
use revm::database_interface::Database;

use super::{
    AdapterCache, AdapterRegistry, CacheError, CallOutcome, PoolKey, ProtocolId, SimError,
    SlotChange, StateDiff, StateUpdate, StateView, StorageAccessList, SwapQuote,
};

/// One representative exact-input quote whose read set must be resident before
/// speculative AMM snapshots are considered ready.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct QuoteWarmup {
    /// Pool whose adapter executes the quote.
    pub pool: PoolKey,
    /// Input token.
    pub token_in: Address,
    /// Output token.
    pub token_out: Address,
    /// Representative exact-input amount.
    pub amount_in: U256,
}

impl QuoteWarmup {
    /// Construct a representative exact-input quote.
    pub const fn exact_input(
        pool: PoolKey,
        token_in: Address,
        token_out: Address,
        amount_in: U256,
    ) -> Self {
        Self {
            pool,
            token_in,
            token_out,
            amount_in,
        }
    }
}

/// One successfully warmed representative quote.
#[derive(Clone, Debug)]
pub struct QuoteWarmupEntry {
    request: QuoteWarmup,
    quote: SwapQuote,
    accesses: StorageAccessList,
    provider_reads: StorageAccessList,
}

impl QuoteWarmupEntry {
    /// Warmup request that was executed.
    pub const fn request(&self) -> &QuoteWarmup {
        &self.request
    }

    /// Quote returned by the canonical warmup execution.
    pub const fn quote(&self) -> &SwapQuote {
        &self.quote
    }

    /// Accounts and slots touched by the quote.
    pub const fn accesses(&self) -> &StorageAccessList {
        &self.accesses
    }

    /// Dependencies that were absent before this warmup and therefore required
    /// lazy provider hydration during its canonical execution.
    pub const fn provider_reads(&self) -> &StorageAccessList {
        &self.provider_reads
    }
}

/// Result of warming a batch of representative quotes.
#[derive(Clone, Debug, Default)]
pub struct QuoteWarmupReport {
    entries: Vec<QuoteWarmupEntry>,
}

impl QuoteWarmupReport {
    /// Successfully warmed entries in request order.
    pub fn entries(&self) -> &[QuoteWarmupEntry] {
        &self.entries
    }
}

/// Per-representative-quote bounds for learned dependency growth.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QuoteReadSetLimits {
    /// Maximum distinct accounts.
    pub max_accounts: usize,
    /// Maximum distinct runtime-code hashes.
    pub max_code_hashes: usize,
    /// Maximum distinct storage slots.
    pub max_slots: usize,
    /// Maximum historical block hashes.
    pub max_block_hashes: usize,
}

impl Default for QuoteReadSetLimits {
    fn default() -> Self {
        Self {
            max_accounts: 4_096,
            max_code_hashes: 4_096,
            max_slots: 131_072,
            max_block_hashes: 256,
        }
    }
}

impl QuoteReadSetLimits {
    fn accepts(self, access: &UpstreamStorageAccessList) -> bool {
        access.accounts.len() <= self.max_accounts
            && access.code_hashes.len() <= self.max_code_hashes
            && access.slots.len() <= self.max_slots
            && access.block_numbers.len() <= self.max_block_hashes
    }
}

/// Result of replaying learned warmups against one RPC-disconnected snapshot.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct QuoteReadinessReport {
    checked: usize,
}

/// Exact-canonical hydration result for every learned representative quote.
#[derive(Clone, Debug, Default)]
pub struct QuoteReadSetHydrationReport {
    /// Number of representative quote manifests covered by the refresh.
    pub warmups: usize,
    /// Account headers refreshed at the cache's exact block pin.
    pub accounts_refreshed: usize,
    /// Storage slots refreshed at the same pin.
    pub slots_refreshed: usize,
    /// Typed proof, provider, code-residency, or proof-shape failures.
    pub failures: Vec<ReadSetHydrationFailure>,
    /// Code identity changes that invalidated learned manifests.
    pub code_changes: Vec<(Address, B256, B256)>,
    /// Manifests invalidated by a code/layout change.
    pub invalidated: Vec<QuoteWarmup>,
    /// Dependencies still absent after hydration.
    pub missing_after: StorageAccessList,
}

impl QuoteReadSetHydrationReport {
    /// Whether every manifest remains valid and fully resident.
    pub fn is_complete(&self) -> bool {
        self.failures.is_empty()
            && self.code_changes.is_empty()
            && self.invalidated.is_empty()
            && self.missing_after.accounts.is_empty()
            && self.missing_after.code_hashes.is_empty()
            && self.missing_after.slots.is_empty()
            && self.missing_after.block_numbers.is_empty()
    }
}

impl QuoteReadinessReport {
    /// Number of representative quotes replayed completely offline.
    pub const fn checked(&self) -> usize {
        self.checked
    }
}

/// A representative quote could not be learned or replayed authoritatively.
#[derive(Debug)]
#[non_exhaustive]
pub enum QuoteWarmupError {
    /// The requested pool is not registered.
    UnknownPool(PoolKey),
    /// The requested pool has no active adapter.
    MissingAdapter(ProtocolId),
    /// An affected pool has no learned representative quote manifest while
    /// fail-closed quote readiness is enabled.
    MissingReadSet(PoolKey),
    /// Adapter simulation failed.
    Simulation {
        /// Request that failed.
        request: Box<QuoteWarmup>,
        /// Stable diagnostic string from the adapter error.
        message: String,
    },
    /// The quote ran against an RPC-disconnected snapshot but touched state the
    /// snapshot did not contain.
    IncompleteSnapshot {
        /// Request whose offline replay was incomplete.
        request: Box<QuoteWarmup>,
        /// Exact unresolved account/code/storage/block-hash reads.
        missing: Box<MissingState>,
    },
    /// Immediate RPC-disconnected replay over the warmed canonical snapshot
    /// produced a different quote.
    ReplayMismatch {
        /// Request whose replay diverged.
        request: Box<QuoteWarmup>,
        /// Quote returned by the canonical warmup execution.
        canonical: SwapQuote,
        /// Quote returned by immediate offline replay of the same state.
        offline: SwapQuote,
    },
    /// A learned or newly observed dependency set exceeded its configured
    /// memory/cost bound.
    ReadSetLimit {
        /// Request whose manifest exceeded a limit.
        request: Box<QuoteWarmup>,
        /// Observed account count.
        accounts: usize,
        /// Observed runtime-code count.
        code_hashes: usize,
        /// Observed slot count.
        slots: usize,
        /// Observed historical block-hash count.
        block_hashes: usize,
    },
}

impl fmt::Display for QuoteWarmupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownPool(pool) => write!(f, "quote warmup pool is not registered: {pool:?}"),
            Self::MissingAdapter(protocol) => {
                write!(f, "quote warmup adapter is not registered: {protocol:?}")
            }
            Self::MissingReadSet(pool) => {
                write!(f, "affected pool has no learned quote read set: {pool:?}")
            }
            Self::Simulation { request, message } => {
                write!(f, "quote warmup failed for {:?}: {message}", request.pool)
            }
            Self::IncompleteSnapshot { request, missing } => write!(
                f,
                "quote warmup snapshot is incomplete for {:?}: {} accounts, {} code hashes, {} slots, {} block hashes",
                request.pool,
                missing.accounts.len(),
                missing.code_hashes.len(),
                missing.storage.len(),
                missing.block_hashes.len()
            ),
            Self::ReplayMismatch {
                request,
                canonical,
                offline,
            } => write!(
                f,
                "offline quote replay diverged for {:?}: canonical amount_out {}, offline amount_out {}",
                request.pool, canonical.amount_out, offline.amount_out
            ),
            Self::ReadSetLimit {
                request,
                accounts,
                code_hashes,
                slots,
                block_hashes,
            } => write!(
                f,
                "quote read set exceeded its configured bound for {:?}: {accounts} accounts, {code_hashes} code hashes, {slots} slots, {block_hashes} block hashes",
                request.pool
            ),
        }
    }
}

impl std::error::Error for QuoteWarmupError {}

impl AdapterRegistry {
    /// Execute representative quotes against the live canonical cache, capture
    /// their exact account/storage read sets, and prove each warmed result can be
    /// replayed against an RPC-disconnected immutable snapshot.
    ///
    /// Registry mutation is atomic: learned manifests are installed only after
    /// every request passes offline replay. Canonical cache fills performed by a
    /// successful or later-failing warmup remain useful and are not rolled back.
    pub fn warm_quote_read_sets(
        &mut self,
        cache: &mut EvmCache,
        requests: impl IntoIterator<Item = QuoteWarmup>,
    ) -> Result<QuoteWarmupReport, QuoteWarmupError> {
        let requests: Vec<_> = requests.into_iter().collect();
        let mut learned = Vec::with_capacity(requests.len());
        let mut entries = Vec::with_capacity(requests.len());

        for request in requests {
            let pool = self
                .pool(&request.pool)
                .cloned()
                .ok_or_else(|| QuoteWarmupError::UnknownPool(request.pool.clone()))?;
            let adapter = self
                .adapter(pool.protocol())
                .cloned()
                .ok_or(QuoteWarmupError::MissingAdapter(pool.protocol()))?;
            let mut recording = RecordingCache::new(cache);
            let quote = adapter
                .simulate_swap(
                    &pool,
                    &mut recording,
                    request.token_in,
                    request.token_out,
                    request.amount_in,
                    &self.sim_config,
                )
                .map_err(|error| simulation_error(&request, error))?;
            let (accesses, provider_reads) = recording.into_parts();
            ensure_within_limits(self.quote_read_set_limits, &request, &accesses)?;

            let offline = validate_one(
                &pool,
                adapter.as_ref(),
                &self.sim_config,
                cache.snapshot(),
                &request,
            )?;
            if offline != quote {
                return Err(QuoteWarmupError::ReplayMismatch {
                    request: Box::new(request),
                    canonical: quote,
                    offline,
                });
            }

            entries.push(QuoteWarmupEntry {
                request: request.clone(),
                quote,
                accesses: accesses.clone().into(),
                provider_reads: provider_reads.into(),
            });
            learned.push((request, accesses));
        }

        for (request, accesses) in learned {
            self.quote_read_sets.insert(request, accesses);
        }
        if !entries.is_empty() {
            self.quote_readiness_required = true;
        }
        Ok(QuoteWarmupReport { entries })
    }

    /// The learned read set for one representative quote.
    pub fn quote_read_set(&self, request: &QuoteWarmup) -> Option<StorageAccessList> {
        self.quote_read_sets.get(request).cloned().map(Into::into)
    }

    /// Whether at least one representative quote is registered for `pool`.
    pub fn has_quote_read_set(&self, pool: &PoolKey) -> bool {
        self.quote_read_sets
            .keys()
            .any(|warmup| &warmup.pool == pool)
    }

    /// Union every representative quote dependency learned for `pool`.
    pub fn quote_read_set_for_pool(&self, pool: &PoolKey) -> StorageAccessList {
        self.quote_read_set_upstream_for_pool(pool).into()
    }

    /// Dependencies for `pool` that are absent from one immutable snapshot.
    pub fn missing_quote_read_set(
        &self,
        snapshot: &EvmSnapshot,
        pool: &PoolKey,
    ) -> StorageAccessList {
        snapshot
            .missing_read_set(&self.quote_read_set_upstream_for_pool(pool))
            .into()
    }

    pub(crate) fn quote_read_set_upstream_for_pool(
        &self,
        pool: &PoolKey,
    ) -> UpstreamStorageAccessList {
        let mut combined = UpstreamStorageAccessList::default();
        for (warmup, accesses) in &self.quote_read_sets {
            if &warmup.pool == pool {
                combined.extend(accesses);
            }
        }
        combined
    }

    /// Refresh every learned quote dependency at the cache's exact canonical
    /// block pin. A changed runtime-code hash invalidates every manifest that
    /// depended on the old code identity.
    pub fn hydrate_quote_read_sets(&mut self, cache: &mut EvmCache) -> QuoteReadSetHydrationReport {
        let mut required = UpstreamStorageAccessList::default();
        for accesses in self.quote_read_sets.values() {
            required.extend(accesses);
        }
        let warmups = self.quote_read_sets.len();
        let hydrated = cache.hydrate_read_set(&required);
        let invalid_hashes: HashSet<_> = hydrated
            .code_changes
            .iter()
            .map(|(_, old, _)| *old)
            .collect();
        let mut invalidated = Vec::new();
        if !invalid_hashes.is_empty() {
            self.quote_read_sets.retain(|warmup, accesses| {
                let retain = accesses.code_hashes.is_disjoint(&invalid_hashes);
                if !retain {
                    invalidated.push(warmup.clone());
                }
                retain
            });
        }
        QuoteReadSetHydrationReport {
            warmups,
            accounts_refreshed: hydrated.accounts_refreshed,
            slots_refreshed: hydrated.slots_refreshed,
            failures: hydrated.failures,
            code_changes: hydrated.code_changes,
            invalidated,
            missing_after: hydrated.missing_after.into(),
        }
    }

    /// Replay all learned representative quotes for `pools` against one
    /// RPC-disconnected snapshot. Any missing account, code, storage, block hash,
    /// or adapter error fails the entire readiness check.
    pub fn validate_quote_read_sets<'a>(
        &mut self,
        snapshot: Arc<EvmSnapshot>,
        pools: impl IntoIterator<Item = &'a PoolKey>,
    ) -> Result<QuoteReadinessReport, QuoteWarmupError> {
        let pools: HashSet<_> = pools.into_iter().cloned().collect();
        if self.quote_readiness_required
            && let Some(pool) = pools.iter().find(|pool| !self.has_quote_read_set(pool))
        {
            return Err(QuoteWarmupError::MissingReadSet(pool.clone()));
        }
        let mut requests: Vec<_> = self
            .quote_read_sets
            .keys()
            .filter(|warmup| pools.contains(&warmup.pool))
            .cloned()
            .collect();
        requests.sort_by(|left, right| {
            (&left.pool, left.token_in, left.token_out, left.amount_in).cmp(&(
                &right.pool,
                right.token_in,
                right.token_out,
                right.amount_in,
            ))
        });

        for request in &requests {
            let pool = self
                .pool(&request.pool)
                .cloned()
                .ok_or_else(|| QuoteWarmupError::UnknownPool(request.pool.clone()))?;
            let adapter = self
                .adapter(pool.protocol())
                .cloned()
                .ok_or(QuoteWarmupError::MissingAdapter(pool.protocol()))?;
            let result = validate_one(
                &pool,
                adapter.as_ref(),
                &self.sim_config,
                Arc::clone(&snapshot),
                request,
            );
            if let Err(QuoteWarmupError::IncompleteSnapshot { missing, .. }) = &result {
                let mut grown = self
                    .quote_read_sets
                    .get(request)
                    .cloned()
                    .unwrap_or_default();
                grown.extend(&missing.as_read_set());
                ensure_within_limits(self.quote_read_set_limits, request, &grown)?;
                self.quote_read_sets.insert(request.clone(), grown);
            }
            result?;
        }
        Ok(QuoteReadinessReport {
            checked: requests.len(),
        })
    }
}

fn ensure_within_limits(
    limits: QuoteReadSetLimits,
    request: &QuoteWarmup,
    access: &UpstreamStorageAccessList,
) -> Result<(), QuoteWarmupError> {
    if limits.accepts(access) {
        return Ok(());
    }
    Err(QuoteWarmupError::ReadSetLimit {
        request: Box::new(request.clone()),
        accounts: access.accounts.len(),
        code_hashes: access.code_hashes.len(),
        slots: access.slots.len(),
        block_hashes: access.block_numbers.len(),
    })
}

fn simulation_error(request: &QuoteWarmup, error: SimError) -> QuoteWarmupError {
    QuoteWarmupError::Simulation {
        request: Box::new(request.clone()),
        message: error.to_string(),
    }
}

fn validate_one(
    pool: &super::PoolRegistration,
    adapter: &dyn super::AmmAdapter,
    config: &super::SimConfig,
    snapshot: Arc<EvmSnapshot>,
    request: &QuoteWarmup,
) -> Result<SwapQuote, QuoteWarmupError> {
    let mut offline = OfflineSnapshotCache::new(snapshot);
    let quote = adapter
        .simulate_swap(
            pool,
            &mut offline,
            request.token_in,
            request.token_out,
            request.amount_in,
            config,
        )
        .map_err(|error| simulation_error(request, error))?;
    if offline.missing_state().is_empty() {
        Ok(quote)
    } else {
        Err(QuoteWarmupError::IncompleteSnapshot {
            request: Box::new(request.clone()),
            missing: Box::new(offline.missing_state().clone()),
        })
    }
}

struct RecordingCache<'a> {
    cache: &'a mut EvmCache,
    baseline: Arc<EvmSnapshot>,
    accesses: UpstreamStorageAccessList,
    provider_reads: UpstreamStorageAccessList,
}

impl<'a> RecordingCache<'a> {
    fn new(cache: &'a mut EvmCache) -> Self {
        let baseline = cache.snapshot();
        Self {
            cache,
            baseline,
            accesses: UpstreamStorageAccessList::default(),
            provider_reads: UpstreamStorageAccessList::default(),
        }
    }

    fn into_parts(self) -> (UpstreamStorageAccessList, UpstreamStorageAccessList) {
        (self.accesses, self.provider_reads)
    }

    fn record(
        &mut self,
        result: Result<(CallOutcome, UpstreamStorageAccessList), CacheError>,
    ) -> Result<CallOutcome, CacheError> {
        result.map(|(outcome, accesses)| {
            self.provider_reads
                .extend(&self.baseline.missing_read_set(&accesses));
            self.accesses.extend(&accesses);
            outcome
        })
    }
}

impl StateView for RecordingCache<'_> {
    fn storage(&self, address: Address, slot: U256) -> Option<U256> {
        evm_fork_cache::StateView::storage(self.cache, address, slot)
    }
}

impl AdapterCache for RecordingCache<'_> {
    fn cached_storage(&self, address: Address, slot: U256) -> Option<U256> {
        self.cache.cached_storage_value(address, slot)
    }

    fn apply_updates(&mut self, updates: &[StateUpdate]) -> StateDiff {
        <EvmCache as AdapterCache>::apply_updates(self.cache, updates)
    }

    fn verify_slots(&mut self, slots: &[(Address, U256)]) -> Result<Vec<SlotChange>, CacheError> {
        <EvmCache as AdapterCache>::verify_slots(self.cache, slots)
    }

    fn purge_storage(&mut self, address: Address) -> StateDiff {
        <EvmCache as AdapterCache>::purge_storage(self.cache, address)
    }

    fn purge_slots(&mut self, address: Address, slots: &[U256]) -> StateDiff {
        <EvmCache as AdapterCache>::purge_slots(self.cache, address, slots)
    }

    fn read_storage_slot(&mut self, address: Address, slot: U256) -> Result<U256, CacheError> {
        <EvmCache as AdapterCache>::read_storage_slot(self.cache, address, slot)
    }

    fn read_storage_slots(&mut self, slots: &[(Address, U256)]) -> Result<Vec<U256>, CacheError> {
        <EvmCache as AdapterCache>::read_storage_slots(self.cache, slots)
    }

    fn call_raw(
        &mut self,
        from: Address,
        to: Address,
        calldata: Bytes,
        _commit: bool,
    ) -> Result<CallOutcome, CacheError> {
        let result =
            <EvmCache as AdapterCache>::call_raw_with_access_list(self.cache, from, to, calldata);
        self.record(result)
    }

    fn call_raw_with_code_overrides(
        &mut self,
        from: Address,
        to: Address,
        calldata: Bytes,
        code_overrides: &[(Address, Bytes)],
        _commit: bool,
    ) -> Result<CallOutcome, CacheError> {
        let result = <EvmCache as AdapterCache>::call_raw_with_code_overrides_and_access_list(
            self.cache,
            from,
            to,
            calldata,
            code_overrides,
        );
        self.record(result)
    }
}

struct OfflineSnapshotCache {
    snapshot: Arc<EvmSnapshot>,
    overlay: EvmOverlay,
}

impl OfflineSnapshotCache {
    fn new(snapshot: Arc<EvmSnapshot>) -> Self {
        Self {
            overlay: EvmOverlay::new(Arc::clone(&snapshot), None),
            snapshot,
        }
    }

    fn missing_state(&self) -> &MissingState {
        self.overlay.missing_state()
    }
}

impl StateView for OfflineSnapshotCache {
    fn storage(&self, address: Address, slot: U256) -> Option<U256> {
        self.snapshot.storage_value(address, slot)
    }
}

impl AdapterCache for OfflineSnapshotCache {
    fn cached_storage(&self, address: Address, slot: U256) -> Option<U256> {
        self.snapshot.storage_value(address, slot)
    }

    fn apply_updates(&mut self, _updates: &[StateUpdate]) -> StateDiff {
        StateDiff::default()
    }

    fn verify_slots(&mut self, _slots: &[(Address, U256)]) -> Result<Vec<SlotChange>, CacheError> {
        Err(offline_mutation_error("verify_slots"))
    }

    fn purge_storage(&mut self, _address: Address) -> StateDiff {
        StateDiff::default()
    }

    fn purge_slots(&mut self, _address: Address, _slots: &[U256]) -> StateDiff {
        StateDiff::default()
    }

    fn read_storage_slot(&mut self, address: Address, slot: U256) -> Result<U256, CacheError> {
        self.overlay
            .storage(address, slot)
            .map_err(|error| CacheError::Backend(Box::new(error)))
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
}

fn offline_mutation_error(operation: &'static str) -> CacheError {
    CacheError::Backend(Box::new(std::io::Error::other(format!(
        "{operation} is unavailable on an offline quote-readiness snapshot"
    ))))
}
