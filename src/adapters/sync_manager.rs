//! AMM-owned live sync driver around the generic reactive runtime.
//!
//! `evm-fork-cache` owns the expensive part of resync execution: grouping
//! [`ResyncRequest`]s by block, resolving them from block traces when possible,
//! falling back to storage fetchers, and applying authoritative values to
//! [`EvmCache`]. This module keeps the AMM-specific part local to this crate:
//! callers get a runtime that always executes resyncs, and failed storage repairs
//! are translated back into pool lifecycle status.

use std::fmt;
use std::sync::Arc;

use alloy_network::Ethereum;
use alloy_primitives::{Address, U256};
use evm_fork_cache::cache::EvmCache;
use evm_fork_cache::reactive::{
    ReactiveBatchReport, ReactiveConfig, ReactiveError, ReactiveInputBatch, ReactiveReport,
    ReactiveRuntime, RegisterError, ResyncFailure, ResyncTarget,
};

use super::{
    AdapterRegistry, AmmReactiveHandler, PoolKey, PoolRegistration, PoolStatus, ProtocolId,
    ProtocolMetadata,
};

/// Error constructing or running [`AmmSyncEngine`].
#[derive(Debug)]
pub enum AmmSyncError {
    /// The AMM reactive handler could not be registered.
    Register(RegisterError),
    /// The underlying reactive ingest failed.
    Reactive(ReactiveError),
}

impl fmt::Display for AmmSyncError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Register(err) => write!(f, "failed to register AMM sync handler: {err}"),
            Self::Reactive(err) => write!(f, "AMM reactive ingest failed: {err}"),
        }
    }
}

impl std::error::Error for AmmSyncError {}

impl From<RegisterError> for AmmSyncError {
    fn from(err: RegisterError) -> Self {
        Self::Register(err)
    }
}

impl From<ReactiveError> for AmmSyncError {
    fn from(err: ReactiveError) -> Self {
        Self::Reactive(err)
    }
}

/// Summary returned by [`AmmSyncEngine::ingest_batch`].
#[derive(Clone, Debug)]
pub struct AmmSyncBatchReport {
    /// Full upstream reactive report, including applied effects and resync
    /// details.
    pub reactive: ReactiveBatchReport<Ethereum>,
    /// Pools marked degraded because at least one of their requested authoritative
    /// resync targets failed.
    pub degraded_pools: Vec<PoolKey>,
    /// Number of authoritative state updates produced by executed resync reports.
    pub resync_state_updates: usize,
    /// Number of failed resync targets reported by the runtime.
    pub resync_failures: usize,
}

/// Live AMM sync driver.
///
/// This is a convenience owner for the intended live path:
///
/// 1. route AMM logs through [`AmmReactiveHandler`],
/// 2. execute every emitted resync via
///    [`ReactiveRuntime::ingest_batch_with_resync`],
/// 3. keep AMM pool status in sync with resync failures.
pub struct AmmSyncEngine {
    registry: AdapterRegistry,
    runtime: ReactiveRuntime<Ethereum>,
    config: ReactiveConfig,
}

impl AmmSyncEngine {
    /// Build an engine with [`ReactiveConfig::default`].
    pub fn new(registry: AdapterRegistry) -> Result<Self, AmmSyncError> {
        Self::with_config(registry, ReactiveConfig::default())
    }

    /// Build an engine with an explicit reactive runtime config.
    pub fn with_config(
        registry: AdapterRegistry,
        config: ReactiveConfig,
    ) -> Result<Self, AmmSyncError> {
        let runtime = runtime_for(&registry, config.clone())?;
        Ok(Self {
            registry,
            runtime,
            config,
        })
    }

    /// Current pool registry.
    pub fn registry(&self) -> &AdapterRegistry {
        &self.registry
    }

    /// Current reactive runtime.
    pub fn runtime(&self) -> &ReactiveRuntime<Ethereum> {
        &self.runtime
    }

    /// Replace the registry and rebuild the underlying handler registration.
    ///
    /// `AmmReactiveHandler` owns a clone of the registry at registration time, so
    /// callers that add pools or update read-set metadata should replace the
    /// registry through this method rather than mutating a detached clone.
    pub fn replace_registry(&mut self, registry: AdapterRegistry) -> Result<(), AmmSyncError> {
        let runtime = runtime_for(&registry, self.config.clone())?;
        self.registry = registry;
        self.runtime = runtime;
        Ok(())
    }

    /// Ingest one batch and execute all emitted slot repairs.
    ///
    /// This deliberately calls `ingest_batch_with_resync`, not plain
    /// `ingest_batch`: Balancer, Curve, and V3 liquidity events rely on the
    /// resync phase to refresh slots whose final values are not carried in logs.
    pub fn ingest_batch(
        &mut self,
        cache: &mut EvmCache,
        batch: ReactiveInputBatch<Ethereum>,
    ) -> Result<AmmSyncBatchReport, AmmSyncError> {
        let reactive = self.runtime.ingest_batch_with_resync(cache, batch)?;
        let resync_state_updates = resync_state_update_count(&reactive);
        let resync_failures = resync_failure_count(&reactive);
        let degraded_pools = self.mark_failed_resync_pools(&reactive);

        Ok(AmmSyncBatchReport {
            reactive,
            degraded_pools,
            resync_state_updates,
            resync_failures,
        })
    }

    fn mark_failed_resync_pools(&mut self, report: &ReactiveBatchReport<Ethereum>) -> Vec<PoolKey> {
        let mut degraded = Vec::new();
        for failure in resync_failures(report) {
            for key in pools_for_failure(&self.registry, failure) {
                if !degraded.contains(&key) {
                    degraded.push(key.clone());
                }
                if let Some(pool) = self.registry.pool_mut(&key) {
                    pool.status = PoolStatus::Degraded;
                }
            }
        }
        degraded
    }
}

fn runtime_for(
    registry: &AdapterRegistry,
    config: ReactiveConfig,
) -> Result<ReactiveRuntime<Ethereum>, RegisterError> {
    let mut runtime = ReactiveRuntime::<Ethereum>::new(config);
    runtime.register_handler(Arc::new(AmmReactiveHandler::new(registry.clone())))?;
    Ok(runtime)
}

fn resync_state_update_count(report: &ReactiveBatchReport<Ethereum>) -> usize {
    report
        .reports
        .iter()
        .filter_map(|report| match report.as_ref() {
            ReactiveReport::Resynced(report) => Some(report.state_updates.len()),
            _ => None,
        })
        .sum()
}

fn resync_failure_count(report: &ReactiveBatchReport<Ethereum>) -> usize {
    resync_failures(report).count()
}

fn resync_failures(report: &ReactiveBatchReport<Ethereum>) -> impl Iterator<Item = &ResyncFailure> {
    report
        .reports
        .iter()
        .flat_map(|report| match report.as_ref() {
            ReactiveReport::Resynced(report) => report.failed.iter(),
            _ => [].iter(),
        })
}

fn pools_for_failure(registry: &AdapterRegistry, failure: &ResyncFailure) -> Vec<PoolKey> {
    registry
        .pools()
        .filter(|pool| pool_matches_resync_target(pool, &failure.target))
        .map(|pool| pool.key.clone())
        .collect()
}

fn pool_matches_resync_target(pool: &PoolRegistration, target: &ResyncTarget) -> bool {
    match target {
        ResyncTarget::StorageSlot { address, slot } => {
            pool_covers_storage_slot(pool, *address, *slot)
        }
        ResyncTarget::StorageSlots { address, slots } => slots
            .iter()
            .any(|slot| pool_covers_storage_slot(pool, *address, *slot)),
        ResyncTarget::Account { address, .. } => pool_owns_address(pool, *address),
    }
}

fn pool_covers_storage_slot(pool: &PoolRegistration, address: Address, slot: U256) -> bool {
    match pool.protocol() {
        ProtocolId::BalancerV2 => {
            let ProtocolMetadata::BalancerV2(metadata) = &pool.metadata else {
                return false;
            };
            metadata
                .vault
                .or_else(|| pool.state_addresses.first().copied())
                == Some(address)
                && metadata.balance_slots.contains(&slot)
        }
        ProtocolId::Curve => {
            let ProtocolMetadata::Curve(metadata) = &pool.metadata else {
                return false;
            };
            pool.key.address() == Some(address) && metadata.discovered_slots.contains(&slot)
        }
        ProtocolId::UniswapV3 | ProtocolId::PancakeV3 | ProtocolId::Slipstream => {
            pool.key.address() == Some(address)
        }
        ProtocolId::UniswapV2 | ProtocolId::SolidlyV2 => pool.key.address() == Some(address),
        #[cfg(feature = "experimental-protocols")]
        ProtocolId::BalancerV3 | ProtocolId::Erc4626 | ProtocolId::UniswapV4 => {
            pool_owns_address(pool, address)
        }
        ProtocolId::Custom(_) => pool_owns_address(pool, address),
    }
}

fn pool_owns_address(pool: &PoolRegistration, address: Address) -> bool {
    pool.key.address() == Some(address) || pool.state_addresses.contains(&address)
}
