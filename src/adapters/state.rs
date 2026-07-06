//! Crate-owned mirrors of the `evm_fork_cache` state-mutation vocabulary.
//!
//! `evm-amm-state` speaks its own state types across its public surface so the
//! facade does not leak `evm_fork_cache`'s own types. Every mirror here keeps the
//! variant / field / method / constructor **names** identical to upstream, so
//! call-sites read the same; the [`From`] conversions in this module bridge to and
//! from the upstream types at the cache/reactive/cold-start boundaries.
//!
//! The mirror set covers only the vocabulary the crate actually constructs,
//! matches, or exposes: the [`StateUpdate`] variants the adapters emit
//! (`Slot`/`SlotMasked`/`SlotDelta`/`Purge`), the [`StateDiff`] they inspect, its
//! [`SlotChange`] / [`SkippedDelta`] / [`SkippedMask`] leaves, [`PurgeScope`],
//! [`SlotDelta`], and the read-only [`StateView`] trait.

use alloy_primitives::{Address, U256};

/// A relative storage-slot mutation: read the current value, transform it, and
/// write it back.
///
/// Crate-owned mirror of [`evm_fork_cache::SlotDelta`]. Both directions
/// **saturate**: `Add` clamps at `U256::MAX`, `Sub` at `U256::ZERO`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlotDelta {
    /// Add to the current value, saturating at `U256::MAX`.
    Add(U256),
    /// Subtract from the current value, saturating at `U256::ZERO`.
    Sub(U256),
}

impl SlotDelta {
    /// Apply the (saturating) delta to a current value.
    pub fn apply(self, current: U256) -> U256 {
        match self {
            SlotDelta::Add(amount) => current.saturating_add(amount),
            SlotDelta::Sub(amount) => current.saturating_sub(amount),
        }
    }
}

impl From<SlotDelta> for evm_fork_cache::SlotDelta {
    fn from(delta: SlotDelta) -> Self {
        match delta {
            SlotDelta::Add(amount) => evm_fork_cache::SlotDelta::Add(amount),
            SlotDelta::Sub(amount) => evm_fork_cache::SlotDelta::Sub(amount),
        }
    }
}

impl From<evm_fork_cache::SlotDelta> for SlotDelta {
    fn from(delta: evm_fork_cache::SlotDelta) -> Self {
        match delta {
            evm_fork_cache::SlotDelta::Add(amount) => SlotDelta::Add(amount),
            evm_fork_cache::SlotDelta::Sub(amount) => SlotDelta::Sub(amount),
        }
    }
}

/// A single targeted mutation to cached EVM state.
///
/// Crate-owned mirror of [`evm_fork_cache::StateUpdate`], carrying only the
/// variants the crate's adapters construct and match. `#[non_exhaustive]` so more
/// variants can be mirrored without a breaking change.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum StateUpdate {
    /// Set one storage slot to `value`, authoritative across both cache layers.
    Slot {
        /// Contract whose storage is written.
        address: Address,
        /// Storage slot key.
        slot: U256,
        /// New slot value.
        value: U256,
    },
    /// Apply a *relative* (saturating) mutation to one storage slot (cold-aware).
    SlotDelta {
        /// Contract whose storage is written.
        address: Address,
        /// Storage slot key.
        slot: U256,
        /// The relative, saturating mutation to apply to the current value.
        delta: SlotDelta,
    },
    /// Set only the `mask` bits of a storage slot to the corresponding bits of
    /// `value`, preserving the rest (cold-aware).
    SlotMasked {
        /// Contract whose storage is written.
        address: Address,
        /// Storage slot key.
        slot: U256,
        /// Which bits of the slot to overwrite (1 = take from `value`).
        mask: U256,
        /// The bits to write (only the bits selected by `mask` are applied).
        value: U256,
    },
    /// Purge cached state for `address` at `scope`; the next read re-fetches.
    Purge {
        /// Account whose cached state is purged.
        address: Address,
        /// What part of the cached state to remove.
        scope: PurgeScope,
    },
}

impl StateUpdate {
    /// Construct a [`StateUpdate::Slot`] that sets `(address, slot)` to `value`.
    pub fn slot(address: Address, slot: U256, value: U256) -> Self {
        Self::Slot {
            address,
            slot,
            value,
        }
    }

    /// Construct a [`StateUpdate::SlotDelta`] that applies `delta` relative to the
    /// current value of `(address, slot)`.
    pub fn slot_delta(address: Address, slot: U256, delta: SlotDelta) -> Self {
        Self::SlotDelta {
            address,
            slot,
            delta,
        }
    }

    /// Construct a [`StateUpdate::SlotMasked`] that sets only the `mask` bits of
    /// `(address, slot)` to the corresponding bits of `value`.
    pub fn slot_masked(address: Address, slot: U256, mask: U256, value: U256) -> Self {
        Self::SlotMasked {
            address,
            slot,
            mask,
            value,
        }
    }

    /// Construct a [`StateUpdate::Purge`] for `address` at `scope`.
    pub fn purge(address: Address, scope: PurgeScope) -> Self {
        Self::Purge { address, scope }
    }
}

impl From<StateUpdate> for evm_fork_cache::StateUpdate {
    fn from(update: StateUpdate) -> Self {
        match update {
            StateUpdate::Slot {
                address,
                slot,
                value,
            } => evm_fork_cache::StateUpdate::slot(address, slot, value),
            StateUpdate::SlotDelta {
                address,
                slot,
                delta,
            } => evm_fork_cache::StateUpdate::slot_delta(address, slot, delta.into()),
            StateUpdate::SlotMasked {
                address,
                slot,
                mask,
                value,
            } => evm_fork_cache::StateUpdate::slot_masked(address, slot, mask, value),
            StateUpdate::Purge { address, scope } => {
                evm_fork_cache::StateUpdate::purge(address, scope.into())
            }
        }
    }
}

/// What part of an address's cached state a purge removes.
///
/// Crate-owned mirror of [`evm_fork_cache::PurgeScope`], carrying the scopes the
/// crate constructs (`AllStorage`, `Slots`). `#[non_exhaustive]` so more scopes can
/// be mirrored without a breaking change.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum PurgeScope {
    /// All storage slots; account info preserved.
    AllStorage,
    /// Only the listed storage slots.
    Slots(Vec<U256>),
}

impl From<PurgeScope> for evm_fork_cache::PurgeScope {
    fn from(scope: PurgeScope) -> Self {
        match scope {
            PurgeScope::AllStorage => evm_fork_cache::PurgeScope::AllStorage,
            PurgeScope::Slots(slots) => evm_fork_cache::PurgeScope::Slots(slots),
        }
    }
}

/// A storage slot whose value changed: `old` is the prior cached/snapshot value
/// (`ZERO` if previously uncached), `new` is the resulting value.
///
/// Crate-owned mirror of [`evm_fork_cache::SlotChange`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SlotChange {
    /// Contract whose storage changed.
    pub address: Address,
    /// Storage slot key.
    pub slot: U256,
    /// Value previously held in the cache/snapshot.
    pub old: U256,
    /// Freshly-fetched / just-written value.
    pub new: U256,
}

impl From<evm_fork_cache::SlotChange> for SlotChange {
    fn from(change: evm_fork_cache::SlotChange) -> Self {
        Self {
            address: change.address,
            slot: change.slot,
            old: change.old,
            new: change.new,
        }
    }
}

/// A relative update ([`StateUpdate::SlotDelta`]) that could not be applied
/// because the slot's current value is unknown (cold).
///
/// Crate-owned mirror of [`evm_fork_cache::SkippedDelta`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SkippedDelta {
    /// Contract whose storage slot the delta targeted.
    pub address: Address,
    /// Storage slot key that was cold.
    pub slot: U256,
    /// The delta that was not applied.
    pub delta: SlotDelta,
}

impl From<evm_fork_cache::SkippedDelta> for SkippedDelta {
    fn from(skipped: evm_fork_cache::SkippedDelta) -> Self {
        Self {
            address: skipped.address,
            slot: skipped.slot,
            delta: skipped.delta.into(),
        }
    }
}

/// A masked write ([`StateUpdate::SlotMasked`]) that could not be applied because
/// the target slot's current value is unknown (cold).
///
/// Crate-owned mirror of [`evm_fork_cache::SkippedMask`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SkippedMask {
    /// Contract whose storage slot the masked write targeted.
    pub address: Address,
    /// Storage slot key that was cold.
    pub slot: U256,
    /// The mask that was not applied.
    pub mask: U256,
    /// The value bits that were not applied.
    pub value: U256,
}

impl From<evm_fork_cache::SkippedMask> for SkippedMask {
    fn from(skipped: evm_fork_cache::SkippedMask) -> Self {
        Self {
            address: skipped.address,
            slot: skipped.slot,
            mask: skipped.mask,
            value: skipped.value,
        }
    }
}

/// What an `apply_*` call actually changed.
///
/// Crate-owned mirror of [`evm_fork_cache::StateDiff`], carrying the fields the
/// crate reads: the changed [`slots`](Self::slots) and the cold-skipped
/// [`skipped`](Self::skipped) / [`skipped_masks`](Self::skipped_masks) metadata.
/// `#[non_exhaustive]` so more fields can be mirrored without a breaking change.
///
/// Construct via [`Default`] + field assignment, never an exhaustive struct
/// literal.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct StateDiff {
    /// Storage slots whose value changed (`old != new`).
    pub slots: Vec<SlotChange>,
    /// Relative slot updates ([`StateUpdate::SlotDelta`]) that were **not** applied
    /// because the target slot's current value was unknown (cold). Informational
    /// metadata, not a change.
    pub skipped: Vec<SkippedDelta>,
    /// Masked slot updates ([`StateUpdate::SlotMasked`]) that were **not** applied
    /// because the target slot's current value was unknown (cold). Informational
    /// metadata, not a change.
    pub skipped_masks: Vec<SkippedMask>,
}

impl StateDiff {
    /// Whether the diff recorded no change at all (changes-only: skipped metadata
    /// does not count).
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// Total number of changed entries (changes-only: skipped metadata is not
    /// counted).
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    /// Whether any cold-aware update was skipped (relative slot **or** masked
    /// slot). A cold-skipped update produces no change, so it is invisible to
    /// [`is_empty`](Self::is_empty).
    pub fn has_skipped(&self) -> bool {
        !self.skipped.is_empty() || !self.skipped_masks.is_empty()
    }
}

impl From<evm_fork_cache::StateDiff> for StateDiff {
    fn from(diff: evm_fork_cache::StateDiff) -> Self {
        Self {
            slots: diff.slots.into_iter().map(SlotChange::from).collect(),
            skipped: diff.skipped.into_iter().map(SkippedDelta::from).collect(),
            skipped_masks: diff
                .skipped_masks
                .into_iter()
                .map(SkippedMask::from)
                .collect(),
        }
    }
}

/// Read-only view of current cached state handed to a decoder.
///
/// Crate-owned mirror of [`evm_fork_cache::StateView`]. Adapters read pre-state
/// through this; a slot absent from the cache reads `None`. It is the supertrait of
/// [`AdapterCache`](super::AdapterCache).
pub trait StateView {
    /// Current cached value of `(address, slot)`, or `None` when the slot is
    /// **cold** — neither cache layer has seen it.
    fn storage(&self, address: Address, slot: U256) -> Option<U256>;
}

/// Adapts an upstream [`evm_fork_cache::StateView`] into the crate-owned
/// [`StateView`] by delegation.
///
/// The reactive handler and the cold-start `Bridge` receive a
/// `&dyn evm_fork_cache::StateView` from the runtime; wrapping it here lets the
/// crate hand a `&dyn StateView` (crate-owned) to adapter code without leaking the
/// upstream trait.
pub struct UpstreamStateView<'a>(pub &'a dyn evm_fork_cache::StateView);

impl StateView for UpstreamStateView<'_> {
    fn storage(&self, address: Address, slot: U256) -> Option<U256> {
        self.0.storage(address, slot)
    }
}
