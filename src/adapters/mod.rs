//! Protocol-neutral AMM adapter boundary.
//!
//! This module defines the crate-owned contract between AMM protocol semantics
//! and the generic EVM state cache in `evm-fork-cache`.

// Protocol-neutral infrastructure — always compiled (no heavy deps).
pub mod bytecode;
/// The [`AdapterCache`] facade over `evm-fork-cache` (reads, writes, raw calls).
pub mod cache;
pub mod cold_start;
/// Progressive background cold-start scheduling.
#[cfg(feature = "live-runtime")]
pub mod cold_start_scheduler;
/// [`AdapterDriver`], which applies decoded logs to a cache in caller order.
pub mod driver;
pub mod factory;
/// Asynchronous single-writer cache runtime.
#[cfg(feature = "live-runtime")]
pub mod live_runtime;
/// Generation-scoped runtime resource ownership indexes.
pub mod ownership;
/// Versioned warm-resume persistence for immutable pool registrations and hints.
pub mod persistence;
/// Hash-pinned state artifacts produced outside the cache actor.
#[cfg(feature = "live-runtime")]
pub mod prepared;
/// The [`AmmReactiveHandler`] bridge onto the `evm-fork-cache` reactive runtime.
pub mod reactive;
/// The [`AdapterRegistry`] of tracked pools and protocol adapters.
pub mod registry;
pub mod repair;
/// Live-runtime identity, lifecycle, version, and change vocabulary.
pub mod runtime;
pub mod sim;
/// Immutable state and registry publications for concurrent readers.
pub mod snapshot;
pub mod state;
pub mod storage;
pub mod storage_sync;
/// Alloy-specific complete-block and transactional interest driver.
#[cfg(feature = "live-runtime")]
pub mod subscriber_driver;
pub mod sync_manager;
/// The [`AmmAdapter`] protocol-adapter trait.
pub mod traits;
/// Core public vocabulary: pool keys, metadata, events, repairs, and outcomes.
pub mod types;

// Per-protocol adapters — gated by their protocol feature.
/// Balancer V2 adapter (shared-vault `queryBatchSwap` quotes).
#[cfg(feature = "balancer-v2")]
pub mod balancer_v2;
#[cfg(feature = "curve")]
pub mod curve;
/// Solidly V2 (Aerodrome / Velodrome) adapter.
#[cfg(feature = "solidly-v2")]
pub mod solidly_v2;
/// Uniswap V2 adapter (constant-product pairs).
#[cfg(feature = "uniswap-v2")]
pub mod uniswap_v2;
/// Uniswap V3-family adapter (Uniswap V3 / PancakeSwap V3 / Slipstream).
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
#[cfg(feature = "live-runtime")]
pub use cold_start_scheduler::{
    AmmColdStartOptions, AmmColdStartWorkerConfig, AmmColdStartWorkerError,
    AmmColdStartWorkerHandle, AmmColdStartWorkerState, AmmScheduledPool,
};
pub use driver::{AdapterDriver, DriverError};
#[cfg(feature = "uniswap-v2")]
pub use factory::UniswapV2FactoryConfig;
#[cfg(feature = "uniswap-v3")]
pub use factory::{
    ClCreate2, ClFactorySpec, ClKeying, ConcentratedLiquidityFactory, FeeSource,
    UniswapV3FactoryConfig,
};
pub use factory::{
    CreationLogContext, DiscoveredPool, DiscoveryError, DiscoverySource, FactoryConfig,
    PoolDiscovery, PoolFactory, PoolQuery, PreparedDiscoveryReads, TokenEdgeDiscoveryReport,
    TokenEdgeDiscoveryRequest,
};
#[cfg(feature = "solidly-v2")]
pub use factory::{SolidlyFactory, SolidlyFactoryConfig};
#[cfg(feature = "live-runtime")]
pub use live_runtime::{
    AmmCanonicalBatch, AmmCanonicalBatchError, AmmChangeSubscription, AmmCommandId,
    AmmCommandTicket, AmmDiscoveryOptions, AmmFactoryWatcherRegistration, AmmObserver,
    AmmObserverError, AmmRuntime, AmmRuntimeBaseline, AmmRuntimeCommandError, AmmRuntimeConfig,
    AmmRuntimeHandle, AmmRuntimeSpawnError, AmmRuntimeSubmitError, AmmScheduledDiscovery,
    AmmScheduledFollowUp,
};
pub use ownership::{
    AmmOwnershipError, AmmOwnershipIndex, DiscoveryOwnership, DiscoveryOwnershipRemoval,
    PoolOwnership, PoolOwnershipRemoval, PoolStateDependencies, StateSlot,
};
pub use persistence::{AmmRegistrationArchive, AmmRegistrationPersistenceError};
#[cfg(feature = "live-runtime")]
pub use prepared::{AmmPreparedPoolState, AmmPreparedStateError, AmmPreparedStorage};
pub use reactive::{
    AmmPoolReactiveHandler, AmmPoolReactiveHandlerError, AmmReactiveHandler,
    AmmReactiveRoutingContext, AmmReactiveSignal,
};
pub use registry::{AdapterRegistry, RegistryError, SubscriptionSpec};
pub use runtime::{
    AdapterGeneration, AdapterInstanceId, AdapterKey, AmmChangeImpact, AmmChangeSet,
    AmmChangeSetError, AmmPoolChange, AmmPoolChangeKind, AmmRuntimeEvent, AmmRuntimeEventKind,
    AmmRuntimeHealth, AmmRuntimeId, AmmRuntimeStatusSnapshot, AmmStateIncident, AmmStatePoint,
    AmmStateQuality, AmmStateVersion, AmmWorkClass, AmmWorkKind, AmmWorkProgress,
    DiscoveryGeneration, DiscoveryOwnerId, DiscoveryOwnerKey, InvalidPoolRuntimeTransition,
    InvalidWorkProgress, OwnerRuntimeState, PoolGeneration, PoolInstanceId, PoolLifecycle,
    PoolRuntimeState, PoolStateRef, PoolStateRevision, QueryEvidencePolicy, QueueDepths,
    RegistrationEvidenceSet, RegistrationProvenance, RegistrationReorgAction,
    RegistrationSourceKey, RuntimeLifecycleMap, RuntimeOwnerId, RuntimeSequenceOverflow,
    RuntimeWorkId, StatePosition, WorkId,
};
pub use sim::{SimConfig, SimError, SwapQuote, quote_via_call, quote_via_call_from};
pub use snapshot::{
    AdapterRegistrySnapshot, AdapterRegistrySnapshotError, AmmStateCommit, AmmStateSnapshot,
    PoolRevisionMap,
};
#[cfg(feature = "live-runtime")]
pub use subscriber_driver::{
    AmmSubscriberDriverConfig, AmmSubscriberDriverError, AmmSubscriberDriverHandle,
    AmmSubscriberDriverState,
};
// Both layout types are always compiled (`storage` is feature-neutral); export
// them unconditionally — `V3StorageLayout` is the field type of the
// root-exported `V3Metadata.storage_layout`, and gating `SolidlyStorageLayout`
// on its adapter feature only hid the root path from mixed builds.
pub use storage::{SolidlyStorageLayout, V3StorageLayout};
pub use storage_sync::{
    CALLDATA_SLOT_LOADER_CODE, StorageSyncEncoding, StorageSyncError, StorageSyncSnapshot,
    StorageSyncSpec, build_calldata_slot_loader_program, build_slot_loader_program,
    decode_storage_sync, run_and_inject_storage_sync, run_and_inject_storage_syncs,
    run_storage_sync, run_storage_syncs, slot_loader_calldata, storage_sync_spec_for_pool,
};
pub use sync_manager::{
    AmmEvictionPolicy, AmmEvictionReport, AmmLifecycleReport, AmmPendingRepair,
    AmmPoolGenerationReservation, AmmPoolRefreshReport, AmmPoolSubscriptionPlan,
    AmmPreparedPoolRefresh, AmmRemovedPool, AmmSyncBatchReport, AmmSyncChangeSource, AmmSyncEngine,
    AmmSyncError, AmmSyncIncident, AmmSyncPoolChange, AmmSyncPoolChangeKind,
};
pub use traits::AmmAdapter;
pub use types::{
    AdapterEvent, AdapterEventError, AdapterEventKind, AdapterEventReport, AdapterEventResult,
    BalancerTokenBalance, BalancerV2Metadata, ColdStartOutcome, ColdStartPolicy, ColdStartReport,
    CurveMetadata, CurveVariant, CustomPoolKey, DeferredOutcome, DeferredWork, EventRoute,
    EventSource, PoolKey, PoolRegistration, PoolStatus, ProtocolId, ProtocolMetadata, RepairAction,
    SolidlyV2Metadata, UniswapV2Metadata, UnsupportedReason, UpdateQuality, V3Metadata,
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
