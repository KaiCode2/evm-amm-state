//! Protocol-neutral AMM adapter boundary.
//!
//! This module defines the crate-owned contract between AMM protocol semantics
//! and the generic EVM state cache in `evm-fork-cache`.

pub mod cache;
pub mod registry;
pub mod storage;
pub mod traits;
pub mod types;

pub use cache::{AdapterCache, PurgeScope, SlotChange, StateDiff, StateUpdate, StateView};
pub use registry::{AdapterRegistry, RegistryError};
pub use traits::AmmAdapter;
pub use types::{
    AdapterEvent, AdapterEventError, AdapterEventKind, AdapterEventResult, ColdStartOutcome,
    ColdStartPolicy, ColdStartReport, CustomPoolKey, DeferredWork, EventRoute, EventSource,
    PoolKey, PoolRegistration, PoolStatus, ProtocolId, ProtocolMetadata, RepairAction,
    UnsupportedReason, UpdateQuality,
};
