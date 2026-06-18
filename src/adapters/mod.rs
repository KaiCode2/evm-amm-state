//! Protocol-neutral AMM adapter boundary.
//!
//! This module defines the crate-owned contract between AMM protocol semantics
//! and the generic EVM state cache in `evm-fork-cache`.

pub mod balancer_v2;
pub mod cache;
pub mod driver;
pub mod registry;
pub mod storage;
pub mod traits;
pub mod types;
pub mod uniswap_v2;
pub mod uniswap_v3;

pub use balancer_v2::BalancerV2Adapter;
pub use cache::{
    AdapterCache, PurgeScope, SkippedDelta, SkippedMask, SlotChange, SlotDelta, StateDiff,
    StateUpdate, StateView,
};
pub use driver::AdapterDriver;
pub use registry::{AdapterRegistry, RegistryError, SubscriptionSpec};
pub use traits::AmmAdapter;
pub use types::{
    AdapterEvent, AdapterEventError, AdapterEventKind, AdapterEventReport, AdapterEventResult,
    BalancerV2Metadata, ColdStartOutcome, ColdStartPolicy, ColdStartReport, CustomPoolKey,
    DeferredWork, EventRoute, EventSource, PoolKey, PoolRegistration, PoolStatus, ProtocolId,
    ProtocolMetadata, RepairAction, UniswapV2Metadata, UnsupportedReason, UpdateQuality,
    V3Metadata,
};
pub use uniswap_v2::UniswapV2Adapter;
pub use uniswap_v3::UniswapV3Adapter;
