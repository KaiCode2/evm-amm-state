//! Forked-state pool initialization and incremental synchronization.
//!
//! Each submodule loads and refreshes one AMM family from an [`evm_fork_cache::cache::EvmCache`]:
//! Uniswap V2 reserves, Uniswap/Pancake V3 slot0/liquidity/ticks, Balancer V2
//! and V3 balances, Curve reserves and parameters, Solidly V2 reserves, and
//! Slipstream concentrated-liquidity state. It also exposes the adaptive
//! bitmap-scan tuning, freshness checks, and the prefetch helpers used to warm
//! V3 tick storage in parallel before simulation.
//!
//! Most callers do not use these functions directly; they drive them through
//! the higher-level loaders in [`crate::configured_amms`].

mod adaptive_prefetch;
mod balancer_sync;
mod balancer_v3_sync;
mod curve_sync;
mod decode;
mod freshness;
mod slipstream_sync;
mod solidly_v2_sync;
mod v2_sync;
mod v3_bitmap;
mod v3_sync;

pub use balancer_sync::{init_balancer_from_cache, refresh_balancer_pool};
pub use balancer_v3_sync::{init_balancer_v3_from_cache, refresh_balancer_v3_pool};
pub use curve_sync::{init_curve_from_cache, refresh_curve_reserves};
pub use decode::{
    decode_v2_reserves_raw, decode_v3_slot0_raw, decode_v3_tick_info_raw, encode_v2_reserves_raw,
    encode_v3_slot0_patch,
};
pub use freshness::{
    PoolFreshnessResult, check_balancer_freshness, check_pools_freshness, check_v2_freshness,
    check_v3_freshness,
};
pub use slipstream_sync::init_slipstream_from_cache;
pub use solidly_v2_sync::{init_solidly_v2_from_cache, refresh_solidly_v2_reserves};
pub use v2_sync::{init_uniswap_v2_from_cache, refresh_uniswap_v2_reserves};
pub(crate) use v3_bitmap::compute_adaptive_scan_params;
pub use v3_bitmap::{V3Flavor, build_v3_factory_map, needs_tick_resync};
pub use v3_sync::{
    V3BitmapPrefetchTarget, V3InitPhase1Result, V3PrefetchStats, extend_v3_tick_region,
    incremental_sync_v3_ticks, init_pancakeswap_v3_from_cache, init_uniswap_v3_from_cache,
    init_v3_phase1, inject_v3_tick_data, prefetch_v3_bitmap_slots,
    prefetch_v3_incremental_resync_slots, prefetch_v3_tick_info_slots,
    refresh_pancakeswap_v3_state, refresh_uniswap_v3_slot0, refresh_uniswap_v3_state,
    save_v3_tick_snapshot, sync_uniswap_v3_ticks, sync_uniswap_v3_ticks_full, targeted_tick_resync,
    targeted_tick_resync_with_injected_slot0,
};

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use alloy_primitives::{Address, B256, U256};
use alloy_sol_types::{SolCall, sol};
use amms::amms::{
    Token,
    uniswap_v2::UniswapV2Pool,
    uniswap_v3::{Info, UniswapV3Pool},
};
use anyhow::{Result, anyhow};
use foundry_fork_db::backend::BlockingMode;
use futures::future::join_all;
use revm::database_interface::DatabaseRef;
use tracing::{debug, info, instrument, warn};

use crate::amm_wrapper::LocalAMM;
use crate::balancer_math::WeightedPool;
use crate::balancer_pool::BalancerPool;
use crate::data::PoolParams;
use crate::progress::{finish_with_message, progress_bar};
use crate::tuning::{SyncSpeedMode, sync_speed_mode};
use evm_fork_cache::cache::{
    BalancerPoolMetadata, EvmCache, PANCAKE_V3_LIQUIDITY_SLOT, PANCAKE_V3_TICK_BITMAP_BASE_SLOT,
    PANCAKE_V3_TICKS_BASE_SLOT, SLIPSTREAM_LIQUIDITY_SLOT, SLIPSTREAM_SLOT0_SLOT,
    SLIPSTREAM_TICK_BITMAP_BASE_SLOT, SLIPSTREAM_TICKS_BASE_SLOT, SlotObservationTracker,
    V2_RESERVES_SLOT, V2PoolMetadata, V3_LIQUIDITY_SLOT, V3_SLOT0_SLOT, V3_TICK_BITMAP_BASE_SLOT,
    V3_TICKS_BASE_SLOT, V3PoolMetadata, V3PoolTickSnapshot, v3_tick_bitmap_storage_key,
    v3_tick_bitmap_storage_key_with_base, v3_tick_info_storage_keys,
    v3_tick_info_storage_keys_with_base,
};
use evm_fork_cache::freshness::FreshnessParams;

/// Shared AMM reference type used by synchronization entry points.
pub type AMMRef = Arc<RwLock<LocalAMM>>;

pub(crate) const MIN_TICK: i32 = -887_272;
pub(crate) const MAX_TICK: i32 = 887_272;

/// Maximum tick drift allowed before invalidating the cache.
/// If the tick has moved more than this many ticks since the snapshot was taken,
/// the cache is considered potentially stale and should be re-verified.
/// 256 ticks = 1 word, so this allows movement within ~4 words before triggering re-validation.
pub(crate) const MAX_TICK_DRIFT_FOR_CACHE: i32 = 1024;

fn current_observation_time() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

/// Maximum number of words to scan in each direction from the current tick.
/// This is a safety limit to prevent runaway scans. With tick_spacing=1,
/// 500 words covers ~128,000 ticks in each direction.
/// Scaled by speed mode to control RPC consumption.
pub(crate) fn max_scan_radius() -> i32 {
    match sync_speed_mode() {
        SyncSpeedMode::Fast => 500,
        SyncSpeedMode::Normal => 350,
        SyncSpeedMode::Slow => 200,
        SyncSpeedMode::XSlow => 50,
    }
}

/// Maximum concurrent storage slot prefetch operations.
/// Scaled by speed mode to control RPC provider load.
pub(crate) fn max_concurrent_storage_prefetch() -> usize {
    match sync_speed_mode() {
        SyncSpeedMode::Fast => 30,
        SyncSpeedMode::Normal => 20,
        SyncSpeedMode::Slow => 12,
        SyncSpeedMode::XSlow => 4,
    }
}

/// Delay between prefetch chunks to avoid RPC rate limiting.
/// Scaled by speed mode to give providers more breathing room.
pub(crate) fn prefetch_inter_chunk_delay() -> std::time::Duration {
    match sync_speed_mode() {
        SyncSpeedMode::Fast => std::time::Duration::from_millis(15),
        SyncSpeedMode::Normal => std::time::Duration::from_millis(30),
        SyncSpeedMode::Slow => std::time::Duration::from_millis(50),
        SyncSpeedMode::XSlow => std::time::Duration::from_millis(200),
    }
}

/// Number of bitmap words around the current tick to refresh every cycle.
///
/// This is the per-cycle hot zone — smaller than the incremental resync hot zone
/// since it runs every cycle. Scaled by speed mode.
pub(crate) fn cycle_hot_zone_radius() -> i32 {
    match sync_speed_mode() {
        SyncSpeedMode::Fast => 4,
        SyncSpeedMode::Normal => 3,
        SyncSpeedMode::Slow => 2,
        SyncSpeedMode::XSlow => 1,
    }
}

/// Number of bitmap words around the current tick that are always re-fetched
/// during incremental resync, regardless of whether the bitmap changed.
///
/// This catches liquidity changes on already-initialized ticks (where the bitmap
/// bit stays 1 but liquidityGross/Net changed). Scaled by speed mode.
pub(crate) fn incremental_hot_zone_radius() -> i32 {
    match sync_speed_mode() {
        SyncSpeedMode::Fast => 4,
        SyncSpeedMode::Normal => 3,
        SyncSpeedMode::Slow => 2,
        SyncSpeedMode::XSlow => 1,
    }
}

/// Maximum drift (in basis points) before a Balancer pool is considered stale.
///
/// Balancer balances are stored internally as f64 and round-tripped through
/// `f64_to_u256_lossy`, which introduces precision noise of ~1e-15 relative error.
/// A threshold of 10 bps (0.1%) safely absorbs this noise while still catching
/// meaningful balance changes that could affect trade profitability.
pub(crate) const BALANCER_FRESHNESS_TOLERANCE_BPS: f64 = 10.0;

sol!(
    #[sol(rpc)]
    contract IBalancerPool {
        function getNormalizedWeights() external view returns (uint256[] memory);
        function getSwapFeePercentage() external view returns (uint256);
    }
);

sol!(
    #[sol(rpc)]
    contract IBalancerVault {
        function getPoolTokens(bytes32 poolId)
            external
            view
            returns (address[] memory tokens, uint256[] memory balances, uint256 lastChangeBlock);
    }
);

sol!(
    #[sol(rpc)]
    contract IUniswapV3Pool {
        function token0() external view returns (address);
        function token1() external view returns (address);
        function fee() external view returns (uint24);
        function tickSpacing() external view returns (int24);
        function slot0()
            external
            view
            returns (
                uint160 sqrtPriceX96,
                int24 tick,
                uint16 observationIndex,
                uint16 observationCardinality,
                uint16 observationCardinalityNext,
                uint8 feeProtocol,
                bool unlocked
            );
        function liquidity() external view returns (uint128);
        function tickBitmap(int16 wordPosition) external view returns (uint256);
        function ticks(int24 tick)
            external
            view
            returns (
                uint128 liquidityGross,
                int128 liquidityNet,
                uint256 feeGrowthOutside0X128,
                uint256 feeGrowthOutside1X128,
                int56 tickCumulativeOutside,
                uint160 secondsPerLiquidityOutsideX128,
                uint32 secondsOutside,
                bool initialized
            );
    }
);

sol!(
    #[sol(rpc)]
    contract IUniswapV2Pair {
        function token0() external view returns (address);
        function token1() external view returns (address);
        function getReserves() external view returns (uint112 reserve0, uint112 reserve1, uint32 blockTimestampLast);
    }
);

pub(crate) fn call_view<C: SolCall>(
    cache: &mut EvmCache,
    to: Address,
    call: C,
) -> Result<C::Return> {
    let result = cache.call_raw(Address::ZERO, to, call.abi_encode().into(), false)?;
    match result {
        revm::context::result::ExecutionResult::Success { output, .. } => {
            let out = output.into_data();
            let decoded = C::abi_decode_returns(&out)
                .map_err(|e| anyhow!("Failed to decode view call: {:?}", e))?;
            Ok(decoded)
        }
        other => Err(anyhow!("Call failed: {:?}", other)),
    }
}

/// Prefetch all accounts needed for a list of AMM addresses in parallel.
///
/// This is a convenience function that prefetches accounts without knowing
/// the AMM type. It's useful for generic address lists.
#[instrument(skip(cache, addresses), fields(address_count = addresses.len()))]
pub async fn prefetch_accounts(cache: &mut EvmCache, addresses: &[Address]) -> Result<()> {
    prefetch_accounts_parallel(cache, addresses, "account").await
}

/// Prefetch account data for multiple UniswapV2 pools in parallel.
///
/// This fetches the pool contracts so that subsequent `init_uniswap_v2_from_cache`
/// calls can hit the cache instead of making RPC calls.
#[instrument(skip(cache, addresses), fields(pool_count = addresses.len()))]
pub async fn prefetch_v2_pool_accounts(cache: &mut EvmCache, addresses: &[Address]) -> Result<()> {
    prefetch_accounts_parallel(cache, addresses, "V2 pool").await
}

/// Prefetch account data for multiple UniswapV3 pools in parallel.
///
/// This fetches the pool contracts so that subsequent `init_uniswap_v3_from_cache`
/// calls can hit the cache instead of making RPC calls.
#[instrument(skip(cache, addresses), fields(pool_count = addresses.len()))]
pub async fn prefetch_v3_pool_accounts(cache: &mut EvmCache, addresses: &[Address]) -> Result<()> {
    prefetch_accounts_parallel(cache, addresses, "V3 pool").await
}

/// Prefetch account data for multiple Balancer pools in parallel.
///
/// This fetches both the pool contract and vault contract for each pool.
#[instrument(skip(cache, pools), fields(pool_count = pools.len()))]
pub async fn prefetch_balancer_pool_accounts(
    cache: &mut EvmCache,
    pools: &[(B256, Address)], // (pool_id, vault_address)
) -> Result<()> {
    if pools.is_empty() {
        return Ok(());
    }

    // Collect all unique addresses to prefetch using HashSet for O(1) dedup
    let mut seen: HashSet<Address> = HashSet::with_capacity(pools.len() * 2);
    let mut addresses: Vec<Address> = Vec::with_capacity(pools.len() * 2);
    for (pool_id, vault) in pools {
        let pool_addr = BalancerPool::address(*pool_id);
        if seen.insert(pool_addr) {
            addresses.push(pool_addr);
        }
        if seen.insert(*vault) {
            addresses.push(*vault);
        }
    }

    prefetch_accounts_parallel(cache, &addresses, "Balancer pool").await
}

/// Internal parallel prefetch implementation.
///
/// Uses the SharedBackend's ability to be cloned and used from multiple tasks.
/// The backend uses channels internally, so cloning is cheap and all requests
/// go through the same background handler.
async fn prefetch_accounts_parallel(
    cache: &mut EvmCache,
    addresses: &[Address],
    label: &str,
) -> Result<()> {
    if addresses.is_empty() {
        return Ok(());
    }

    // Filter out addresses already in cache
    let addresses_to_fetch: Vec<Address> = addresses
        .iter()
        .filter(|addr| !cache.db_mut().cache.accounts.contains_key(*addr))
        .copied()
        .collect();

    if addresses_to_fetch.is_empty() {
        debug!(
            total = addresses.len(),
            "all {} accounts already cached", label
        );
        return Ok(());
    }

    // Use BlockingMode::Block so basic_ref calls can run on the blocking thread pool
    // without calling block_in_place (which would starve tokio worker threads).
    let backend = cache
        .unchecked_backend()
        .with_blocking_mode(BlockingMode::Block);

    let result = adaptive_prefetch::run_adaptive_prefetch(
        &addresses_to_fetch,
        adaptive_prefetch::AdaptivePrefetchConfig::throttle_aware(),
        &format!("Fetching {}", label),
        |addr, idx| {
            let backend = backend.clone();
            let addr = *addr;
            tokio::task::spawn_blocking(move || {
                backend.basic_ref(addr).map(|_| ()).map_err(|_| idx)
            })
        },
    )
    .await;

    debug!(
        success_count = result.success_count,
        error_count = result.error_count,
        retry_rounds = result.retry_rounds,
        total = addresses.len(),
        cached = addresses.len() - addresses_to_fetch.len(),
        "prefetched {} accounts in parallel",
        label
    );

    Ok(())
}

// ============================================================================
// Parallel Pool State Refresh
// ============================================================================
//
// Parallel alternative to a sequential per-pool refresh. Uses cloned
// SharedBackend instances for parallel raw storage reads, keeping serial
// phases only for fast in-memory operations (purging, decoding, pool updates).

/// Work item for a V2 pool during parallel refresh.
struct V2PoolWork {
    address: Address,
    pool: UniswapV2Pool,
}

/// Work item for a V3 pool during parallel refresh.
struct V3PoolWork {
    address: Address,
    pool: UniswapV3Pool,
    hot_zone_words: Vec<i16>,
    flavor: V3Flavor,
}

/// Work item for a Balancer pool during parallel refresh.
struct BalancerPoolWork {
    address: Address,
    pool: BalancerPool,
}

/// Work item for a Balancer V3 pool during sequential refresh.
struct BalancerV3PoolWork {
    address: Address,
    pool: crate::balancer_v3_pool::BalancerV3Pool,
}

/// Work item for a Curve pool during sequential refresh.
struct CurvePoolWork {
    address: Address,
    pool: crate::curve_pool::CurvePool,
}

/// Refresh all pool state (V2 reserves + V3 slot0/ticks) using parallel raw storage reads.
///
/// This is functionally equivalent to refreshing each pool sequentially, but
/// fetches storage slots in parallel via cloned `SharedBackend` instances, which
/// dramatically reduces wall-clock time for large pool sets.
#[allow(clippy::needless_late_init)]
pub async fn sync_all_pool_state_parallel(
    cache: &mut EvmCache,
    amms: &mut HashMap<Address, AMMRef>,
) -> Result<()> {
    let fn_start = Instant::now();
    let phase_classify_ms: u64;
    let phase_purge_ms: u64;
    let phase_fetch_ms: u64;
    let phase_decode_ms: u64;
    let phase_bitmap_ms: u64;
    let phase_tick_ms: u64;
    let phase_writeback_ms: u64;

    // -- Phase 0: Classify and extract pool data --
    let mut v2_work: Vec<V2PoolWork> = Vec::new();
    let mut v3_work: Vec<V3PoolWork> = Vec::new();
    let mut balancer_work: Vec<BalancerPoolWork> = Vec::new();
    let mut balancer_v3_work: Vec<BalancerV3PoolWork> = Vec::new();
    let mut curve_work: Vec<CurvePoolWork> = Vec::new();

    for (addr, amm_ref) in amms.iter() {
        let guard = match amm_ref.read() {
            Ok(g) => g,
            Err(_) => continue,
        };
        match &*guard {
            LocalAMM::UniswapV2(p) => {
                v2_work.push(V2PoolWork {
                    address: *addr,
                    pool: p.clone(),
                });
            }
            LocalAMM::UniswapV3(p) | LocalAMM::PancakeSwapV3(p) => {
                let flavor = if matches!(&*guard, LocalAMM::PancakeSwapV3(_)) {
                    V3Flavor::PancakeSwapV3
                } else {
                    V3Flavor::UniswapV3
                };
                if p.tick_spacing == 0 || p.tick_bitmap.is_empty() {
                    v3_work.push(V3PoolWork {
                        address: *addr,
                        pool: p.clone(),
                        hot_zone_words: Vec::new(),
                        flavor,
                    });
                } else {
                    let current_word = v3_bitmap::tick_to_word(p.tick, p.tick_spacing);
                    let hot_zone_words: Vec<i16> = ((-cycle_hot_zone_radius())
                        ..=cycle_hot_zone_radius())
                        .map(|offset| (current_word + offset) as i16)
                        .collect();
                    v3_work.push(V3PoolWork {
                        address: *addr,
                        pool: p.clone(),
                        hot_zone_words,
                        flavor,
                    });
                }
            }
            LocalAMM::SolidlyV2(_) => {
                // SolidlyV2 refreshed via getReserves() call after parallel phase
            }
            LocalAMM::Slipstream(p) => {
                let v3 = p.as_v3_pool();
                let flavor = V3Flavor::Slipstream;
                if v3.tick_spacing == 0 || v3.tick_bitmap.is_empty() {
                    v3_work.push(V3PoolWork {
                        address: *addr,
                        pool: v3,
                        hot_zone_words: Vec::new(),
                        flavor,
                    });
                } else {
                    let current_word = v3_bitmap::tick_to_word(v3.tick, v3.tick_spacing);
                    let hot_zone_words: Vec<i16> = ((-cycle_hot_zone_radius())
                        ..=cycle_hot_zone_radius())
                        .map(|offset| (current_word + offset) as i16)
                        .collect();
                    v3_work.push(V3PoolWork {
                        address: *addr,
                        pool: v3,
                        hot_zone_words,
                        flavor,
                    });
                }
            }
            LocalAMM::Balancer(p) => {
                balancer_work.push(BalancerPoolWork {
                    address: *addr,
                    pool: p.clone(),
                });
            }
            LocalAMM::BalancerV3(p) => {
                balancer_v3_work.push(BalancerV3PoolWork {
                    address: *addr,
                    pool: p.clone(),
                });
            }
            LocalAMM::Curve(p) => {
                curve_work.push(CurvePoolWork {
                    address: *addr,
                    pool: p.clone(),
                });
            }
            _ => {} // ERC4626, UniswapV4 stubs not refreshed per-cycle
        }
    }

    let total_pools = v2_work.len()
        + v3_work.len()
        + balancer_work.len()
        + balancer_v3_work.len()
        + curve_work.len();
    if total_pools == 0 {
        return Ok(());
    }

    let pb = progress_bar(total_pools as u64, "Refreshing pool state (parallel)");
    phase_classify_ms = fn_start.elapsed().as_millis() as u64;

    // -- Phase 1: Batch purge mutable slots --
    let phase1_start = Instant::now();
    for w in &v2_work {
        cache.purge_pool_storage(w.address);
    }
    for w in &v3_work {
        // Refresh slot0/liquidity first and only fetch hot-zone bitmap words for
        // pools whose active liquidity actually changed.
        cache.purge_pool_slots(w.address, &[V3_SLOT0_SLOT, w.flavor.liquidity_slot()]);
    }
    // Purge Balancer vault storage (deduplicated by vault address — covers both V2 and V3 vaults)
    {
        let mut purged_vaults: HashSet<Address> = HashSet::new();
        for w in &balancer_work {
            if purged_vaults.insert(w.pool.vault) {
                cache.purge_pool_storage(w.pool.vault);
            }
        }
        for w in &balancer_v3_work {
            if purged_vaults.insert(w.pool.vault) {
                cache.purge_pool_storage(w.pool.vault);
            }
        }
    }

    phase_purge_ms = phase1_start.elapsed().as_millis() as u64;

    // -- Phase 2: Parallel fetch via cloned backend --
    let phase2_start = Instant::now();
    let backend = cache.unchecked_backend().clone();

    // Tag to identify what each fetched slot represents
    #[derive(Clone, Copy)]
    enum SlotTag {
        V2Reserves(usize), // index into v2_work
        V3Slot0(usize),    // index into v3_work
        V3Liquidity(usize),
    }

    let mut fetch_requests: Vec<(Address, U256, SlotTag)> = Vec::new();

    for (i, w) in v2_work.iter().enumerate() {
        fetch_requests.push((w.address, V2_RESERVES_SLOT, SlotTag::V2Reserves(i)));
    }
    for (i, w) in v3_work.iter().enumerate() {
        fetch_requests.push((w.address, V3_SLOT0_SLOT, SlotTag::V3Slot0(i)));
        fetch_requests.push((
            w.address,
            w.flavor.liquidity_slot(),
            SlotTag::V3Liquidity(i),
        ));
    }

    // Fetch all slots in parallel with adaptive rate limiting
    let mut fetch_results: Vec<Option<U256>> = vec![None; fetch_requests.len()];
    let mut fetch_errors: HashSet<usize> = HashSet::new(); // pool work indices with errors

    let mut adaptive = adaptive_prefetch::AdaptiveState::new(
        adaptive_prefetch::AdaptivePrefetchConfig::throttle_aware(),
    );
    let mut offset = 0;
    while offset < fetch_requests.len() {
        let chunk_end = (offset + adaptive.current_chunk_size).min(fetch_requests.len());
        let chunk = &fetch_requests[offset..chunk_end];

        let futures: Vec<_> = chunk
            .iter()
            .enumerate()
            .map(|(local_idx, (addr, slot, _tag))| {
                let backend = backend.clone();
                let addr = *addr;
                let slot = *slot;
                let global_idx = offset + local_idx;
                async move { (global_idx, backend.storage_ref(addr, slot)) }
            })
            .collect();

        let results = join_all(futures).await;
        let chunk_total = results.len();
        let mut chunk_failures = 0;
        for (global_idx, result) in results {
            match result {
                Ok(val) => {
                    fetch_results[global_idx] = Some(val);
                }
                Err(e) => {
                    let (addr, _slot, tag) = &fetch_requests[global_idx];
                    let pool_idx = match tag {
                        SlotTag::V2Reserves(i) => *i + v3_work.len(), // won't collide
                        SlotTag::V3Slot0(i) | SlotTag::V3Liquidity(i) => *i,
                    };
                    warn!(%addr, error = ?e, "parallel fetch failed for pool slot");
                    fetch_errors.insert(pool_idx);
                    chunk_failures += 1;
                }
            }
        }
        adaptive.adjust_after_chunk(chunk_total, chunk_failures);
        tokio::time::sleep(adaptive.current_delay).await;
        offset = chunk_end;
    }

    phase_fetch_ms = phase2_start.elapsed().as_millis() as u64;

    // -- Phase 3: Decode and compare --
    let phase3_start = Instant::now();
    let mut v2_refreshed = 0u32;
    let mut v3_refreshed = 0u32;
    let mut full_resyncs = 0u32;
    let mut v2_fallback_addrs: Vec<Address> = Vec::new();
    let mut v3_fallback_addrs: Vec<Address> = Vec::new();

    // Decode V2
    for (req_idx, (_addr, _slot, tag)) in fetch_requests.iter().enumerate() {
        if let SlotTag::V2Reserves(wi) = tag {
            let w = &mut v2_work[*wi];
            if let Some(raw) = fetch_results[req_idx] {
                let (r0, r1) = decode_v2_reserves_raw(raw);
                if w.pool.reserve_0 != r0 || w.pool.reserve_1 != r1 {
                    debug!(
                        pool = %w.address,
                        old_r0 = w.pool.reserve_0,
                        new_r0 = r0,
                        old_r1 = w.pool.reserve_1,
                        new_r1 = r1,
                        "V2 reserves changed (parallel)"
                    );
                }
                w.pool.reserve_0 = r0;
                w.pool.reserve_1 = r1;
                v2_refreshed += 1;
            } else {
                v2_fallback_addrs.push(w.address);
            }
            pb.inc(1);
        }
    }

    // Decode V3 slot0 + liquidity first. Hot-zone bitmap/tick refresh now only
    // runs for pools whose active liquidity changed.
    let mut v3_slot0_raw: Vec<Option<U256>> = vec![None; v3_work.len()];
    let mut v3_liquidity_raw: Vec<Option<U256>> = vec![None; v3_work.len()];

    for (req_idx, (_addr, _slot, tag)) in fetch_requests.iter().enumerate() {
        match tag {
            SlotTag::V3Slot0(wi) => {
                v3_slot0_raw[*wi] = fetch_results[req_idx];
            }
            SlotTag::V3Liquidity(wi) => {
                v3_liquidity_raw[*wi] = fetch_results[req_idx];
            }
            _ => {}
        }
    }

    let mut v3_bitmap_refresh_requests: Vec<(Address, U256, usize, i16)> = Vec::new();
    let mut refreshed_words_per_pool: Vec<HashSet<i16>> = vec![HashSet::new(); v3_work.len()];
    let mut ticks_to_fetch_per_pool: Vec<Vec<i32>> = vec![Vec::new(); v3_work.len()];
    let mut v3_liquidity_fast_paths = 0u32;

    for (wi, w) in v3_work.iter_mut().enumerate() {
        // Check if this pool had any fetch errors
        if fetch_errors.contains(&wi) {
            v3_fallback_addrs.push(w.address);
            pb.inc(1);
            continue;
        }

        // Decode slot0
        if let Some(raw) = v3_slot0_raw[wi] {
            let (sqrt_price, tick) = decode_v3_slot0_raw(raw);
            let old_tick = w.pool.tick;
            w.pool.sqrt_price = sqrt_price;
            w.pool.tick = tick;

            debug!(
                pool = %w.address,
                old_tick,
                new_tick = tick,
                "refreshed V3 slot0 (parallel)"
            );
        } else {
            v3_fallback_addrs.push(w.address);
            pb.inc(1);
            continue;
        }

        // Decode liquidity
        if let Some(raw) = v3_liquidity_raw[wi] {
            let old_liquidity = w.pool.liquidity;
            // liquidity is uint128, stored in lower 128 bits
            let limbs = raw.as_limbs();
            let fresh_liquidity = limbs[0] as u128 | ((limbs[1] as u128) << 64);
            w.pool.liquidity = fresh_liquidity;

            if w.hot_zone_words.is_empty() {
                continue;
            }

            if old_liquidity == fresh_liquidity {
                v3_liquidity_fast_paths += 1;
                continue;
            }

            for &word in &w.hot_zone_words {
                refreshed_words_per_pool[wi].insert(word);
                let slot = w.flavor.tick_bitmap_key(word);
                cache.purge_pool_slots(w.address, &[slot]);
                v3_bitmap_refresh_requests.push((w.address, slot, wi, word));
            }
        } else {
            v3_fallback_addrs.push(w.address);
            pb.inc(1);
            continue;
        }
    }

    phase_decode_ms = phase3_start.elapsed().as_millis() as u64;

    // -- Phase 4: Parallel hot-zone bitmap fetch for liquidity-changed V3 pools --
    let phase4_start = Instant::now();
    let mut v3_bitmap_fresh: Vec<HashMap<i16, U256>> = vec![HashMap::new(); v3_work.len()];

    if !v3_bitmap_refresh_requests.is_empty() {
        let backend = cache.unchecked_backend().clone();
        let mut adaptive = adaptive_prefetch::AdaptiveState::new(
            adaptive_prefetch::AdaptivePrefetchConfig::throttle_aware(),
        );
        let mut offset = 0;
        while offset < v3_bitmap_refresh_requests.len() {
            let chunk_end =
                (offset + adaptive.current_chunk_size).min(v3_bitmap_refresh_requests.len());
            let chunk = &v3_bitmap_refresh_requests[offset..chunk_end];

            let futures: Vec<_> = chunk
                .iter()
                .enumerate()
                .map(|(local_idx, (addr, slot, _wi, _word))| {
                    let backend = backend.clone();
                    let addr = *addr;
                    let slot = *slot;
                    let global_idx = offset + local_idx;
                    async move { (global_idx, backend.storage_ref(addr, slot)) }
                })
                .collect();

            let results = join_all(futures).await;
            let chunk_total = results.len();
            let mut chunk_failures = 0;
            for (global_idx, result) in results {
                match result {
                    Ok(val) => {
                        let (_addr, _slot, wi, word) = v3_bitmap_refresh_requests[global_idx];
                        v3_bitmap_fresh[wi].insert(word, val);
                    }
                    Err(e) => {
                        let (addr, _slot, wi, _word) = v3_bitmap_refresh_requests[global_idx];
                        warn!(%addr, error = ?e, "parallel fetch failed for V3 bitmap slot");
                        fetch_errors.insert(wi);
                        chunk_failures += 1;
                    }
                }
            }
            adaptive.adjust_after_chunk(chunk_total, chunk_failures);
            tokio::time::sleep(adaptive.current_delay).await;
            offset = chunk_end;
        }
    }

    for (wi, w) in v3_work.iter_mut().enumerate() {
        if fetch_errors.contains(&wi) {
            if !v3_fallback_addrs.contains(&w.address) {
                v3_fallback_addrs.push(w.address);
            }
            continue;
        }

        for &word in &refreshed_words_per_pool[wi] {
            let fresh_bitmap = v3_bitmap_fresh[wi]
                .get(&word)
                .copied()
                .unwrap_or(U256::ZERO);
            let old_bitmap = w.pool.tick_bitmap.get(&word).copied().unwrap_or(U256::ZERO);
            w.pool.tick_bitmap.insert(word, fresh_bitmap);

            if fresh_bitmap != U256::ZERO {
                let word_ticks = v3_bitmap::extract_ticks_from_bitmap_word(
                    word as i32,
                    fresh_bitmap,
                    w.pool.tick_spacing,
                );
                ticks_to_fetch_per_pool[wi].extend_from_slice(&word_ticks);
            }

            if fresh_bitmap != old_bitmap {
                debug!(
                    pool = %w.address,
                    word,
                    "V3 bitmap changed in refreshed hot zone"
                );
            }
        }
    }

    phase_bitmap_ms = phase4_start.elapsed().as_millis() as u64;

    // -- Phase 5: Parallel tick info fetch --
    let phase5_start = Instant::now();
    // Purge tick info slots for ticks that need re-fetching
    for (wi, ticks) in ticks_to_fetch_per_pool.iter().enumerate() {
        if ticks.is_empty() {
            continue;
        }
        let flavor = &v3_work[wi].flavor;
        let slots_to_purge: Vec<U256> = ticks
            .iter()
            .flat_map(|&tick| {
                let keys = flavor.tick_info_keys(tick);
                [keys[0], keys[3]] // Only slots 0 and 3
            })
            .collect();
        cache.purge_pool_slots(v3_work[wi].address, &slots_to_purge);
    }

    // Build flat list of tick info fetch requests
    #[derive(Clone, Copy)]
    enum TickSlotTag {
        Slot0(usize, i32), // (v3_work index, tick)
        Slot3(usize, i32),
    }

    let mut tick_fetch_requests: Vec<(Address, U256, TickSlotTag)> = Vec::new();
    for (wi, ticks) in ticks_to_fetch_per_pool.iter().enumerate() {
        let flavor = &v3_work[wi].flavor;
        for &tick in ticks {
            let keys = flavor.tick_info_keys(tick);
            tick_fetch_requests.push((v3_work[wi].address, keys[0], TickSlotTag::Slot0(wi, tick)));
            tick_fetch_requests.push((v3_work[wi].address, keys[3], TickSlotTag::Slot3(wi, tick)));
        }
    }

    // Fetch tick info in parallel
    let mut tick_slot0_results: HashMap<(usize, i32), U256> = HashMap::new();
    let mut tick_slot3_results: HashMap<(usize, i32), U256> = HashMap::new();

    if !tick_fetch_requests.is_empty() {
        let backend = cache.unchecked_backend().clone();

        let mut adaptive = adaptive_prefetch::AdaptiveState::new(
            adaptive_prefetch::AdaptivePrefetchConfig::throttle_aware(),
        );
        let mut offset = 0;
        while offset < tick_fetch_requests.len() {
            let chunk_end = (offset + adaptive.current_chunk_size).min(tick_fetch_requests.len());
            let chunk = &tick_fetch_requests[offset..chunk_end];

            let futures: Vec<_> = chunk
                .iter()
                .enumerate()
                .map(|(local_idx, (addr, slot, _tag))| {
                    let backend = backend.clone();
                    let addr = *addr;
                    let slot = *slot;
                    let global_idx = offset + local_idx;
                    async move { (global_idx, backend.storage_ref(addr, slot)) }
                })
                .collect();

            let results = join_all(futures).await;
            let chunk_total = results.len();
            let mut chunk_failures = 0;
            for (global_idx, result) in results {
                match result {
                    Ok(val) => {
                        let (_addr, _slot, tag) = &tick_fetch_requests[global_idx];
                        match tag {
                            TickSlotTag::Slot0(wi, tick) => {
                                tick_slot0_results.insert((*wi, *tick), val);
                            }
                            TickSlotTag::Slot3(wi, tick) => {
                                tick_slot3_results.insert((*wi, *tick), val);
                            }
                        }
                    }
                    Err(_) => {
                        chunk_failures += 1;
                    }
                }
            }
            adaptive.adjust_after_chunk(chunk_total, chunk_failures);
            tokio::time::sleep(adaptive.current_delay).await;
            offset = chunk_end;
        }
    }

    phase_tick_ms = phase5_start.elapsed().as_millis() as u64;

    // -- Phase 6: Update pools and handle edge cases --
    let phase6_start = Instant::now();
    for (wi, ticks) in ticks_to_fetch_per_pool.iter().enumerate() {
        for &tick in ticks {
            if let (Some(&s0), Some(&s3)) = (
                tick_slot0_results.get(&(wi, tick)),
                tick_slot3_results.get(&(wi, tick)),
            ) {
                let info = decode_v3_tick_info_raw(s0, s3);
                v3_work[wi].pool.ticks.insert(tick, info);
            }
        }

        if refreshed_words_per_pool[wi].is_empty() {
            continue;
        }

        let initialized_in_refreshed: HashSet<i32> = ticks.iter().copied().collect();
        let tick_spacing = v3_work[wi].pool.tick_spacing;
        let refreshed_words = &refreshed_words_per_pool[wi];
        v3_work[wi].pool.ticks.retain(|&tick, _| {
            let tick_word = v3_bitmap::tick_to_word(tick, tick_spacing) as i16;
            !refreshed_words.contains(&tick_word) || initialized_in_refreshed.contains(&tick)
        });
    }

    // Save snapshots and check for full resyncs
    for w in v3_work.iter_mut() {
        if v3_fallback_addrs.contains(&w.address) {
            continue;
        }
        if !w.hot_zone_words.is_empty() {
            save_v3_tick_snapshot(cache, &w.pool);
        }

        // Check if tick moved to an unknown word
        if w.pool.tick_bitmap.is_empty() {
            // No tick data at all — need full scan (cold start edge case)
            sync_uniswap_v3_ticks(cache, &mut w.pool, w.flavor)?;
            full_resyncs += 1;
            debug!(
                pool = %w.address,
                new_tick = w.pool.tick,
                "performed full tick resync (no bitmap data)"
            );
        } else if needs_tick_resync(&w.pool, w.pool.tick) {
            // Tick moved to an unknown word — extend lazily instead of full rescan
            extend_v3_tick_region(cache, &mut w.pool, w.flavor)?;
            debug!(
                pool = %w.address,
                new_tick = w.pool.tick,
                "extended tick region lazily (parallel path)"
            );
        }
        v3_refreshed += 1;
        pb.inc(1);
    }

    // -- Fallback: sequential refresh for pools that failed --
    for addr in &v2_fallback_addrs {
        if let Some(amm_ref) = amms.get(addr) {
            let mut pool = {
                let guard = amm_ref.read().map_err(|_| anyhow!("AMM lock poisoned"))?;
                match &*guard {
                    LocalAMM::UniswapV2(p) => p.clone(),
                    _ => continue,
                }
            };
            if let Err(e) = refresh_uniswap_v2_reserves(cache, &mut pool) {
                warn!(%addr, error = ?e, "V2 fallback refresh also failed");
            } else {
                let mut guard = amm_ref.write().map_err(|_| anyhow!("AMM lock poisoned"))?;
                *guard = LocalAMM::UniswapV2(pool);
                v2_refreshed += 1;
            }
            pb.inc(1);
        }
    }

    for addr in &v3_fallback_addrs {
        if let Some(amm_ref) = amms.get(addr) {
            let (mut pool, is_pancake) = {
                let guard = amm_ref.read().map_err(|_| anyhow!("AMM lock poisoned"))?;
                match &*guard {
                    LocalAMM::UniswapV3(p) => (p.clone(), false),
                    LocalAMM::PancakeSwapV3(p) => (p.clone(), true),
                    _ => continue,
                }
            };
            let fallback_flavor = if is_pancake {
                V3Flavor::PancakeSwapV3
            } else {
                V3Flavor::UniswapV3
            };
            if let Err(e) = v3_sync::refresh_v3_state(cache, &mut pool, fallback_flavor) {
                warn!(%addr, error = ?e, "V3 fallback refresh also failed");
            } else {
                if pool.tick_bitmap.is_empty() {
                    sync_uniswap_v3_ticks(cache, &mut pool, fallback_flavor)?;
                    full_resyncs += 1;
                } else if needs_tick_resync(&pool, pool.tick) {
                    extend_v3_tick_region(cache, &mut pool, fallback_flavor)?;
                }
                let mut guard = amm_ref.write().map_err(|_| anyhow!("AMM lock poisoned"))?;
                *guard = if is_pancake {
                    LocalAMM::PancakeSwapV3(pool)
                } else {
                    LocalAMM::UniswapV3(pool)
                };
                v3_refreshed += 1;
            }
            pb.inc(1);
        }
    }

    // -- Balancer: Sequential refresh (requires EVM call_view) --
    let mut balancer_refreshed = 0u32;
    for w in &mut balancer_work {
        if let Err(e) = refresh_balancer_pool(cache, &mut w.pool) {
            warn!(pool = %w.address, error = ?e, "Balancer refresh failed");
        } else {
            balancer_refreshed += 1;
        }
        pb.inc(1);
    }

    // -- Write updated pools back to AMMRef --
    for w in v2_work {
        if v2_fallback_addrs.contains(&w.address) {
            continue; // Already handled in fallback
        }
        if let Some(amm_ref) = amms.get(&w.address) {
            let mut guard = amm_ref
                .write()
                .map_err(|_| anyhow!("AMM lock poisoned for {}", w.address))?;
            *guard = LocalAMM::UniswapV2(w.pool);
        }
    }
    for w in v3_work {
        if v3_fallback_addrs.contains(&w.address) {
            continue; // Already handled in fallback
        }
        if let Some(amm_ref) = amms.get(&w.address) {
            let mut guard = amm_ref
                .write()
                .map_err(|_| anyhow!("AMM lock poisoned for {}", w.address))?;
            *guard = match w.flavor {
                V3Flavor::UniswapV3 => LocalAMM::UniswapV3(w.pool),
                V3Flavor::PancakeSwapV3 => LocalAMM::PancakeSwapV3(w.pool),
                V3Flavor::Slipstream => {
                    // Preserve the original SlipstreamPool and update V3 state
                    if let LocalAMM::Slipstream(ref mut slip) = *guard {
                        slip.apply_v3_state(&w.pool);
                    }
                    continue;
                }
            };
        }
    }
    for w in balancer_work {
        if let Some(amm_ref) = amms.get(&w.address) {
            let mut guard = amm_ref
                .write()
                .map_err(|_| anyhow!("AMM lock poisoned for {}", w.address))?;
            *guard = LocalAMM::Balancer(w.pool);
        }
    }

    // -- BalancerV3: Sequential refresh (vault already purged above) --
    let mut balancer_v3_refreshed = 0u32;
    for w in &mut balancer_v3_work {
        if let Err(e) = balancer_v3_sync::refresh_balancer_v3_pool(cache, &mut w.pool) {
            warn!(pool = %w.address, error = ?e, "BalancerV3 refresh failed");
        } else {
            balancer_v3_refreshed += 1;
        }
        pb.inc(1);
    }
    for w in balancer_v3_work {
        if let Some(amm_ref) = amms.get(&w.address) {
            let mut guard = amm_ref
                .write()
                .map_err(|_| anyhow!("AMM lock poisoned for {}", w.address))?;
            *guard = LocalAMM::BalancerV3(w.pool);
        }
    }

    // -- Curve: Sequential refresh via balances(i) view calls --
    let mut curve_refreshed = 0u32;
    for w in &mut curve_work {
        if let Err(e) = curve_sync::refresh_curve_reserves(cache, &mut w.pool) {
            warn!(pool = %w.address, error = ?e, "Curve refresh failed");
        } else {
            curve_refreshed += 1;
        }
        pb.inc(1);
    }
    for w in curve_work {
        if let Some(amm_ref) = amms.get(&w.address) {
            let mut guard = amm_ref
                .write()
                .map_err(|_| anyhow!("AMM lock poisoned for {}", w.address))?;
            *guard = LocalAMM::Curve(w.pool);
        }
    }

    // -- SolidlyV2: refresh reserves via getReserves() view call --
    for (addr, amm_ref) in amms.iter() {
        let is_solidly = {
            let guard = amm_ref.read().map_err(|_| anyhow!("AMM lock poisoned"))?;
            matches!(&*guard, LocalAMM::SolidlyV2(_))
        };
        if is_solidly {
            let mut guard = amm_ref
                .write()
                .map_err(|_| anyhow!("AMM lock poisoned for {}", addr))?;
            if let LocalAMM::SolidlyV2(ref mut pool) = *guard {
                if let Err(e) = solidly_v2_sync::refresh_solidly_v2_reserves(cache, pool) {
                    warn!("Failed to refresh SolidlyV2 pool {}: {:?}", addr, e);
                } else {
                    v2_refreshed += 1;
                }
            }
        }
    }

    finish_with_message(
        &pb,
        &format!(
            "V2: {} refreshed, V3: {} refreshed ({} fast path, {} full resyncs), Bal: {}, BalV3: {}, Curve: {}",
            v2_refreshed,
            v3_refreshed,
            v3_liquidity_fast_paths,
            full_resyncs,
            balancer_refreshed,
            balancer_v3_refreshed,
            curve_refreshed
        ),
    );

    phase_writeback_ms = phase6_start.elapsed().as_millis() as u64;

    let total_ms = fn_start.elapsed().as_millis();
    tracing::info!(
        v2_refreshed,
        v3_refreshed,
        v3_liquidity_fast_paths,
        balancer_refreshed,
        balancer_v3_refreshed,
        curve_refreshed,
        full_resyncs,
        total_ms,
        phase_classify_ms,
        phase_purge_ms,
        phase_fetch_ms,
        phase_decode_ms,
        phase_bitmap_ms,
        phase_tick_ms,
        phase_writeback_ms,
        "Parallel pool state refresh complete"
    );

    Ok(())
}

/// Purge type-specific hot storage slots for the given pool addresses.
///
/// This selectively purges only the slots that change between blocks:
/// - V2 pools: reserves slot only
/// - V3 pools: slot0 + liquidity (preserves expensive tick data)
/// - PancakeV3: slot0 + PancakeSwap liquidity slot
/// - Other/unknown: full storage purge
///
/// Returns the number of pools purged.
pub fn purge_amm_hot_slots(
    cache: &mut EvmCache,
    amms: &HashMap<Address, AMMRef>,
    pool_addresses: &[Address],
) -> usize {
    let mut purged_count = 0usize;
    for addr in pool_addresses {
        if let Some(amm_ref) = amms.get(addr) {
            let guard = amm_ref.read().expect("AMM lock poisoned during purge");
            match &*guard {
                LocalAMM::UniswapV2(_) => {
                    cache.purge_pool_slots(*addr, &[V2_RESERVES_SLOT]);
                }
                LocalAMM::UniswapV3(_) => {
                    cache.purge_pool_slots(*addr, &[V3_SLOT0_SLOT, V3_LIQUIDITY_SLOT]);
                }
                LocalAMM::PancakeSwapV3(_) => {
                    cache.purge_pool_slots(*addr, &[V3_SLOT0_SLOT, PANCAKE_V3_LIQUIDITY_SLOT]);
                }
                LocalAMM::SolidlyV2(_) => {
                    cache.purge_pool_storage(*addr);
                }
                LocalAMM::Slipstream(_) => {
                    cache.purge_pool_slots(
                        *addr,
                        &[SLIPSTREAM_SLOT0_SLOT, SLIPSTREAM_LIQUIDITY_SLOT],
                    );
                }
                _ => {
                    cache.purge_pool_storage(*addr);
                }
            }
            drop(guard);
        } else {
            // Pool not in amms map — purge full storage so lazy RPC
            // fetch uses the current pinned block
            cache.purge_pool_storage(*addr);
        }
        purged_count += 1;
    }
    purged_count
}

/// Result of hot-state EVM cache injection.
pub struct HotInjectionResult {
    /// Number of V2 pools with reserves injected directly (zero RPC).
    pub v2_injected: usize,
    /// Number of V3 pools with slot0+liquidity injected directly (zero RPC).
    pub v3_injected: usize,
    /// V3 pools that need tick resync due to liquidity changes.
    pub v3_needs_tick_resync: Vec<(Address, TickResyncReason)>,
    /// Pools where injection failed (fallback to RPC sync).
    pub injection_failures: Vec<Address>,
}

/// Reason a V3 pool needs tick resync after hot-state injection.
pub enum TickResyncReason {
    /// Mint/Burn events detected — resync only the affected tick ranges.
    KnownRanges(Vec<(i32, i32)>),
    /// Liquidity changed but no Mint/Burn events captured (cold cache or WS gap).
    /// Use `incremental_sync_v3_ticks()` with adaptive scan params.
    UnknownRanges,
}

/// Inject hot-state values directly into the EVM cache for pools with fresh WS data.
///
/// This **replaces** `purge_amm_hot_slots()` for the hot-state sync path. Instead of
/// purging known values and letting revm lazy-fetch them via RPC, we write the values
/// we already know from WebSocket events directly into the CacheDB layer.
///
/// - **V2 pools**: Pack reserves into slot 8 and inject (zero RPC).
/// - **V3 pools**: Read-modify-write slot0 (patch sqrtPriceX96+tick, preserve observation
///   fields), inject liquidity into slot 4/5 (zero RPC).
///
/// The `pending_tick_ranges` parameter maps V3 pool addresses to their pending Mint/Burn
/// tick ranges (from `hot_state.peek_pending_tick_changes()`). Pools with pending changes
/// or liquidity changes are flagged for tick resync in the returned `HotInjectionResult`.
pub fn inject_hot_state_to_evm(
    cache: &mut EvmCache,
    amms: &HashMap<Address, AMMRef>,
    pool_addresses: &[Address],
    pending_tick_ranges: &HashMap<Address, Vec<(i32, i32)>>,
    observations: &mut SlotObservationTracker,
) -> HotInjectionResult {
    let mut result = HotInjectionResult {
        v2_injected: 0,
        v3_injected: 0,
        v3_needs_tick_resync: Vec::new(),
        injection_failures: Vec::new(),
    };

    for &addr in pool_addresses {
        let Some(amm_ref) = amms.get(&addr) else {
            // Pool not in AMM map — fall back to full storage purge
            cache.purge_pool_storage(addr);
            result.injection_failures.push(addr);
            continue;
        };

        let guard = amm_ref.read().expect("AMM lock poisoned during injection");
        match &*guard {
            LocalAMM::UniswapV2(pool) => {
                let packed = encode_v2_reserves_raw(pool.reserve_0, pool.reserve_1);
                if let Err(e) = cache.insert_storage_slot(addr, V2_RESERVES_SLOT, packed) {
                    warn!(pool = %addr, error = ?e, "V2 reserves injection failed");
                    cache.purge_pool_slots(addr, &[V2_RESERVES_SLOT]);
                    result.injection_failures.push(addr);
                } else {
                    observations.observe(
                        addr,
                        V2_RESERVES_SLOT,
                        packed,
                        current_observation_time(),
                    );
                    result.v2_injected += 1;
                }
            }
            LocalAMM::UniswapV3(pool) => {
                inject_v3_hot_state(
                    cache,
                    addr,
                    pool,
                    V3_SLOT0_SLOT,
                    V3_LIQUIDITY_SLOT,
                    pending_tick_ranges,
                    observations,
                    &mut result,
                );
            }
            LocalAMM::PancakeSwapV3(pool) => {
                inject_v3_hot_state(
                    cache,
                    addr,
                    pool,
                    V3_SLOT0_SLOT,
                    PANCAKE_V3_LIQUIDITY_SLOT,
                    pending_tick_ranges,
                    observations,
                    &mut result,
                );
            }
            LocalAMM::Slipstream(slip) => {
                // Slipstream uses V3-style storage at shifted slots.
                // Build a temporary V3 pool view with just the fields needed for injection.
                let v3_view = UniswapV3Pool {
                    address: slip.address,
                    token_a: amms::amms::Token::new_with_decimals(slip.token_a, slip.decimals_a),
                    token_b: amms::amms::Token::new_with_decimals(slip.token_b, slip.decimals_b),
                    fee: slip.fee,
                    tick: slip.tick,
                    tick_spacing: slip.tick_spacing,
                    liquidity: slip.liquidity,
                    sqrt_price: slip.sqrt_price,
                    ticks: HashMap::new(),
                    tick_bitmap: HashMap::new(),
                };
                inject_v3_hot_state(
                    cache,
                    addr,
                    &v3_view,
                    SLIPSTREAM_SLOT0_SLOT,
                    SLIPSTREAM_LIQUIDITY_SLOT,
                    pending_tick_ranges,
                    observations,
                    &mut result,
                );
            }
            _ => {
                // Balancer/ERC4626/Curve/SolidlyV2/BalancerV3/UniswapV4 — fall back to full storage purge
                cache.purge_pool_storage(addr);
            }
        }
    }

    if result.v2_injected > 0 || result.v3_injected > 0 {
        debug!(
            v2 = result.v2_injected,
            v3 = result.v3_injected,
            tick_resync = result.v3_needs_tick_resync.len(),
            failures = result.injection_failures.len(),
            "Injected hot state into EVM cache"
        );
    }

    result
}

/// Internal helper: inject V3 slot0 + liquidity into EVM cache.
#[allow(clippy::too_many_arguments)]
fn inject_v3_hot_state(
    cache: &mut EvmCache,
    addr: Address,
    pool: &UniswapV3Pool,
    slot0_slot: U256,
    liquidity_slot: U256,
    pending_tick_ranges: &HashMap<Address, Vec<(i32, i32)>>,
    observations: &mut SlotObservationTracker,
    result: &mut HotInjectionResult,
) {
    // Read-modify-write slot0: read existing (preserves observation fields), patch price+tick
    let slot0_result = cache.read_storage_slot(addr, slot0_slot);
    match slot0_result {
        Ok(existing_slot0) => {
            let patched = encode_v3_slot0_patch(existing_slot0, pool.sqrt_price, pool.tick);
            if let Err(e) = cache.insert_storage_slot(addr, slot0_slot, patched) {
                warn!(pool = %addr, error = ?e, "V3 slot0 injection failed");
                cache.purge_pool_slots(addr, &[slot0_slot, liquidity_slot]);
                result.injection_failures.push(addr);
                return;
            }
            observations.observe(addr, slot0_slot, patched, current_observation_time());
        }
        Err(e) => {
            // First cycle or cache miss — can't read-modify-write, fall back to purge
            debug!(pool = %addr, error = ?e, "V3 slot0 read failed (first cycle?), falling back to purge");
            cache.purge_pool_slots(addr, &[slot0_slot, liquidity_slot]);
            result.injection_failures.push(addr);
            return;
        }
    }

    // Inject liquidity directly
    let liquidity_value = U256::from(pool.liquidity);
    if let Err(e) = cache.insert_storage_slot(addr, liquidity_slot, liquidity_value) {
        warn!(pool = %addr, error = ?e, "V3 liquidity injection failed");
        cache.purge_pool_slots(addr, &[slot0_slot, liquidity_slot]);
        result.injection_failures.push(addr);
        return;
    }

    // Check if liquidity changed from last observation
    let prev_liquidity = observations.last_value(addr, liquidity_slot);
    let liquidity_changed = prev_liquidity.is_some_and(|prev| prev != liquidity_value);
    observations.observe(
        addr,
        liquidity_slot,
        liquidity_value,
        current_observation_time(),
    );

    result.v3_injected += 1;

    // Determine if tick resync is needed
    let has_pending = pending_tick_ranges
        .get(&addr)
        .is_some_and(|ranges| !ranges.is_empty());

    if has_pending {
        let ranges = pending_tick_ranges[&addr].clone();
        result
            .v3_needs_tick_resync
            .push((addr, TickResyncReason::KnownRanges(ranges)));
    } else if liquidity_changed {
        // Liquidity changed but no Mint/Burn events captured — need incremental resync
        result
            .v3_needs_tick_resync
            .push((addr, TickResyncReason::UnknownRanges));
    }
    // If liquidity unchanged and no pending tick changes → skip tick resync entirely
}

/// Selectively purge contract storage using the slot observation tracker.
///
/// Instead of `cache.purge_contracts_storage()` which deletes ALL slots for a contract,
/// this enumerates known cached slots and only purges those that `should_refetch()` says
/// are likely to have changed. Stable slots (config, immutable addresses, etc.) are skipped,
/// saving RPC calls when revm subsequently reads them.
///
/// Falls back to full purge if no cached slots are found (first cycle).
///
/// Returns `(slots_purged, slots_skipped)`.
pub fn smart_purge_contract_storage(
    cache: &mut EvmCache,
    addr: Address,
    tracker: &mut SlotObservationTracker,
) -> (usize, usize) {
    let cached_slots = cache.enumerate_contract_slots(addr);
    if cached_slots.is_empty() {
        // No cached data — fall back to full purge (first cycle)
        cache.purge_pool_storage(addr);
        return (0, 0);
    }

    let mut purged = 0usize;
    let mut skipped = 0usize;

    for slot in &cached_slots {
        if tracker.should_refetch(
            addr,
            *slot,
            current_observation_time(),
            &FreshnessParams::default(),
        ) {
            cache.purge_pool_slots(addr, &[*slot]);
            purged += 1;
        } else {
            tracker.record_skip(addr, *slot);
            skipped += 1;
        }
    }

    (purged, skipped)
}

/// Selectively purge multiple contracts using the slot observation tracker.
///
/// Like `smart_purge_contract_storage` but for a batch of addresses.
/// Returns `(total_purged, total_skipped)`.
pub fn smart_purge_contracts_storage(
    cache: &mut EvmCache,
    addresses: impl IntoIterator<Item = Address>,
    tracker: &mut SlotObservationTracker,
) -> (usize, usize) {
    let mut total_purged = 0usize;
    let mut total_skipped = 0usize;
    for addr in addresses {
        let (purged, skipped) = smart_purge_contract_storage(cache, addr, tracker);
        total_purged += purged;
        total_skipped += skipped;
    }
    (total_purged, total_skipped)
}

/// Observation-aware full storage purge, replacing the 48h `purge_all_storage()`.
///
/// Instead of nuking everything, walks each contract's cached slots and only purges
/// those whose staleness exceeds their time-based threshold. Stable slots (never-changed
/// in observation history) are kept, eliminating the 48h RPC spike.
///
/// Falls back to `purge_all_storage()` if the tracker has insufficient data.
pub fn smart_purge_all_storage(
    cache: &mut EvmCache,
    tracker: &mut SlotObservationTracker,
) -> (usize, usize) {
    // If tracker is very new (< MIN_OBSERVATIONS worth of data), fall back
    if tracker.len() < 50 {
        let purged = cache.purge_all_storage();
        return (purged, 0);
    }

    let all_addresses: Vec<Address> = cache.all_cached_contract_addresses();
    smart_purge_contracts_storage(cache, all_addresses, tracker)
}
