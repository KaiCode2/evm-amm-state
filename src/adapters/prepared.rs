//! Hash-pinned, worker-produced state ready for serialized runtime commit.

use std::collections::BTreeSet;
use std::fmt;

use alloy_primitives::{Address, U256};
use evm_fork_cache::PreparedAccountPatch;

use super::{AmmStatePoint, DeferredWork, PoolInstanceId, PoolRegistration, RuntimeWorkId};

/// One authoritative storage value fetched for a prepared pool.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AmmPreparedStorage {
    address: Address,
    slot: U256,
    value: U256,
}

impl AmmPreparedStorage {
    /// Construct one prepared storage value.
    pub const fn new(address: Address, slot: U256, value: U256) -> Self {
        Self {
            address,
            slot,
            value,
        }
    }

    /// Storage-owning contract.
    pub const fn address(self) -> Address {
        self.address
    }

    /// Storage key.
    pub const fn slot(self) -> U256 {
        self.slot
    }

    /// Authoritative value at the prepared baseline.
    pub const fn value(self) -> U256 {
        self.value
    }
}

/// Worker-produced pool metadata, exact state, and optional deferred follow-up
/// work pinned to one canonical point.
///
/// The public constructor is a trusted integration seam: the caller attests
/// that every value was fetched from the advertised hash. Runtime-owned
/// background jobs add an unforgeable work/generation claim internally before
/// they may use the scheduled commit path. Applications should normally queue
/// cold-start work through [`AmmRuntimeHandle::queue_cold_start`](super::AmmRuntimeHandle::queue_cold_start)
/// instead of constructing this artifact directly.
#[derive(Clone, Debug)]
pub struct AmmPreparedPoolState {
    registration: PoolRegistration,
    baseline: AmmStatePoint,
    storage: Vec<AmmPreparedStorage>,
    accounts: Option<PreparedAccountPatch>,
    deferred: Vec<DeferredWork>,
    schedule: Option<(RuntimeWorkId, PoolInstanceId)>,
}

pub(crate) type AmmPreparedPoolParts = (
    PoolRegistration,
    AmmStatePoint,
    Vec<AmmPreparedStorage>,
    Option<PreparedAccountPatch>,
    Vec<DeferredWork>,
    Option<(RuntimeWorkId, PoolInstanceId)>,
);

impl AmmPreparedPoolState {
    /// Construct a caller-attested prepared pool, rejecting duplicate storage identities.
    ///
    /// This validates the artifact's shape, not its external RPC provenance.
    /// Passing values that were not fetched at `baseline` violates this API's
    /// trust contract. The runtime still rejects a baseline that is no longer
    /// current and validates the adapter's declared storage dependencies.
    pub fn new(
        registration: PoolRegistration,
        baseline: AmmStatePoint,
        storage: impl IntoIterator<Item = AmmPreparedStorage>,
    ) -> Result<Self, AmmPreparedStateError> {
        let mut storage: Vec<_> = storage.into_iter().collect();
        storage.sort_unstable_by_key(|entry| (entry.address, entry.slot));
        let mut identities = BTreeSet::new();
        for entry in &storage {
            if !identities.insert((entry.address, entry.slot)) {
                return Err(AmmPreparedStateError::DuplicateStorage {
                    address: entry.address,
                    slot: entry.slot,
                });
            }
        }
        Ok(Self {
            registration,
            baseline,
            storage,
            accounts: None,
            deferred: Vec::new(),
            schedule: None,
        })
    }

    /// Prepared registration metadata.
    pub const fn registration(&self) -> &PoolRegistration {
        &self.registration
    }

    /// Exact post-block point used for every fetched value.
    pub const fn baseline(&self) -> AmmStatePoint {
        self.baseline
    }

    /// Canonically ordered prepared storage.
    pub fn storage(&self) -> &[AmmPreparedStorage] {
        &self.storage
    }

    /// Exact-hash account/code proof patch, when the worker verified code seeds.
    pub const fn accounts(&self) -> Option<&PreparedAccountPatch> {
        self.accounts.as_ref()
    }

    /// Optional adapter work intentionally left until after the pool is searchable.
    pub fn deferred(&self) -> &[DeferredWork] {
        &self.deferred
    }

    /// Scheduled work identity, when produced by the runtime worker.
    pub fn work(&self) -> Option<&RuntimeWorkId> {
        self.schedule.as_ref().map(|(work, _)| work)
    }

    /// Reserved pool generation, when produced by the runtime worker.
    pub fn pool_instance(&self) -> Option<&PoolInstanceId> {
        self.schedule.as_ref().map(|(_, pool)| pool)
    }

    #[allow(dead_code)]
    pub(crate) fn with_schedule(mut self, work: RuntimeWorkId, pool: PoolInstanceId) -> Self {
        self.schedule = Some((work, pool));
        self
    }

    pub(crate) fn with_accounts(mut self, accounts: PreparedAccountPatch) -> Self {
        self.accounts = Some(accounts);
        self
    }

    pub(crate) fn with_deferred(mut self, deferred: Vec<DeferredWork>) -> Self {
        self.deferred = deferred;
        self
    }

    pub(crate) fn into_parts(self) -> AmmPreparedPoolParts {
        (
            self.registration,
            self.baseline,
            self.storage,
            self.accounts,
            self.deferred,
            self.schedule,
        )
    }
}

/// Invalid worker-produced prepared state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum AmmPreparedStateError {
    /// The artifact carried more than one value for the same storage identity.
    DuplicateStorage {
        /// Storage-owning contract.
        address: Address,
        /// Duplicated storage key.
        slot: U256,
    },
}

impl fmt::Display for AmmPreparedStateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateStorage { address, slot } => {
                write!(
                    f,
                    "duplicate prepared storage value for ({address}, {slot})"
                )
            }
        }
    }
}

impl std::error::Error for AmmPreparedStateError {}
