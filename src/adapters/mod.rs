//! Protocol-neutral AMM adapter boundary.
//!
//! This module defines the crate-owned contract between AMM protocol semantics
//! and the generic EVM state cache in `evm-fork-cache`.

// Protocol-neutral infrastructure — always compiled (no heavy deps).
pub mod bytecode;
pub mod cache;
pub mod cold_start;
pub mod driver;
pub mod factory;
pub mod reactive;
pub mod registry;
pub mod repair;
pub mod sim;
pub mod state;
pub mod storage;
pub mod storage_sync;
pub mod sync_manager;
pub mod traits;
pub mod types;

// Per-protocol adapters — gated by their protocol feature.
#[cfg(feature = "balancer-v2")]
pub mod balancer_v2;
#[cfg(feature = "curve")]
pub mod curve;
#[cfg(feature = "solidly-v2")]
pub mod solidly_v2;
#[cfg(feature = "uniswap-v2")]
pub mod uniswap_v2;
#[cfg(feature = "uniswap-v3")]
pub mod uniswap_v3;
#[cfg(feature = "uniswap-v3")]
pub mod v3_sync;

pub use bytecode::{
    AdapterCodeSeed, BytecodePatch, BytecodeTemplateError, V3ImmutablePatchValues,
    V3ImmutablePatches, V3RuntimeBytecodeTemplate, uniswap_v2_pair_code_seed,
    uniswap_v2_pair_runtime_bytecode, uniswap_v2_pair_runtime_code_hash, uniswap_v3_code_seed,
    uniswap_v3_max_liquidity_per_tick, uniswap_v3_pool_template, v3_code_seed_from_metadata,
};
pub use cache::{
    AdapterCache, CacheError, CallOutcome, PurgeScope, SkippedDelta, SkippedMask, SlotChange,
    SlotDelta, StateDiff, StateUpdate, StateView,
};
pub use cold_start::{
    AdapterColdStartPlanner, CodeSeedMismatch, CodeSeedReport, ColdStartCall, ColdStartCallResult,
    ColdStartError, ColdStartPlan, ColdStartResults, ColdStartRunReport, ColdStartStep,
    HydrationError, SlotFetch, SlotOutcome, StorageAccessList, supports_one_shot_hydration,
};
pub use driver::{AdapterDriver, DriverError};
#[cfg(feature = "uniswap-v2")]
pub use factory::UniswapV2FactoryConfig;
#[cfg(feature = "uniswap-v3")]
pub use factory::UniswapV3FactoryConfig;
pub use factory::{
    CreationLogContext, DiscoveredPool, DiscoveryError, DiscoverySource, FactoryConfig,
    PoolDiscovery, PoolFactory, PoolQuery,
};
pub use reactive::AmmReactiveHandler;
pub use registry::{AdapterRegistry, RegistryError, SubscriptionSpec};
pub use sim::{SimConfig, SimError, SwapQuote, quote_via_call};
pub use storage_sync::{
    CALLDATA_SLOT_LOADER_CODE, StorageSyncEncoding, StorageSyncError, StorageSyncSnapshot,
    StorageSyncSpec, build_calldata_slot_loader_program, build_slot_loader_program,
    decode_storage_sync, run_and_inject_storage_sync, run_and_inject_storage_syncs,
    run_storage_sync, run_storage_syncs, slot_loader_calldata, storage_sync_spec_for_pool,
};
pub use sync_manager::{AmmSyncBatchReport, AmmSyncEngine, AmmSyncError};
pub use traits::AmmAdapter;
pub use types::{
    AdapterEvent, AdapterEventError, AdapterEventKind, AdapterEventReport, AdapterEventResult,
    BalancerV2Metadata, ColdStartOutcome, ColdStartPolicy, ColdStartReport, CurveMetadata,
    CurveVariant, CustomPoolKey, DeferredOutcome, DeferredWork, EventRoute, EventSource, PoolKey,
    PoolRegistration, PoolStatus, ProtocolId, ProtocolMetadata, RepairAction, SolidlyV2Metadata,
    UniswapV2Metadata, UnsupportedReason, UpdateQuality, V3Metadata,
};

#[cfg(feature = "balancer-v2")]
pub use balancer_v2::BalancerV2Adapter;
#[cfg(feature = "curve")]
pub use curve::CurveAdapter;
#[cfg(feature = "solidly-v2")]
pub use solidly_v2::SolidlyV2Adapter;
#[cfg(feature = "uniswap-v2")]
pub use uniswap_v2::UniswapV2Adapter;
#[cfg(feature = "uniswap-v3")]
pub use uniswap_v3::ConcentratedLiquidityAdapter;
