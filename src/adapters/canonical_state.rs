//! Canonical snapshots from complete state feeds, without fabricated AMM logs.
//!
//! The source is trusted to supply every committed effect for its declared
//! scope, just as a canonical log driver attests log completeness. This module
//! verifies header hashing, lineage, snapshot context, coverage, code identity
//! and bounded rollback. It does not verify Merkle proofs or consensus, fetch
//! missing state, persist cursors, or grant application execution readiness.

use std::{collections::BTreeMap, fmt, sync::Arc};

use alloy_primitives::{Address, B256};
use alloy_rpc_types_eth::Header;
use evm_fork_cache::{
    StorageAccessList,
    cache::{EvmOverlay, EvmSnapshot},
};
use revm::Database;

use super::{
    AmmCanonicalHeader, AmmRuntimeId, AmmStatePoint, AmmStateSnapshot, AmmStateVersion,
    PoolStateRevision,
};

/// Immutable identity of the externally reviewed complete-state source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceIdentity {
    /// Chain authenticated by the source/bootstrap owner.
    pub chain_id: u64,
    /// Digest of the reviewed completeness policy, including dynamic-key rules.
    pub scope: B256,
    /// Digest of the exact mapper/package implementation.
    pub package: B256,
    /// Source data model version; distinct from a gRPC API version.
    pub model: u32,
}

/// Complete state supplied by the authenticated source owner at one block.
///
/// Construction is not admission. `CanonicalStateOwner` validates it atomically
/// before producing a commit. The cache must be provider-incapable and contain
/// exact post-block accounts/code/storage for the declared scope. The caller
/// must validate ordered old/new transitions and dynamic-key completeness before
/// supplying it; a header hash alone does not prove the state's contents.
pub struct StateDelivery {
    /// Exact configured identity, including completeness-policy digest.
    pub source: SourceIdentity,
    /// Full consensus-hashed canonical header.
    pub header: Header,
    /// Complete immutable post-block state; no cache-miss provider is retained.
    pub state: Arc<EvmSnapshot>,
    /// Monotonic last irreversible height supplied by the canonical source.
    pub irreversible: u64,
}

/// A validated source commit; source durability/readiness remains application-owned.
#[derive(Clone)]
pub struct CanonicalStateCommit {
    snapshot: Arc<AmmStateSnapshot>,
    header: Arc<AmmCanonicalHeader>,
}

impl CanonicalStateCommit {
    /// Coherent AMM state with the same topology, cache and canonical point.
    pub fn snapshot(&self) -> &Arc<AmmStateSnapshot> {
        &self.snapshot
    }

    /// Hash-verified header; never claims log completeness.
    pub fn header(&self) -> &AmmCanonicalHeader {
        &self.header
    }
}

/// Bounded validation failures. No sensitive source URL or payload is retained.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StateSourceError {
    /// Source identity or immutable scope differs.
    Identity,
    /// Invalid scope, history bound, or oversized state.
    Bounds,
    /// Header hash or EVM execution context is inconsistent.
    Header,
    /// Delivery does not extend the current canonical point.
    Parent,
    /// An explicitly required account, code body or slot is missing.
    Incomplete,
    /// A reviewed account's runtime code changed.
    CodeChanged,
    /// Finality regressed, exceeds the head, or prevents rollback.
    Finality,
    /// Undo target is not retained under its exact hash.
    UnknownUndo,
    /// State/revision sequence exhausted.
    Overflow,
}
impl fmt::Display for StateSourceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "canonical state source rejected: {self:?}")
    }
}
impl std::error::Error for StateSourceError {}

/// Single mutable owner for one fixed topology and complete-state scope.
///
/// Bootstrap transfers the immutable registry from an already prepared snapshot,
/// allocates a new process lineage, and retains no runtime/provider handle.
/// Rebootstrap is required for topology or reviewed code changes. Application
/// readiness must be revoked before calling `advance`/`rewind`, and publication
/// must wait for the application's durable cursor/state acknowledgement.
pub struct CanonicalStateOwner {
    source: SourceIdentity,
    required: StorageAccessList,
    code: BTreeMap<Address, B256>,
    max_slots: usize,
    history_limit: usize,
    irreversible: u64,
    history: Vec<CanonicalStateCommit>,
    current: CanonicalStateCommit,
}

impl CanonicalStateOwner {
    /// Transfer a fully prepared baseline into a new source-owned lineage.
    ///
    /// `required` contains all invariant account/code/static-slot dependencies.
    /// Per-height keys may additionally be present up to `max_slots`; their
    /// exact derivation/completeness is bound by `source.scope` and verified by
    /// the source owner, not inferred from missing cells. All baseline accounts
    /// and code bodies remain required. No absent storage defaults to zero.
    pub fn bootstrap(
        baseline: &AmmStateSnapshot,
        source: SourceIdentity,
        header: Header,
        mut required: StorageAccessList,
        max_slots: usize,
        history_limit: usize,
        irreversible: u64,
    ) -> Result<Self, StateSourceError> {
        if source.chain_id == 0
            || source.scope == B256::ZERO
            || source.package == B256::ZERO
            || source.model == 0
            || baseline.point().chain_id() != source.chain_id
        {
            return Err(StateSourceError::Identity);
        }
        if !(1..=256).contains(&history_limit) || !(1..=65_536).contains(&max_slots) {
            return Err(StateSourceError::Bounds);
        }
        let resident = baseline.cache().resident_read_set();
        required.accounts.extend(resident.accounts);
        required.code_hashes.extend(resident.code_hashes);
        if required.accounts.is_empty()
            || required.accounts.len() > 1024
            || required.slots.is_empty()
            || required.slots.len() > max_slots
            || required
                .slots
                .iter()
                .any(|(a, _)| !required.accounts.contains(a))
        {
            return Err(StateSourceError::Bounds);
        }
        let code = required
            .accounts
            .iter()
            .map(|a| {
                baseline
                    .cache()
                    .account_code_hash(*a)
                    .map(|hash| (*a, hash))
                    .ok_or(StateSourceError::Incomplete)
            })
            .collect::<Result<_, _>>()?;
        if baseline.point().block_number() != header.number
            || baseline.point().block_hash() != header.hash
        {
            return Err(StateSourceError::Header);
        }
        if irreversible > header.number {
            return Err(StateSourceError::Finality);
        }
        let snapshot = Arc::new(AmmStateSnapshot::new(
            AmmRuntimeId::allocate(),
            AmmStateVersion::initial(),
            baseline.point(),
            baseline.interest_revision(),
            baseline.cache_snapshot(),
            baseline.registry_snapshot(),
            baseline.pool_revisions_snapshot(),
        ));
        let current = CanonicalStateCommit {
            snapshot,
            header: Arc::new(AmmCanonicalHeader::from_verified_parts(
                source.chain_id,
                header,
            )),
        };
        let owner = Self {
            source,
            required,
            code,
            max_slots,
            history_limit,
            irreversible,
            history: vec![current.clone()],
            current,
        };
        owner.validate(
            owner.current.header.header(),
            owner.current.snapshot.cache_snapshot(),
        )?;
        Ok(owner)
    }

    /// Last committed state. This is not an application readiness permit.
    pub const fn current(&self) -> &CanonicalStateCommit {
        &self.current
    }

    /// Validate and commit one complete successor. Failure leaves all state intact.
    pub fn advance(
        &mut self,
        delivery: StateDelivery,
    ) -> Result<CanonicalStateCommit, StateSourceError> {
        if delivery.source != self.source {
            return Err(StateSourceError::Identity);
        }
        let parent = self.current.header.header();
        if parent.number.checked_add(1) != Some(delivery.header.number)
            || delivery.header.parent_hash != parent.hash
            || delivery.header.timestamp < parent.timestamp
        {
            return Err(StateSourceError::Parent);
        }
        if delivery.irreversible < self.irreversible
            || delivery.irreversible > delivery.header.number
        {
            return Err(StateSourceError::Finality);
        }
        self.validate(&delivery.header, Arc::clone(&delivery.state))?;
        let commit = self.mint(delivery.header, delivery.state)?;
        self.irreversible = delivery.irreversible;
        self.history.push(commit.clone());
        if self.history.len() > self.history_limit {
            self.history.remove(0);
        }
        self.current = commit.clone();
        Ok(commit)
    }

    /// Roll back only to a retained, non-finalized exact point; mint a new version.
    pub fn rewind(
        &mut self,
        number: u64,
        hash: B256,
    ) -> Result<CanonicalStateCommit, StateSourceError> {
        if number < self.irreversible {
            return Err(StateSourceError::Finality);
        }
        let index = self
            .history
            .iter()
            .position(|c| {
                c.header.point().block_number() == number && c.header.point().block_hash() == hash
            })
            .ok_or(StateSourceError::UnknownUndo)?;
        let target = &self.history[index];
        let commit = self.mint(
            target.header.header().clone(),
            target.snapshot.cache_snapshot(),
        )?;
        self.history.truncate(index + 1);
        self.history[index] = commit.clone();
        self.current = commit.clone();
        Ok(commit)
    }

    fn validate(&self, header: &Header, state: Arc<EvmSnapshot>) -> Result<(), StateSourceError> {
        if header.hash != header.inner.hash_slow()
            || state.chain_id() != self.source.chain_id
            || state.block_number() != Some(header.number)
            || state.block_context_hash() != Some(header.hash)
            || state.timestamp() != Some(header.timestamp)
            || state.basefee() != header.base_fee_per_gas
            || state.coinbase() != Some(header.beneficiary)
            || state.gas_limit() != Some(header.gas_limit)
            || state.prevrandao() != Some(header.mix_hash)
        {
            return Err(StateSourceError::Header);
        }
        let resident = state.resident_read_set();
        if resident.accounts != self.required.accounts
            || resident.slots.len() > self.max_slots
            || resident.code_hashes != self.required.code_hashes
        {
            return Err(StateSourceError::Bounds);
        }
        if !state.missing_read_set(&self.required).is_empty() {
            return Err(StateSourceError::Incomplete);
        }
        if state.spec_id() != self.current.snapshot.cache().spec_id() {
            return Err(StateSourceError::Identity);
        }
        if self
            .code
            .iter()
            .any(|(a, h)| state.account_code_hash(*a) != Some(*h))
        {
            return Err(StateSourceError::CodeChanged);
        }
        let mut overlay = EvmOverlay::new(state, None);
        for (address, hash) in &self.code {
            let account = overlay
                .basic(*address)
                .map_err(|_| StateSourceError::Incomplete)?
                .ok_or(StateSourceError::Incomplete)?;
            let code = account.code.map_or_else(
                || {
                    overlay
                        .code_by_hash(*hash)
                        .map_err(|_| StateSourceError::Incomplete)
                },
                Ok,
            )?;
            if code.hash_slow() != *hash {
                return Err(StateSourceError::CodeChanged);
            }
        }
        Ok(())
    }

    fn mint(
        &self,
        header: Header,
        state: Arc<EvmSnapshot>,
    ) -> Result<CanonicalStateCommit, StateSourceError> {
        let old = &self.current.snapshot;
        let version = old
            .version()
            .checked_next()
            .map_err(|_| StateSourceError::Overflow)?;
        // Conservatively invalidate every pool's quote memo on each complete
        // state transition, including rollback. Optimizing this is separate work.
        let revisions = old
            .pool_revisions()
            .iter()
            .map(|(pool, rev)| {
                rev.get()
                    .checked_add(1)
                    .map(|v| (pool.clone(), PoolStateRevision::new(v)))
                    .ok_or(StateSourceError::Overflow)
            })
            .collect::<Result<_, _>>()?;
        let point = AmmStatePoint::post_block(self.source.chain_id, header.number, header.hash);
        Ok(CanonicalStateCommit {
            snapshot: Arc::new(AmmStateSnapshot::new(
                old.runtime_id(),
                version,
                point,
                old.interest_revision(),
                state,
                old.registry_snapshot(),
                Arc::new(revisions),
            )),
            header: Arc::new(AmmCanonicalHeader::from_verified_parts(
                self.source.chain_id,
                header,
            )),
        })
    }
}

#[cfg(test)]
mod tests;
