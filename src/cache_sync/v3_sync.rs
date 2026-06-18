use super::v3_bitmap::{
    V3Flavor, compute_adaptive_scan_params, extract_initialized_ticks_from_bitmap,
    extract_ticks_from_bitmap_word, needs_tick_resync, tick_to_word,
};
use super::*;
use evm_fork_cache::cache::TickInfo as CacheTickInfo;

/// Result of V3 phase 1 initialization.
///
/// When using the two-phase V3 init flow, phase 1 determines whether the pool
/// can be fully initialized from cache or needs a full bitmap resync. If resync
/// is needed, the caller can batch-prefetch bitmap storage slots in parallel
/// before completing the resync in phase 2.
pub enum V3InitPhase1Result {
    /// Pool fully initialized (snapshot hit or preloaded storage).
    Complete(UniswapV3Pool),
    /// Pool needs full bitmap resync. Contains partially-built pool and flavor.
    NeedsResync {
        pool: UniswapV3Pool,
        flavor: V3Flavor,
    },
    /// Pool needs incremental resync (snapshot validation failed).
    /// Contains the partially-built pool, flavor, and old bitmap/ticks for comparison.
    NeedsIncrementalResync {
        pool: UniswapV3Pool,
        flavor: V3Flavor,
        old_bitmap: HashMap<i16, U256>,
        old_ticks: HashMap<i32, Info>,
    },
}

/// Target for V3 bitmap prefetch -- describes one pool that needs parallel bitmap fetching.
pub struct V3BitmapPrefetchTarget {
    pub address: Address,
    pub flavor: V3Flavor,
    pub tick_spacing: i32,
    pub center_tick: i32,
    pub max_scan_words: i32,
    /// Consecutive empty bitmap words before stopping the scan (mirrors sync logic).
    pub empty_word_threshold: usize,
}

pub type V3IncrementalPrefetchTarget<'a> =
    (Address, V3Flavor, i32, i32, u128, &'a HashMap<i16, U256>);

#[instrument(skip(cache), fields(pool = %address))]
pub async fn init_uniswap_v3_from_cache(
    cache: &mut EvmCache,
    address: Address,
) -> Result<UniswapV3Pool> {
    match init_v3_from_cache(cache, address, V3Flavor::UniswapV3).await? {
        V3InitInternalResult::Complete(pool) => Ok(pool),
        V3InitInternalResult::DeferredResync(mut pool) => {
            sync_uniswap_v3_ticks(cache, &mut pool, V3Flavor::UniswapV3)?;
            save_v3_tick_snapshot(cache, &pool);
            Ok(pool)
        }
        V3InitInternalResult::DeferredIncrementalResync(mut pool, old_bitmap, old_ticks) => {
            let flavor = V3Flavor::UniswapV3;
            incremental_sync_v3_ticks(cache, &mut pool, &old_bitmap, &old_ticks, flavor, false)?;
            inject_v3_tick_data(cache, address, &pool, flavor);
            Ok(pool)
        }
    }
}

pub async fn init_pancakeswap_v3_from_cache(
    cache: &mut EvmCache,
    address: Address,
) -> Result<UniswapV3Pool> {
    match init_v3_from_cache(cache, address, V3Flavor::PancakeSwapV3).await? {
        V3InitInternalResult::Complete(pool) => Ok(pool),
        V3InitInternalResult::DeferredResync(mut pool) => {
            sync_uniswap_v3_ticks(cache, &mut pool, V3Flavor::PancakeSwapV3)?;
            save_v3_tick_snapshot(cache, &pool);
            Ok(pool)
        }
        V3InitInternalResult::DeferredIncrementalResync(mut pool, old_bitmap, old_ticks) => {
            let flavor = V3Flavor::PancakeSwapV3;
            incremental_sync_v3_ticks(cache, &mut pool, &old_bitmap, &old_ticks, flavor, false)?;
            inject_v3_tick_data(cache, address, &pool, flavor);
            Ok(pool)
        }
    }
}

/// Phase 1 of V3 initialization: determines cache status and returns NeedsResync
/// instead of performing the expensive tick bitmap scan inline.
///
/// Call this for each V3 pool, then batch-prefetch bitmap storage for all pools
/// that return NeedsResync, then call `sync_uniswap_v3_ticks` + `save_v3_tick_snapshot`
/// to complete them.
pub async fn init_v3_phase1(
    cache: &mut EvmCache,
    address: Address,
    flavor: V3Flavor,
) -> Result<V3InitPhase1Result> {
    match init_v3_from_cache(cache, address, flavor).await? {
        V3InitInternalResult::Complete(pool) => Ok(V3InitPhase1Result::Complete(pool)),
        V3InitInternalResult::DeferredResync(pool) => {
            Ok(V3InitPhase1Result::NeedsResync { pool, flavor })
        }
        V3InitInternalResult::DeferredIncrementalResync(pool, old_bitmap, old_ticks) => {
            Ok(V3InitPhase1Result::NeedsIncrementalResync {
                pool,
                flavor,
                old_bitmap,
                old_ticks,
            })
        }
    }
}

/// Internal result from init_v3_from_cache -- never leaves resync inline.
/// The caller decides whether to complete the resync or defer it.
enum V3InitInternalResult {
    Complete(UniswapV3Pool),
    DeferredResync(UniswapV3Pool),
    DeferredIncrementalResync(UniswapV3Pool, HashMap<i16, U256>, HashMap<i32, Info>),
}

/// Internal status for tick cache handling in init_uniswap_v3_from_cache
#[derive(Debug)]
enum TickCacheStatus {
    /// Storage is pre-loaded from evm_state.json, no injection needed
    PreloadedStorage,
    /// Tick snapshot is valid but needs to be injected into EVM storage
    SnapshotNeedsInjection,
    /// Need full resync from chain (no snapshot available)
    NeedsResync,
    /// Has stale snapshot -- do incremental resync (compare bitmaps, re-fetch changed ticks)
    NeedsIncrementalResync {
        old_bitmap: HashMap<i16, U256>,
        old_ticks: HashMap<i32, Info>,
    },
}

fn cache_tick_to_info(tick: CacheTickInfo) -> Info {
    Info {
        liquidity_gross: tick.liquidity_gross,
        liquidity_net: tick.liquidity_net,
        initialized: tick.initialized,
    }
}

fn info_to_cache_tick(info: &Info) -> CacheTickInfo {
    CacheTickInfo {
        liquidity_gross: info.liquidity_gross,
        liquidity_net: info.liquidity_net,
        initialized: info.initialized,
    }
}

fn cache_ticks_to_info(ticks: HashMap<i32, CacheTickInfo>) -> HashMap<i32, Info> {
    ticks
        .into_iter()
        .map(|(tick, info)| (tick, cache_tick_to_info(info)))
        .collect()
}

fn info_ticks_to_cache(ticks: &HashMap<i32, Info>) -> HashMap<i32, CacheTickInfo> {
    ticks
        .iter()
        .map(|(tick, info)| (*tick, info_to_cache_tick(info)))
        .collect()
}

fn can_reuse_v3_tick_snapshot(
    cached_liquidity: u128,
    current_liquidity: u128,
    cached_tick: i32,
    current_tick: i32,
) -> bool {
    cached_liquidity == current_liquidity
        && (cached_tick - current_tick).abs() <= MAX_TICK_DRIFT_FOR_CACHE
}

fn collect_targeted_tick_words(
    current_tick: i32,
    tick_spacing: i32,
    affected_tick_ranges: &[(i32, i32)],
) -> HashSet<i16> {
    let mut affected_words = HashSet::new();
    if tick_spacing == 0 {
        return affected_words;
    }

    for &(tick_lower, tick_upper) in affected_tick_ranges {
        let lower_word = tick_to_word(tick_lower, tick_spacing);
        let upper_word = tick_to_word(tick_upper, tick_spacing);
        for word in lower_word..=upper_word {
            affected_words.insert(word as i16);
        }
    }

    affected_words.insert(tick_to_word(current_tick, tick_spacing) as i16);
    affected_words
}

async fn init_v3_from_cache(
    cache: &mut EvmCache,
    address: Address,
    flavor: V3Flavor,
) -> Result<V3InitInternalResult> {
    let fn_start = Instant::now();

    let t0 = Instant::now();
    cache.ensure_account(address).await?;
    let ensure_ms = t0.elapsed().as_millis();

    // Check if this pool has pre-loaded storage from the unified EVM state cache
    let has_preloaded_storage = cache.has_pool_storage(address);
    let preloaded_slot_count = if has_preloaded_storage {
        cache.pool_storage_slot_count(address)
    } else {
        0
    };

    // Try to load immutable metadata from cache (token0, token1, fee, tickSpacing)
    let t1 = Instant::now();
    let (token0, token1, fee, tick_spacing) =
        if let Some(metadata) = cache.immutable_cache().get_v3_pool(address) {
            debug!("using cached V3 pool metadata");
            (
                metadata.token0,
                metadata.token1,
                metadata.fee,
                metadata.tick_spacing,
            )
        } else {
            // Fetch immutable data from RPC and cache
            let token0 = call_view(cache, address, IUniswapV3Pool::token0Call {})?;
            let token1 = call_view(cache, address, IUniswapV3Pool::token1Call {})?;
            let fee_raw = call_view(cache, address, IUniswapV3Pool::feeCall {})?;
            let tick_spacing_raw = call_view(cache, address, IUniswapV3Pool::tickSpacingCall {})?;

            let fee = fee_raw.to::<u32>();
            let tick_spacing = tick_spacing_raw.as_i32();

            cache.immutable_cache_mut().set_v3_pool(
                address,
                V3PoolMetadata {
                    token0,
                    token1,
                    fee,
                    tick_spacing,
                },
            );
            debug!("fetched and cached V3 pool metadata");
            (token0, token1, fee, tick_spacing)
        };
    let metadata_ms = t1.elapsed().as_millis();

    // Selectively purge only slot0 and liquidity slots to force fresh RPC reads.
    // This preserves tick bitmap and tick info data in the cache, avoiding expensive
    // re-injection or re-fetch of tick data when the snapshot is still valid.
    cache.purge_pool_slots(address, &[V3_SLOT0_SLOT, flavor.liquidity_slot()]);

    // Mutable data: slot0 and liquidity must be fetched fresh
    let t2 = Instant::now();
    let slot0 = call_view(cache, address, IUniswapV3Pool::slot0Call {})?;
    let liquidity = call_view(cache, address, IUniswapV3Pool::liquidityCall {})?;
    let slot0_ms = t2.elapsed().as_millis();

    let dec0 = cache.erc20_decimals(token0).unwrap_or(18);
    let dec1 = cache.erc20_decimals(token1).unwrap_or(18);

    let current_tick = slot0.tick.as_i32();

    // Try to restore tick data from snapshot cache
    // Four possible outcomes:
    // 1. Liquidity unchanged + tick drift OK + storage pre-loaded: use cached data
    // 2. Liquidity unchanged + tick drift OK + no storage: use cached, inject into EVM
    // 3. Validation fails: purge old storage (if any), resync from chain
    // 4. No snapshot: purge and resync

    let t3 = Instant::now();

    // First, extract snapshot data to avoid borrow conflicts later.
    let snapshot_data = cache.tick_snapshot_cache().get(address).map(|snapshot| {
        (
            snapshot.last_liquidity,
            snapshot.last_tick,
            snapshot.to_tick_bitmap(),
            cache_ticks_to_info(snapshot.to_ticks()),
            snapshot.ticks.len(),
        )
    });

    let (tick_bitmap, ticks, cache_status) = if let Some((
        cached_liquidity,
        cached_tick,
        cached_bitmap,
        cached_ticks,
        cached_ticks_count,
    )) = snapshot_data
    {
        let tick_drift = (cached_tick - current_tick).abs();
        if cached_bitmap.is_empty() {
            debug!(pool = %address, "tick snapshot empty, will do incremental resync");
            (
                HashMap::new(),
                HashMap::new(),
                TickCacheStatus::NeedsIncrementalResync {
                    old_bitmap: cached_bitmap,
                    old_ticks: cached_ticks,
                },
            )
        } else if can_reuse_v3_tick_snapshot(cached_liquidity, liquidity, cached_tick, current_tick)
        {
            let status = if has_preloaded_storage {
                debug!(
                    last_liquidity = cached_liquidity,
                    current_liquidity = liquidity,
                    tick_drift,
                    preloaded_slots = preloaded_slot_count,
                    cached_ticks = cached_ticks_count,
                    cached_bitmap_words = cached_bitmap.len(),
                    "using pre-loaded EVM storage (validation passed: liquidity + tick drift)"
                );
                TickCacheStatus::PreloadedStorage
            } else {
                debug!(
                    last_liquidity = cached_liquidity,
                    current_liquidity = liquidity,
                    tick_drift,
                    cached_ticks = cached_ticks_count,
                    cached_bitmap_words = cached_bitmap.len(),
                    "using cached V3 tick snapshot (validation passed, will inject)"
                );
                TickCacheStatus::SnapshotNeedsInjection
            };
            (cached_bitmap, cached_ticks, status)
        } else {
            if cached_liquidity != liquidity {
                debug!(
                    cached_liquidity,
                    current_liquidity = liquidity,
                    has_preloaded_storage,
                    "tick snapshot: liquidity changed, will do incremental resync"
                );
            } else {
                debug!(
                    cached_tick,
                    current_tick,
                    tick_drift,
                    max_drift = MAX_TICK_DRIFT_FOR_CACHE,
                    "tick snapshot: tick drift exceeded, will do incremental resync"
                );
            }
            (
                HashMap::new(),
                HashMap::new(),
                TickCacheStatus::NeedsIncrementalResync {
                    old_bitmap: cached_bitmap,
                    old_ticks: cached_ticks,
                },
            )
        }
    } else {
        debug!(has_preloaded_storage, "no tick snapshot in cache");
        // No snapshot - if we have storage, it might be stale, but we can't validate
        // Better to purge and resync to be safe
        if has_preloaded_storage {
            let purged = cache.purge_pool_storage(address);
            debug!(
                pool = %address,
                purged_slots = purged,
                "purged pool storage (no tick snapshot for validation)"
            );
        }
        (HashMap::new(), HashMap::new(), TickCacheStatus::NeedsResync)
    };

    let mut pool = UniswapV3Pool {
        address,
        token_a: Token::new_with_decimals(token0, dec0),
        token_b: Token::new_with_decimals(token1, dec1),
        fee,
        tick_spacing,
        sqrt_price: slot0.sqrtPriceX96.to(),
        tick: current_tick,
        liquidity,
        tick_bitmap,
        ticks,
    };

    let validation_ms = t3.elapsed().as_millis();

    let t4 = Instant::now();
    let cache_status_label = match &cache_status {
        TickCacheStatus::PreloadedStorage => "PreloadedStorage",
        TickCacheStatus::SnapshotNeedsInjection => "SnapshotNeedsInjection",
        TickCacheStatus::NeedsResync => "NeedsResync",
        TickCacheStatus::NeedsIncrementalResync { .. } => "NeedsIncrementalResync",
    };
    match cache_status {
        TickCacheStatus::PreloadedStorage => {
            debug!(
                pool = %address,
                storage_slots = preloaded_slot_count,
                "V3 pool initialized from pre-loaded EVM storage (fast path)"
            );
        }
        TickCacheStatus::SnapshotNeedsInjection => {
            // Inject tick data into EVM storage cache so subsequent EVM calls
            // to tickBitmap() and ticks() hit the cache instead of going to RPC
            let mut bitmap_injected = 0;
            let mut ticks_injected = 0;

            match cache.inject_v3_tick_bitmap_with_base(
                address,
                &pool.tick_bitmap,
                flavor.tick_bitmap_base_slot(),
            ) {
                Ok(count) => bitmap_injected = count,
                Err(e) => {
                    warn!(
                        pool = %address,
                        error = %e,
                        "failed to inject tick bitmap into storage cache"
                    );
                }
            }

            let cache_ticks = info_ticks_to_cache(&pool.ticks);
            match cache.inject_v3_ticks_with_base(address, &cache_ticks, flavor.ticks_base_slot()) {
                Ok(count) => ticks_injected = count,
                Err(e) => {
                    warn!(
                        pool = %address,
                        error = %e,
                        "failed to inject ticks into storage cache"
                    );
                }
            }

            debug!(
                pool = %address,
                bitmap_words = bitmap_injected,
                ticks = ticks_injected,
                "injected V3 tick data into EVM storage cache"
            );
        }
        TickCacheStatus::NeedsResync => {
            // No snapshot -- defer resync to caller so it can batch-prefetch
            // bitmap storage slots in parallel across all pools.
            let tick_resolution_ms = t4.elapsed().as_millis();
            let total_ms = fn_start.elapsed().as_millis();
            debug!(
                pool = %address,
                ensure_ms,
                metadata_ms,
                slot0_ms,
                validation_ms,
                tick_resolution_ms,
                total_ms,
                cache_status = "NeedsResync (deferred)",
                has_preloaded_storage,
                preloaded_slot_count,
                "V3 pool init phase1 — deferring resync"
            );
            return Ok(V3InitInternalResult::DeferredResync(pool));
        }
        TickCacheStatus::NeedsIncrementalResync {
            old_bitmap,
            old_ticks,
        } => {
            // Defer incremental resync to caller for parallel bitmap prefetch.
            // This avoids hundreds of sequential RPC calls per pool.
            let tick_resolution_ms = t4.elapsed().as_millis();
            let total_ms = fn_start.elapsed().as_millis();
            debug!(
                pool = %address,
                ensure_ms,
                metadata_ms,
                slot0_ms,
                validation_ms,
                tick_resolution_ms,
                total_ms,
                cache_status = "NeedsIncrementalResync (deferred)",
                has_preloaded_storage,
                preloaded_slot_count,
                "V3 pool init phase1 — deferring incremental resync"
            );
            return Ok(V3InitInternalResult::DeferredIncrementalResync(
                pool, old_bitmap, old_ticks,
            ));
        }
    }
    let mut lazy_extended = false;
    if !pool.tick_bitmap.is_empty() && needs_tick_resync(&pool, pool.tick) {
        extend_v3_tick_region(cache, &mut pool, flavor)?;
        lazy_extended = true;
    }
    let tick_resolution_ms = t4.elapsed().as_millis();
    let total_ms = fn_start.elapsed().as_millis();

    debug!(
        pool = %address,
        ensure_ms,
        metadata_ms,
        slot0_ms,
        validation_ms,
        tick_resolution_ms,
        total_ms,
        cache_status = cache_status_label,
        has_preloaded_storage,
        preloaded_slot_count,
        lazy_extended,
        tick_bitmap_words = pool.tick_bitmap.len(),
        ticks_count = pool.ticks.len(),
        "V3 pool init breakdown"
    );

    Ok(V3InitInternalResult::Complete(pool))
}

/// Save a tick snapshot for a V3 pool to the cache.
///
/// Call this after syncing tick data to persist for future restarts.
pub fn save_v3_tick_snapshot(cache: &mut EvmCache, pool: &UniswapV3Pool) {
    let cache_ticks = info_ticks_to_cache(&pool.ticks);
    let snapshot = V3PoolTickSnapshot::from_pool_data(
        &pool.tick_bitmap,
        &cache_ticks,
        pool.liquidity,
        pool.tick,
    );
    cache.tick_snapshot_cache_mut().set(pool.address, snapshot);
    debug!(
        pool = %pool.address,
        ticks = pool.ticks.len(),
        bitmap_words = pool.tick_bitmap.len(),
        liquidity = pool.liquidity,
        "saved V3 tick snapshot to cache"
    );
}

/// Extend the pool's loaded tick region to cover the current tick.
///
/// When the tick moves to a bitmap word not present in `pool.tick_bitmap`,
/// [`needs_tick_resync`] fires. Instead of clearing all tick data and doing
/// a full adaptive scan (which is expensive), this function loads only the
/// missing bitmap word(s) in a zone around the new tick position and fetches
/// tick info for any newly discovered initialized ticks. Existing tick data
/// outside the new region is preserved.
///
/// This is safe because initialized ticks don't change their liquidityNet
/// unless a Mint/Burn event occurs. Mint/Burn events now queue
/// `pending_tick_changes` in hot state, which are handled via targeted resync
/// before `sync_selected_pool_state` falls back to a broader RPC refresh.
#[instrument(skip(cache), fields(pool = %pool.address, tick_spacing = pool.tick_spacing, current_tick = pool.tick))]
pub fn extend_v3_tick_region(
    cache: &mut EvmCache,
    pool: &mut UniswapV3Pool,
    flavor: V3Flavor,
) -> Result<()> {
    if pool.tick_spacing == 0 {
        return Ok(());
    }

    let fn_start = Instant::now();
    let current_word = tick_to_word(pool.tick, pool.tick_spacing);
    let min_word = tick_to_word(MIN_TICK, pool.tick_spacing);
    let max_word = tick_to_word(MAX_TICK, pool.tick_spacing);
    let bitmap_base = flavor.tick_bitmap_base_slot();

    // Load a zone of cycle_hot_zone_radius() words around the current tick.
    // Only fetch words not already present in the pool's bitmap.
    let hot_radius = cycle_hot_zone_radius();
    let zone_start = (current_word - hot_radius).max(min_word);
    let zone_end = (current_word + hot_radius).min(max_word);

    let mut new_words = 0usize;
    let mut new_ticks = Vec::new();

    for word in zone_start..=zone_end {
        let word_i16 = word as i16;
        if pool.tick_bitmap.contains_key(&word_i16) {
            continue; // Already loaded
        }

        // Purge and read the bitmap word fresh from RPC
        let slot = v3_tick_bitmap_storage_key_with_base(word_i16, bitmap_base);
        cache.purge_pool_slots(pool.address, &[slot]);
        let bitmap = cache.read_storage_slot(pool.address, slot)?;

        pool.tick_bitmap.insert(word_i16, bitmap);
        new_words += 1;

        // Extract initialized ticks from this new word
        if bitmap != U256::ZERO {
            let ticks = extract_ticks_from_bitmap_word(word, bitmap, pool.tick_spacing);
            new_ticks.extend(ticks);
        }
    }

    // Fetch tick info for all newly discovered initialized ticks
    if !new_ticks.is_empty() {
        let tick_infos =
            fetch_tick_info_raw_storage(cache, pool.address, &new_ticks, flavor, false)?;
        for (tick, info) in tick_infos {
            pool.ticks.insert(tick, info);
        }
    }

    let elapsed_ms = fn_start.elapsed().as_millis();
    debug!(
        pool = %pool.address,
        new_words,
        new_ticks = new_ticks.len(),
        total_bitmap_words = pool.tick_bitmap.len(),
        total_ticks = pool.ticks.len(),
        elapsed_ms,
        "extended V3 tick region (lazy)"
    );

    // Persist updated tick data
    save_v3_tick_snapshot(cache, pool);

    // Inject new tick data into EVM storage cache so subsequent SLOADs hit cache
    inject_v3_tick_data(cache, pool.address, pool, flavor);

    Ok(())
}

/// Inject V3 tick bitmap and tick info into EVM storage cache after incremental resync.
///
/// After incremental_sync_v3_ticks, bitmaps are fresh (from raw storage reads) but the
/// CacheDB overlay was cleared during purge. Restored ticks (from unchanged words) are
/// not in BlockchainDb. This function re-injects both to ensure EVM reads hit cache.
pub fn inject_v3_tick_data(
    cache: &mut EvmCache,
    address: Address,
    pool: &UniswapV3Pool,
    flavor: V3Flavor,
) {
    if let Err(e) = cache.inject_v3_tick_bitmap_with_base(
        address,
        &pool.tick_bitmap,
        flavor.tick_bitmap_base_slot(),
    ) {
        warn!(pool = %address, error = %e, "failed to inject tick bitmap after incremental resync");
    }
    let cache_ticks = info_ticks_to_cache(&pool.ticks);
    if let Err(e) = cache.inject_v3_ticks_with_base(address, &cache_ticks, flavor.ticks_base_slot())
    {
        warn!(pool = %address, error = %e, "failed to inject ticks after incremental resync");
    }
}

/// Incremental resync of V3 tick data using bitmap comparison.
///
/// Instead of fetching all ticks from scratch, this function:
/// 1. Scans bitmap words in a bounded range around the current tick
/// 2. Compares fresh bitmap values against cached values
/// 3. In the "hot zone" near current tick: always re-fetches tick info
///    (catches liquidity changes without bitmap changes)
/// 4. Outside hot zone: only re-fetches ticks in changed bitmap words
/// 5. Restores cached tick info for unchanged words outside hot zone
///
/// The pool's tick_bitmap and ticks are rebuilt from scratch within the scan range.
#[instrument(skip(cache, old_bitmap, old_ticks), fields(pool = %pool.address, tick_spacing = pool.tick_spacing, current_tick = pool.tick))]
pub fn incremental_sync_v3_ticks(
    cache: &mut EvmCache,
    pool: &mut UniswapV3Pool,
    old_bitmap: &HashMap<i16, U256>,
    old_ticks: &HashMap<i32, Info>,
    flavor: V3Flavor,
    pre_purged: bool,
) -> Result<()> {
    if pool.tick_spacing == 0 {
        return Ok(());
    }

    let fn_start = Instant::now();

    let min_word = tick_to_word(MIN_TICK, pool.tick_spacing);
    let max_word = tick_to_word(MAX_TICK, pool.tick_spacing);
    let current_word = tick_to_word(pool.tick, pool.tick_spacing);

    // Clear pool data -- we'll rebuild from scan range
    pool.tick_bitmap.clear();
    pool.ticks.clear();

    let scan_params = compute_adaptive_scan_params(pool.liquidity, pool.tick_spacing);
    debug!(
        pool = %pool.address,
        liquidity = pool.liquidity,
        tick_spacing = pool.tick_spacing,
        max_scan_words = scan_params.max_scan_words,
        empty_word_threshold = scan_params.empty_word_threshold,
        "incremental adaptive scan params"
    );

    // Step 1: Determine scan range using outward scan, using OLD bitmap for boundaries.
    // This ensures we scan at least as far as the old data covered near current tick,
    // and extends further if needed.
    let mut words_to_scan = Vec::new();

    // Positive direction
    let mut consecutive_empty_up = 0;
    for offset in 0..=scan_params.max_scan_words {
        let word = current_word + offset;
        if word > max_word {
            break;
        }
        words_to_scan.push(word as i16);
        let cached_val = old_bitmap
            .get(&(word as i16))
            .copied()
            .unwrap_or(U256::ZERO);
        if cached_val == U256::ZERO {
            consecutive_empty_up += 1;
            if consecutive_empty_up >= scan_params.empty_word_threshold {
                break;
            }
        } else {
            consecutive_empty_up = 0;
        }
    }

    // Negative direction
    let mut consecutive_empty_down = 0;
    for offset in 1..=scan_params.max_scan_words {
        let word = current_word - offset;
        if word < min_word {
            break;
        }
        words_to_scan.push(word as i16);
        let cached_val = old_bitmap
            .get(&(word as i16))
            .copied()
            .unwrap_or(U256::ZERO);
        if cached_val == U256::ZERO {
            consecutive_empty_down += 1;
            if consecutive_empty_down >= scan_params.empty_word_threshold {
                break;
            }
        } else {
            consecutive_empty_down = 0;
        }
    }

    let scan_start = Instant::now();
    let bitmap_base = flavor.tick_bitmap_base_slot();

    // Step 2: Purge bitmap storage slots for entire scan range to force fresh RPC reads
    // (skip if already purged and prefetched by the parallel prefetch phase)
    if !pre_purged {
        let bitmap_slots_to_purge: Vec<U256> = words_to_scan
            .iter()
            .map(|&w| v3_tick_bitmap_storage_key_with_base(w, bitmap_base))
            .collect();
        cache.purge_pool_slots(pool.address, &bitmap_slots_to_purge);
    }

    // Step 3: Fetch fresh bitmaps and compare with cached
    let mut words_scanned = 0;
    let mut words_changed = 0;
    let mut words_hot_zone = 0;
    let mut ticks_to_fetch = Vec::new();

    for &word in &words_to_scan {
        words_scanned += 1;

        let slot = v3_tick_bitmap_storage_key_with_base(word, bitmap_base);
        let fresh_bitmap = cache.read_storage_slot(pool.address, slot)?;

        let cached_value = old_bitmap.get(&word).copied().unwrap_or(U256::ZERO);
        pool.tick_bitmap.insert(word, fresh_bitmap);

        let word_i32 = word as i32;
        let in_hot_zone =
            (word_i32 - current_word).unsigned_abs() < incremental_hot_zone_radius() as u32;

        if in_hot_zone {
            // Hot zone: always re-fetch all initialized ticks (catches liquidity changes)
            words_hot_zone += 1;
            let fresh_ticks =
                extract_ticks_from_bitmap_word(word_i32, fresh_bitmap, pool.tick_spacing);
            ticks_to_fetch.extend_from_slice(&fresh_ticks);

            if fresh_bitmap != cached_value {
                words_changed += 1;
            }
        } else if fresh_bitmap != cached_value {
            // Outside hot zone with changed bitmap: re-fetch ticks for this word
            words_changed += 1;
            let fresh_ticks =
                extract_ticks_from_bitmap_word(word_i32, fresh_bitmap, pool.tick_spacing);
            ticks_to_fetch.extend_from_slice(&fresh_ticks);
        } else {
            // Outside hot zone, bitmap unchanged: restore cached tick info
            let cached_tick_indices =
                extract_ticks_from_bitmap_word(word_i32, cached_value, pool.tick_spacing);
            for tick_idx in cached_tick_indices {
                if let Some(info) = old_ticks.get(&tick_idx) {
                    pool.ticks.insert(tick_idx, info.clone());
                }
            }
        }
    }

    let scan_ms = scan_start.elapsed().as_millis();

    // Step 4: Fetch fresh tick info via raw storage reads (slots 0+3 only)
    // Raw reads populate BlockchainDb directly, so EVM SLOADs find fresh values
    let batch_start = Instant::now();
    if !ticks_to_fetch.is_empty() {
        let fresh_ticks =
            fetch_tick_info_raw_storage(cache, pool.address, &ticks_to_fetch, flavor, pre_purged)?;
        for (tick, info) in fresh_ticks {
            pool.ticks.insert(tick, info);
        }
    }
    let batch_ms = batch_start.elapsed().as_millis();

    let ticks_restored = pool.ticks.len().saturating_sub(ticks_to_fetch.len());
    let total_ms = fn_start.elapsed().as_millis();

    debug!(
        pool = %pool.address,
        words_scanned,
        words_changed,
        words_hot_zone,
        ticks_refreshed = ticks_to_fetch.len(),
        ticks_restored,
        ticks_total = pool.ticks.len(),
        scan_ms,
        batch_fetch_ms = batch_ms,
        total_ms,
        "incremental_sync_v3_ticks breakdown"
    );

    // Save updated snapshot
    save_v3_tick_snapshot(cache, pool);

    Ok(())
}

/// Sync UniswapV3 tick data using smart range scanning.
///
/// Instead of scanning the entire tick range (which can be 6000+ RPC calls),
/// this function scans outward from the current tick position and stops when
/// it hits consecutive empty bitmap words. This typically reduces RPC calls
/// by 95%+ for most pools.
#[instrument(skip(cache), fields(pool = %pool.address, tick_spacing = pool.tick_spacing, current_tick = pool.tick))]
pub fn sync_uniswap_v3_ticks(
    cache: &mut EvmCache,
    pool: &mut UniswapV3Pool,
    flavor: V3Flavor,
) -> Result<()> {
    if pool.tick_spacing == 0 {
        return Ok(());
    }

    let fn_start = Instant::now();

    let min_word = tick_to_word(MIN_TICK, pool.tick_spacing);
    let max_word = tick_to_word(MAX_TICK, pool.tick_spacing);
    let current_word = tick_to_word(pool.tick, pool.tick_spacing);

    pool.tick_bitmap.clear();
    pool.ticks.clear();

    let scan_params = compute_adaptive_scan_params(pool.liquidity, pool.tick_spacing);
    debug!(
        pool = %pool.address,
        liquidity = pool.liquidity,
        tick_spacing = pool.tick_spacing,
        max_scan_words = scan_params.max_scan_words,
        empty_word_threshold = scan_params.empty_word_threshold,
        "adaptive scan params"
    );

    // Scan outward from current tick position using direct storage reads
    let mut words_scanned = 0;
    let mut words_with_liquidity = 0;
    let bitmap_base = flavor.tick_bitmap_base_slot();

    let scan_start = Instant::now();

    // Scan in positive direction (higher ticks)
    let mut consecutive_empty_up = 0;
    for offset in 0..=scan_params.max_scan_words {
        let word = current_word + offset;
        if word > max_word {
            break;
        }

        let slot = v3_tick_bitmap_storage_key_with_base(word as i16, bitmap_base);
        let bitmap = cache.read_storage_slot(pool.address, slot)?;

        words_scanned += 1;
        pool.tick_bitmap.insert(word as i16, bitmap);

        if bitmap == U256::ZERO {
            consecutive_empty_up += 1;
            if consecutive_empty_up >= scan_params.empty_word_threshold {
                break;
            }
        } else {
            consecutive_empty_up = 0;
            words_with_liquidity += 1;
        }
    }

    // Scan in negative direction (lower ticks), starting from current_word - 1
    let mut consecutive_empty_down = 0;
    for offset in 1..=scan_params.max_scan_words {
        let word = current_word - offset;
        if word < min_word {
            break;
        }

        let slot = v3_tick_bitmap_storage_key_with_base(word as i16, bitmap_base);
        let bitmap = cache.read_storage_slot(pool.address, slot)?;

        words_scanned += 1;
        pool.tick_bitmap.insert(word as i16, bitmap);

        if bitmap == U256::ZERO {
            consecutive_empty_down += 1;
            if consecutive_empty_down >= scan_params.empty_word_threshold {
                break;
            }
        } else {
            consecutive_empty_down = 0;
            words_with_liquidity += 1;
        }
    }

    let scan_ms = scan_start.elapsed().as_millis();

    // Extract initialized ticks from the scanned bitmaps
    let initialized_ticks = extract_initialized_ticks_from_bitmap(pool);

    // Fetch tick info using raw storage reads (slots 0+3 only).
    // Only liquidityGross, liquidityNet (slot 0) and initialized (slot 3) are needed.
    // If bitmap/tick info slots were prefetched, these reads hit BlockchainDb cache.
    let batch_start = Instant::now();
    let ticks_base = flavor.ticks_base_slot();
    for &tick in &initialized_ticks {
        let keys = v3_tick_info_storage_keys_with_base(tick, ticks_base);
        let slot0_value = cache.read_storage_slot(pool.address, keys[0])?;
        let limbs = slot0_value.as_limbs();
        let liquidity_gross: u128 = limbs[0] as u128 | ((limbs[1] as u128) << 64);
        let liquidity_net: i128 = (limbs[2] as u128 | ((limbs[3] as u128) << 64)) as i128;

        let slot3_value = cache.read_storage_slot(pool.address, keys[3])?;
        let initialized = (slot3_value >> 248) & U256::from(1u64) != U256::ZERO;

        pool.ticks.insert(
            tick,
            Info {
                liquidity_gross,
                liquidity_net,
                initialized,
            },
        );
    }
    let batch_ms = batch_start.elapsed().as_millis();

    let total_ms = fn_start.elapsed().as_millis();

    debug!(
        pool = %pool.address,
        words_scanned,
        words_with_liquidity,
        bitmap_words = pool.tick_bitmap.len(),
        ticks_loaded = pool.ticks.len(),
        initialized_ticks_found = initialized_ticks.len(),
        scan_ms,
        batch_fetch_ms = batch_ms,
        total_ms,
        "sync_uniswap_v3_ticks breakdown"
    );

    // Save tick snapshot for future restarts
    save_v3_tick_snapshot(cache, pool);

    Ok(())
}

/// Full tick range sync - scans the entire tick range.
///
/// Use this as a fallback when smart scanning might miss liquidity,
/// or for initial verification that the smart scan is working correctly.
#[instrument(skip(cache), fields(pool = %pool.address, tick_spacing = pool.tick_spacing))]
pub fn sync_uniswap_v3_ticks_full(cache: &mut EvmCache, pool: &mut UniswapV3Pool) -> Result<()> {
    if pool.tick_spacing == 0 {
        return Ok(());
    }

    let min_word = tick_to_word(MIN_TICK, pool.tick_spacing);
    let max_word = tick_to_word(MAX_TICK, pool.tick_spacing);

    pool.tick_bitmap.clear();
    pool.ticks.clear();

    warn!(
        pool = %pool.address,
        word_range = max_word - min_word + 1,
        "performing FULL tick bitmap scan - this will be slow"
    );

    for word in min_word..=max_word {
        let bitmap = call_view(
            cache,
            pool.address,
            IUniswapV3Pool::tickBitmapCall {
                wordPosition: word as i16,
            },
        )?;
        pool.tick_bitmap.insert(word as i16, bitmap);
    }

    let initialized_ticks = extract_initialized_ticks_from_bitmap(pool);

    // Batch fetch tick info using Multicall3
    if !initialized_ticks.is_empty() {
        fetch_tick_info_batched(cache, pool, &initialized_ticks)?;
    }

    debug!(
        bitmap_words = pool.tick_bitmap.len(),
        ticks_loaded = pool.ticks.len(),
        "synced V3 ticks (full scan + batched tick info)"
    );

    // Save tick snapshot for future restarts
    save_v3_tick_snapshot(cache, pool);

    Ok(())
}

/// Batch fetch tick info for multiple ticks using Multicall3.
///
/// This significantly reduces RPC calls when a pool has many initialized ticks.
/// Instead of N individual calls, this batches them into ceil(N/MAX_BATCH_SIZE) calls.
fn fetch_tick_info_batched(
    cache: &mut EvmCache,
    pool: &mut UniswapV3Pool,
    initialized_ticks: &[i32],
) -> Result<()> {
    use alloy_primitives::Bytes;
    use evm_fork_cache::multicall::execute_batched;

    if initialized_ticks.is_empty() {
        return Ok(());
    }

    // Build calls for batched execution
    let calls: Vec<(Address, Bytes, bool)> = initialized_ticks
        .iter()
        .filter_map(|&tick| {
            let tick_value = alloy_primitives::Signed::<24, 1>::try_from(tick as i128).ok()?;
            let call_data = IUniswapV3Pool::ticksCall { tick: tick_value }.abi_encode();
            Some((pool.address, Bytes::from(call_data), true))
        })
        .collect();

    // Execute with proper chunking (respects MAX_BATCH_SIZE)
    let results = execute_batched(cache, calls)?;

    // Process results
    let mut successful = 0;
    for (&tick, result) in initialized_ticks.iter().zip(results.iter()) {
        if result.success
            && let Ok(tick_info) = IUniswapV3Pool::ticksCall::abi_decode_returns(&result.returnData)
        {
            pool.ticks.insert(
                tick,
                Info {
                    liquidity_gross: tick_info.liquidityGross,
                    liquidity_net: tick_info.liquidityNet,
                    initialized: tick_info.initialized,
                },
            );
            successful += 1;
        }
    }

    debug!(
        requested = initialized_ticks.len(),
        successful, "fetched tick info via multicall"
    );

    Ok(())
}

/// Fetch tick info using raw storage reads (slots 0 and 3 only).
///
/// Instead of calling the `ticks()` getter (which reads all 4 Tick.Info slots),
/// this reads only slot 0 (liquidityGross + liquidityNet) and slot 3 (initialized flag)
/// directly via raw storage access. This cuts from 4 to 2 RPC calls per tick.
///
/// The raw reads go through `cache.read_storage_slot()` which populates BlockchainDb,
/// so subsequent EVM SLOADs will find the values cached there.
pub(super) fn fetch_tick_info_raw_storage(
    cache: &mut EvmCache,
    pool_address: Address,
    ticks_to_fetch: &[i32],
    flavor: V3Flavor,
    skip_purge: bool,
) -> Result<HashMap<i32, Info>> {
    let mut result = HashMap::with_capacity(ticks_to_fetch.len());

    if ticks_to_fetch.is_empty() {
        return Ok(result);
    }

    let ticks_base = flavor.ticks_base_slot();

    // Purge slots 0 and 3 for all ticks, then read fresh values
    // Skip purge when slots were already freshly fetched by parallel prefetch
    if !skip_purge {
        let slots_to_purge: Vec<U256> = ticks_to_fetch
            .iter()
            .flat_map(|&tick| {
                let keys = v3_tick_info_storage_keys_with_base(tick, ticks_base);
                [keys[0], keys[3]]
            })
            .collect();
        cache.purge_pool_slots(pool_address, &slots_to_purge);
    }

    let mut successful = 0;
    for &tick in ticks_to_fetch {
        let keys = v3_tick_info_storage_keys_with_base(tick, ticks_base);

        // Read slot 0: liquidityGross (lower 128 bits) | liquidityNet (upper 128 bits)
        // U256 limbs are in little-endian order: [bits 0-63, 64-127, 128-191, 192-255]
        let slot0_value = cache.read_storage_slot(pool_address, keys[0])?;
        let limbs = slot0_value.as_limbs();
        let liquidity_gross: u128 = limbs[0] as u128 | ((limbs[1] as u128) << 64);
        let liquidity_net: i128 = (limbs[2] as u128 | ((limbs[3] as u128) << 64)) as i128;

        // Read slot 3: initialized flag at bit 248
        let slot3_value = cache.read_storage_slot(pool_address, keys[3])?;
        let initialized = (slot3_value >> 248) & U256::from(1u64) != U256::ZERO;

        result.insert(
            tick,
            Info {
                liquidity_gross,
                liquidity_net,
                initialized,
            },
        );
        successful += 1;
    }

    debug!(
        pool = %pool_address,
        requested = ticks_to_fetch.len(),
        successful,
        "fetched tick info via raw storage (slots 0+3)"
    );

    Ok(result)
}

/// Refresh only the slot0 data (sqrtPrice, tick, liquidity) for a V3 pool.
///
/// This is useful for incremental updates where we don't need to re-scan
/// all tick data, just update the current pool state.
#[instrument(skip(cache), fields(pool = %pool.address))]
pub fn refresh_uniswap_v3_slot0(cache: &mut EvmCache, pool: &mut UniswapV3Pool) -> Result<()> {
    refresh_v3_slot0(cache, pool, V3Flavor::UniswapV3)
}

pub(super) fn refresh_v3_slot0(
    cache: &mut EvmCache,
    pool: &mut UniswapV3Pool,
    flavor: V3Flavor,
) -> Result<()> {
    // Selectively purge only slot0 and liquidity to force fresh reads from RPC.
    // This preserves tick bitmap and tick info data in the cache, which is critical
    // since this function is called every cycle for every V3 pool.
    cache.purge_pool_slots(pool.address, &[V3_SLOT0_SLOT, flavor.liquidity_slot()]);

    let slot0 = call_view(cache, pool.address, IUniswapV3Pool::slot0Call {})?;
    let liquidity = call_view(cache, pool.address, IUniswapV3Pool::liquidityCall {})?;

    let old_tick = pool.tick;
    pool.sqrt_price = slot0.sqrtPriceX96.to();
    pool.tick = slot0.tick.as_i32();
    pool.liquidity = liquidity;

    debug!(
        old_tick,
        new_tick = pool.tick,
        liquidity = %pool.liquidity,
        "refreshed V3 slot0"
    );

    Ok(())
}

/// Refresh the V3 pool state: slot0 + liquidity + hot-zone tick data.
///
/// This is a per-cycle refresh that goes beyond `refresh_uniswap_v3_slot0` by also
/// refreshing tick bitmap and tick info in the hot zone around the current tick.
/// This catches LP add/remove operations that change tick liquidity without
/// necessarily changing the pool's global liquidity or current tick.
///
/// The hot zone covers `cycle_hot_zone_radius()` words in each direction from
/// the current tick's word, ensuring that swap simulations crossing nearby
/// ticks use fresh liquidityGross/liquidityNet values.
#[instrument(skip(cache), fields(pool = %pool.address))]
pub fn refresh_uniswap_v3_state(cache: &mut EvmCache, pool: &mut UniswapV3Pool) -> Result<()> {
    refresh_v3_state(cache, pool, V3Flavor::UniswapV3)
}

pub fn refresh_pancakeswap_v3_state(cache: &mut EvmCache, pool: &mut UniswapV3Pool) -> Result<()> {
    refresh_v3_state(cache, pool, V3Flavor::PancakeSwapV3)
}

pub(super) fn refresh_v3_state(
    cache: &mut EvmCache,
    pool: &mut UniswapV3Pool,
    flavor: V3Flavor,
) -> Result<()> {
    let fn_start = Instant::now();

    // Step 1: Refresh slot0 + liquidity (existing behavior)
    refresh_v3_slot0(cache, pool, flavor)?;

    if pool.tick_spacing == 0 || pool.tick_bitmap.is_empty() {
        return Ok(());
    }

    // Step 2: Refresh tick bitmap + tick info in the hot zone
    let current_word = tick_to_word(pool.tick, pool.tick_spacing);

    let hot_radius = cycle_hot_zone_radius();
    let hot_zone_words: Vec<i16> = ((-hot_radius)..=hot_radius)
        .map(|offset| (current_word + offset) as i16)
        .collect();

    // Purge bitmap storage slots for the hot zone
    let bitmap_slots_to_purge: Vec<U256> = hot_zone_words
        .iter()
        .map(|&w| v3_tick_bitmap_storage_key_with_base(w, flavor.tick_bitmap_base_slot()))
        .collect();
    cache.purge_pool_slots(pool.address, &bitmap_slots_to_purge);

    // Fetch fresh bitmap values and compare with existing.
    // Only re-fetch tick info for words whose bitmap actually changed.
    let mut words_changed = 0;
    let mut ticks_to_fetch = Vec::new();

    for &word in &hot_zone_words {
        let fresh_bitmap = call_view(
            cache,
            pool.address,
            IUniswapV3Pool::tickBitmapCall { wordPosition: word },
        )?;

        let old_bitmap = pool.tick_bitmap.get(&word).copied().unwrap_or(U256::ZERO);
        pool.tick_bitmap.insert(word, fresh_bitmap);

        if fresh_bitmap != old_bitmap {
            words_changed += 1;
            // Bitmap changed: re-fetch all initialized ticks for this word
            let word_ticks =
                extract_ticks_from_bitmap_word(word as i32, fresh_bitmap, pool.tick_spacing);
            ticks_to_fetch.extend_from_slice(&word_ticks);
        }
        // Unchanged words: keep existing pool.ticks entries (no re-fetch needed)
    }

    // Fetch tick info via raw storage reads (slots 0+3 only -- 2 reads per tick)
    // Raw reads populate BlockchainDb directly, so EVM SLOADs will find fresh values
    if !ticks_to_fetch.is_empty() {
        let fresh_ticks =
            fetch_tick_info_raw_storage(cache, pool.address, &ticks_to_fetch, flavor, false)?;
        for (tick, info) in fresh_ticks {
            pool.ticks.insert(tick, info);
        }
    }

    // Update the tick snapshot with fresh data
    save_v3_tick_snapshot(cache, pool);

    let total_ms = fn_start.elapsed().as_millis();
    debug!(
        pool = %pool.address,
        hot_zone_words = hot_zone_words.len(),
        words_changed,
        ticks_refreshed = ticks_to_fetch.len(),
        total_ms,
        "refreshed V3 state with hot-zone tick data"
    );

    Ok(())
}

/// Targeted resync of V3 ticks affected by Mint/Burn events.
///
/// Instead of scanning the entire hot zone (7+ bitmap words), this function:
/// 1. Refreshes slot0 + liquidity (Mint/Burn can change active liquidity)
/// 2. Extends tick region if the tick drifted to an unfetched bitmap word
/// 3. Computes the bitmap words that overlap the affected tick ranges
/// 4. Purges and re-fetches only those specific bitmap words
/// 5. Re-fetches tick info for all initialized ticks in affected words
///    (Mint/Burn changes liquidityNet even when the bitmap bit stays set)
/// 6. Removes ticks that are no longer initialized (full Burn)
/// 7. Persists tick snapshot and injects into EVM cache
///
/// The `affected_tick_ranges` are `(tick_lower, tick_upper)` pairs from
/// Mint/Burn events. Most LP positions span only 1-3 bitmap words, so this
/// is dramatically cheaper than the full hot-zone resync.
#[instrument(skip(cache, affected_tick_ranges), fields(pool = %pool.address, ranges = affected_tick_ranges.len()))]
pub fn targeted_tick_resync(
    cache: &mut EvmCache,
    pool: &mut UniswapV3Pool,
    flavor: V3Flavor,
    affected_tick_ranges: &[(i32, i32)],
) -> Result<()> {
    targeted_tick_resync_inner(cache, pool, flavor, affected_tick_ranges, false)
}

/// Targeted tick resync that optionally skips the slot0+liquidity RPC refresh.
///
/// When `skip_slot0_refresh` is true, assumes slot0 and liquidity have already been
/// injected into the EVM cache (e.g., via `inject_hot_state_to_evm`), saving 1 RPC call.
pub fn targeted_tick_resync_with_injected_slot0(
    cache: &mut EvmCache,
    pool: &mut UniswapV3Pool,
    flavor: V3Flavor,
    affected_tick_ranges: &[(i32, i32)],
) -> Result<()> {
    targeted_tick_resync_inner(cache, pool, flavor, affected_tick_ranges, true)
}

fn targeted_tick_resync_inner(
    cache: &mut EvmCache,
    pool: &mut UniswapV3Pool,
    flavor: V3Flavor,
    affected_tick_ranges: &[(i32, i32)],
    skip_slot0_refresh: bool,
) -> Result<()> {
    if pool.tick_spacing == 0 || affected_tick_ranges.is_empty() {
        return Ok(());
    }

    let fn_start = Instant::now();

    // Step 0: Refresh slot0 + liquidity (skip if already injected from hot state)
    if !skip_slot0_refresh {
        refresh_v3_slot0(cache, pool, flavor)?;
    }

    // Step 0b: Check if tick moved to an unfetched bitmap word
    if needs_tick_resync(pool, pool.tick) {
        extend_v3_tick_region(cache, pool, flavor)?;
    }

    // Step 1: Compute affected bitmap words from all pending tick ranges
    let affected_words =
        collect_targeted_tick_words(pool.tick, pool.tick_spacing, affected_tick_ranges);

    let bitmap_base = flavor.tick_bitmap_base_slot();

    // Step 2: Purge affected bitmap storage slots
    let bitmap_slots: Vec<U256> = affected_words
        .iter()
        .map(|&w| v3_tick_bitmap_storage_key_with_base(w, bitmap_base))
        .collect();
    cache.purge_pool_slots(pool.address, &bitmap_slots);

    // Step 3: Fetch fresh bitmap values and extract initialized ticks
    let mut words_changed = 0;
    let mut ticks_to_fetch: Vec<i32> = Vec::new();

    for &word in &affected_words {
        let slot = v3_tick_bitmap_storage_key_with_base(word, bitmap_base);
        let fresh_bitmap = cache.read_storage_slot(pool.address, slot)?;
        let old_bitmap = pool.tick_bitmap.get(&word).copied().unwrap_or(U256::ZERO);
        pool.tick_bitmap.insert(word, fresh_bitmap);

        if fresh_bitmap != old_bitmap {
            words_changed += 1;
        }

        // Always re-fetch all initialized ticks in affected words because
        // Mint/Burn changes liquidityNet even when the bitmap bit stays set
        if fresh_bitmap != U256::ZERO {
            let word_ticks =
                extract_ticks_from_bitmap_word(word as i32, fresh_bitmap, pool.tick_spacing);
            ticks_to_fetch.extend_from_slice(&word_ticks);
        }
    }

    // Step 4: Fetch fresh tick info
    if !ticks_to_fetch.is_empty() {
        let fresh_ticks =
            fetch_tick_info_raw_storage(cache, pool.address, &ticks_to_fetch, flavor, false)?;
        for (tick, info) in fresh_ticks {
            pool.ticks.insert(tick, info);
        }
    }

    // Step 5: Remove ticks that are no longer initialized in affected words
    // (Burn can remove a position entirely, making a tick no longer initialized)
    let initialized_in_affected: HashSet<i32> = ticks_to_fetch.into_iter().collect();
    pool.ticks.retain(|&tick, _| {
        let tick_word = tick_to_word(tick, pool.tick_spacing) as i16;
        if affected_words.contains(&tick_word) {
            // Tick is in an affected word — keep only if still initialized
            initialized_in_affected.contains(&tick)
        } else {
            // Tick is outside affected words — unchanged, keep it
            true
        }
    });

    // Step 6: Persist and inject
    save_v3_tick_snapshot(cache, pool);
    inject_v3_tick_data(cache, pool.address, pool, flavor);

    let total_ms = fn_start.elapsed().as_millis();
    debug!(
        pool = %pool.address,
        ranges = affected_tick_ranges.len(),
        affected_words = affected_words.len(),
        words_changed,
        ticks_refreshed = initialized_in_affected.len(),
        total_ms,
        "targeted V3 tick resync complete"
    );

    Ok(())
}

// ============================================================================
// Parallel Prefetch Functions for V3 Pools
// ============================================================================

/// Result of a batch storage slot fetch, including per-address failure counts.
pub(super) struct BatchFetchResult {
    pub success_count: usize,
    pub error_count: usize,
    /// Number of failed slot fetches per address (only addresses with errors).
    pub errors_by_address: HashMap<Address, usize>,
    /// One representative error string per address.
    pub error_samples_by_address: HashMap<Address, String>,
}

/// Per-pool prefetch error stats returned by V3 prefetch helpers.
pub struct V3PrefetchStats {
    /// Number of failed slot fetches per pool address.
    pub errors_by_pool: HashMap<Address, usize>,
    /// Total storage slots requested per pool address.
    pub total_requested_by_pool: HashMap<Address, usize>,
    /// One representative error string per pool address.
    pub error_samples_by_pool: HashMap<Address, String>,
}

/// Batch-fetch storage slots using direct RPC calls when available,
/// falling back to SharedBackend-based adaptive prefetch.
///
/// When the batch fetcher is available, this bypasses SharedBackend's
/// per-request channel overhead and fires concurrent `eth_getStorageAt`
/// calls directly via the provider, injecting results into BlockchainDb in bulk.
/// This reduces 16K+ individual requests into ~160 concurrent batches of 100.
///
/// Failed slots are retried up to 2 times with exponential backoff to recover
/// transient 429s before they cascade to serial SharedBackend fallback.
async fn batch_fetch_storage_slots(
    cache: &mut EvmCache,
    requests: &[(Address, U256)],
    progress_label: &str,
) -> BatchFetchResult {
    if requests.is_empty() {
        return BatchFetchResult {
            success_count: 0,
            error_count: 0,
            errors_by_address: HashMap::new(),
            error_samples_by_address: HashMap::new(),
        };
    }

    if let Some(fetcher) = cache.storage_batch_fetcher().cloned() {
        const BATCH_RETRY_ATTEMPTS: usize = 2;
        const BATCH_RETRY_BASE_DELAY_MS: u64 = 500;

        let pb = progress_bar(requests.len() as u64, progress_label);
        let mut all_successes: Vec<(Address, U256, U256)> = Vec::with_capacity(requests.len());
        let mut pending: Vec<(Address, U256)> = requests.to_vec();
        let mut total_errors = 0usize;
        let mut last_failures: Vec<(Address, U256, String)> = Vec::new();

        for attempt in 0..=BATCH_RETRY_ATTEMPTS {
            if pending.is_empty() {
                break;
            }
            if attempt > 0 {
                let delay_ms = BATCH_RETRY_BASE_DELAY_MS * (1 << (attempt - 1));
                debug!(
                    attempt,
                    retry_slots = pending.len(),
                    delay_ms,
                    "retrying failed batch fetch slots"
                );
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
            }

            let results = fetcher(pending.clone(), None);
            let mut failed_this_round: Vec<(Address, U256, String)> = Vec::new();

            for (addr, slot, result) in results {
                if attempt == 0 {
                    pb.inc(1);
                }
                match result {
                    Ok(value) => all_successes.push((addr, slot, value)),
                    Err(err) => failed_this_round.push((addr, slot, err.to_string())),
                }
            }

            if failed_this_round.is_empty() {
                last_failures.clear();
                break;
            }
            total_errors = failed_this_round.len();
            pending = failed_this_round
                .iter()
                .map(|(addr, slot, _)| (*addr, *slot))
                .collect();
            last_failures = failed_this_round;
        }

        // Build per-address error counts from remaining failures
        let mut errors_by_address: HashMap<Address, usize> = HashMap::new();
        let mut error_samples_by_address: HashMap<Address, String> = HashMap::new();
        for (addr, _, sample) in last_failures {
            *errors_by_address.entry(addr).or_default() += 1;
            error_samples_by_address.entry(addr).or_insert(sample);
        }

        let success_count = all_successes.len();
        cache.inject_storage_batch(&all_successes);
        finish_with_message(
            &pb,
            &format!("{} fetched, {} failed", success_count, total_errors),
        );
        BatchFetchResult {
            success_count,
            error_count: total_errors,
            errors_by_address,
            error_samples_by_address,
        }
    } else {
        // Fallback: existing SharedBackend path with adaptive throttling
        let backend = cache
            .unchecked_backend()
            .with_blocking_mode(BlockingMode::Block);
        let result = super::adaptive_prefetch::run_adaptive_prefetch(
            requests,
            super::adaptive_prefetch::AdaptivePrefetchConfig::throttle_aware(),
            progress_label,
            |&(addr, slot), idx| {
                let backend = backend.clone();
                tokio::task::spawn_blocking(move || {
                    backend.storage_ref(addr, slot).map(|_| ()).map_err(|_| idx)
                })
            },
        )
        .await;
        BatchFetchResult {
            success_count: result.success_count,
            error_count: result.error_count,
            errors_by_address: HashMap::new(),
            error_samples_by_address: HashMap::new(),
        }
    }
}

/// Prefetch all tick bitmap storage slots for V3 pools that need a full resync.
///
/// For each pool in `targets`, computes the storage slot keys for all bitmap words
/// in the adaptive scan range (+-max_scan_words from center_tick), then fetches them
/// all in parallel via cloned SharedBackend. Results auto-populate BlockchainDb so
/// that subsequent `sync_uniswap_v3_ticks` calls find the values cached and complete
/// without individual RPC round-trips.
///
/// This is the key optimization for cold start: instead of ~13,600 sequential RPC calls
/// (~22 minutes), we fetch them in parallel via adaptive chunked prefetch.
#[instrument(skip(cache, targets), fields(pool_count = targets.len()))]
pub async fn prefetch_v3_bitmap_slots(
    cache: &mut EvmCache,
    targets: &[V3BitmapPrefetchTarget],
) -> Result<()> {
    if targets.is_empty() {
        return Ok(());
    }

    // Compute all (address, storage_slot) pairs needed across all pools
    let mut fetch_requests: Vec<(Address, U256)> = Vec::new();

    for target in targets {
        let current_word = tick_to_word(target.center_tick, target.tick_spacing);
        let min_word = tick_to_word(MIN_TICK, target.tick_spacing);
        let max_word = tick_to_word(MAX_TICK, target.tick_spacing);
        let bitmap_base = target.flavor.tick_bitmap_base_slot();

        // Positive direction (including current_word)
        for offset in 0..=target.max_scan_words {
            let word = current_word + offset;
            if word > max_word {
                break;
            }
            let slot = v3_tick_bitmap_storage_key_with_base(word as i16, bitmap_base);
            fetch_requests.push((target.address, slot));
        }

        // Negative direction
        for offset in 1..=target.max_scan_words {
            let word = current_word - offset;
            if word < min_word {
                break;
            }
            let slot = v3_tick_bitmap_storage_key_with_base(word as i16, bitmap_base);
            fetch_requests.push((target.address, slot));
        }
    }

    let total_slots = fetch_requests.len();
    if total_slots == 0 {
        return Ok(());
    }

    info!(
        pools = targets.len(),
        total_slots, "prefetching V3 bitmap storage slots in parallel"
    );

    let result = batch_fetch_storage_slots(cache, &fetch_requests, "Prefetching V3 bitmaps").await;

    debug!(
        result.success_count,
        result.error_count, total_slots, "V3 bitmap slot prefetch complete"
    );

    Ok(())
}

/// Prefetch V3 tick info storage slots in parallel.
///
/// After bitmap slots have been prefetched (via `prefetch_v3_bitmap_slots`), this function:
/// 1. Reads the cached bitmap words from BlockchainDb (instant, no RPC)
/// 2. Extracts initialized tick indices from the bitmaps
/// 3. Computes all 4 tick info storage slot keys per tick
/// 4. Prefetches them in parallel using spawn_blocking
///
/// This ensures that the subsequent `fetch_tick_info_batched` Multicall3 execution
/// finds all tick info SLOADs already cached in BlockchainDb.
#[instrument(skip(cache, targets), fields(pool_count = targets.len()))]
pub async fn prefetch_v3_tick_info_slots(
    cache: &mut EvmCache,
    targets: &[V3BitmapPrefetchTarget],
) -> Result<V3PrefetchStats> {
    if targets.is_empty() {
        return Ok(V3PrefetchStats {
            errors_by_pool: HashMap::new(),
            total_requested_by_pool: HashMap::new(),
            error_samples_by_pool: HashMap::new(),
        });
    }

    let backend = cache
        .unchecked_backend()
        .with_blocking_mode(BlockingMode::Block);

    // Phase 1: Read cached bitmaps (with early termination) and extract initialized ticks.
    // Only slots 0 and 3 per tick are needed (liquidityGross/Net + initialized flag).
    let mut tick_info_requests: Vec<(Address, U256)> = Vec::new();
    let mut total_ticks = 0usize;
    let mut total_requested_by_pool: HashMap<Address, usize> = HashMap::new();

    for target in targets {
        let center_word = tick_to_word(target.center_tick, target.tick_spacing);
        let min_word = tick_to_word(MIN_TICK, target.tick_spacing);
        let max_word = tick_to_word(MAX_TICK, target.tick_spacing);
        let bitmap_base = target.flavor.tick_bitmap_base_slot();
        let ticks_base = target.flavor.ticks_base_slot();

        let mut pool_ticks: Vec<i32> = Vec::new();

        // Helper: extract initialized ticks from a bitmap word
        let mut extract_ticks = |word: i32, bitmap: U256| {
            let limbs = bitmap.as_limbs();
            for (limb_idx, &limb) in limbs.iter().enumerate() {
                if limb == 0 {
                    continue;
                }
                let base_bit = (limb_idx * 64) as i32;
                let mut remaining = limb;
                while remaining != 0 {
                    let bit_in_limb = remaining.trailing_zeros() as i32;
                    remaining &= remaining - 1;
                    let bit = base_bit + bit_in_limb;
                    let tick_index = (word * 256 + bit) * target.tick_spacing;
                    pool_ticks.push(tick_index);
                }
            }
        };

        // Scan positive direction with early termination (mirrors sync_uniswap_v3_ticks)
        let mut consecutive_empty = 0usize;
        for offset in 0..=target.max_scan_words {
            let word = center_word + offset;
            if word > max_word {
                break;
            }
            let slot = v3_tick_bitmap_storage_key_with_base(word as i16, bitmap_base);
            let bitmap = match backend.storage_ref(target.address, slot) {
                Ok(val) => val,
                Err(_) => continue,
            };
            if bitmap == U256::ZERO {
                consecutive_empty += 1;
                if consecutive_empty >= target.empty_word_threshold {
                    break;
                }
            } else {
                consecutive_empty = 0;
                extract_ticks(word, bitmap);
            }
        }

        // Scan negative direction with early termination
        consecutive_empty = 0;
        for offset in 1..=target.max_scan_words {
            let word = center_word - offset;
            if word < min_word {
                break;
            }
            let slot = v3_tick_bitmap_storage_key_with_base(word as i16, bitmap_base);
            let bitmap = match backend.storage_ref(target.address, slot) {
                Ok(val) => val,
                Err(_) => continue,
            };
            if bitmap == U256::ZERO {
                consecutive_empty += 1;
                if consecutive_empty >= target.empty_word_threshold {
                    break;
                }
            } else {
                consecutive_empty = 0;
                extract_ticks(word, bitmap);
            }
        }

        // Only slots 0 and 3 per tick (liquidityGross/Net + initialized flag)
        for &tick in &pool_ticks {
            let keys = v3_tick_info_storage_keys_with_base(tick, ticks_base);
            tick_info_requests.push((target.address, keys[0]));
            tick_info_requests.push((target.address, keys[3]));
        }
        let requested_slots = pool_ticks.len() * 2;
        if requested_slots > 0 {
            total_requested_by_pool.insert(target.address, requested_slots);
        }

        total_ticks += pool_ticks.len();
    }

    if tick_info_requests.is_empty() {
        info!("no initialized ticks found — skipping tick info prefetch");
        return Ok(V3PrefetchStats {
            errors_by_pool: HashMap::new(),
            total_requested_by_pool,
            error_samples_by_pool: HashMap::new(),
        });
    }

    info!(
        pools = targets.len(),
        total_ticks,
        total_slots = tick_info_requests.len(),
        "prefetching V3 tick info storage slots in parallel"
    );

    let result =
        batch_fetch_storage_slots(cache, &tick_info_requests, "Prefetching V3 tick info").await;

    debug!(
        result.success_count,
        result.error_count,
        total_ticks,
        total_slots = tick_info_requests.len(),
        "V3 tick info slot prefetch complete"
    );

    Ok(V3PrefetchStats {
        errors_by_pool: result.errors_by_address,
        total_requested_by_pool,
        error_samples_by_pool: result.error_samples_by_address,
    })
}

/// Parallel prefetch for V3 pools needing incremental resync.
///
/// For each pool: computes the scan range (using old_bitmap for early termination),
/// purges stale bitmap storage slots, then parallel-prefetches fresh values.
/// After bitmap prefetch, reads the fresh bitmaps from cache, identifies ticks to
/// re-fetch (changed words + hot zone), and parallel-prefetches their tick info slots.
///
/// After this function returns, `incremental_sync_v3_ticks` can be called with
/// `pre_purged=true` and all reads will hit the BlockchainDb cache.
#[instrument(skip_all, fields(pool_count = pools.len()))]
pub async fn prefetch_v3_incremental_resync_slots(
    cache: &mut EvmCache,
    pools: &[V3IncrementalPrefetchTarget<'_>],
    // (address, flavor, tick_spacing, current_tick, liquidity, old_bitmap)
) -> Result<V3PrefetchStats> {
    if pools.is_empty() {
        return Ok(V3PrefetchStats {
            errors_by_pool: HashMap::new(),
            total_requested_by_pool: HashMap::new(),
            error_samples_by_pool: HashMap::new(),
        });
    }

    let min_tick_word = |ts: i32| tick_to_word(MIN_TICK, ts);
    let max_tick_word = |ts: i32| tick_to_word(MAX_TICK, ts);

    // Step 1: Compute scan ranges and collect all bitmap slot keys to purge+prefetch
    let mut bitmap_requests: Vec<(Address, U256)> = Vec::new();
    // Track per-pool scan words for tick info extraction later
    let mut pool_scan_words: Vec<Vec<i16>> = Vec::with_capacity(pools.len());

    for &(address, flavor, tick_spacing, current_tick, liquidity, old_bitmap) in pools {
        if tick_spacing == 0 {
            pool_scan_words.push(Vec::new());
            continue;
        }
        let current_word = tick_to_word(current_tick, tick_spacing);
        let min_word = min_tick_word(tick_spacing);
        let max_word = max_tick_word(tick_spacing);
        let bitmap_base = flavor.tick_bitmap_base_slot();
        let scan_params = compute_adaptive_scan_params(liquidity, tick_spacing);

        let mut words_to_scan = Vec::new();

        // Positive direction with early termination using old_bitmap
        let mut consecutive_empty = 0usize;
        for offset in 0..=scan_params.max_scan_words {
            let word = current_word + offset;
            if word > max_word {
                break;
            }
            words_to_scan.push(word as i16);
            let cached_val = old_bitmap
                .get(&(word as i16))
                .copied()
                .unwrap_or(U256::ZERO);
            if cached_val == U256::ZERO {
                consecutive_empty += 1;
                if consecutive_empty >= scan_params.empty_word_threshold {
                    break;
                }
            } else {
                consecutive_empty = 0;
            }
        }

        // Negative direction
        consecutive_empty = 0;
        for offset in 1..=scan_params.max_scan_words {
            let word = current_word - offset;
            if word < min_word {
                break;
            }
            words_to_scan.push(word as i16);
            let cached_val = old_bitmap
                .get(&(word as i16))
                .copied()
                .unwrap_or(U256::ZERO);
            if cached_val == U256::ZERO {
                consecutive_empty += 1;
                if consecutive_empty >= scan_params.empty_word_threshold {
                    break;
                }
            } else {
                consecutive_empty = 0;
            }
        }

        // Collect bitmap slot keys
        for &w in &words_to_scan {
            let slot = v3_tick_bitmap_storage_key_with_base(w, bitmap_base);
            bitmap_requests.push((address, slot));
        }

        pool_scan_words.push(words_to_scan);
    }

    if bitmap_requests.is_empty() {
        return Ok(V3PrefetchStats {
            errors_by_pool: HashMap::new(),
            total_requested_by_pool: HashMap::new(),
            error_samples_by_pool: HashMap::new(),
        });
    }

    // Step 2: Purge stale bitmap slots
    {
        let mut purge_by_address: HashMap<Address, Vec<U256>> = HashMap::new();
        for &(addr, slot) in &bitmap_requests {
            purge_by_address.entry(addr).or_default().push(slot);
        }
        for (addr, slots) in &purge_by_address {
            cache.purge_pool_slots(*addr, slots);
        }
    }

    // Step 3: Parallel-prefetch fresh bitmap values
    let total_bitmap_slots = bitmap_requests.len();
    info!(
        pools = pools.len(),
        total_bitmap_slots, "prefetching incremental resync bitmap slots in parallel"
    );

    let _bitmap_result =
        batch_fetch_storage_slots(cache, &bitmap_requests, "Prefetching incremental bitmaps").await;

    let backend = cache
        .unchecked_backend()
        .with_blocking_mode(BlockingMode::Block);

    // Step 4: Read cached fresh bitmaps and identify ticks to prefetch
    // Compare fresh vs old bitmaps, extract ticks from changed words + hot zone
    let mut tick_info_requests: Vec<(Address, U256)> = Vec::new();
    let mut total_ticks = 0usize;
    let mut pool_errors: HashMap<Address, usize> = HashMap::new();
    let mut total_requested_by_pool: HashMap<Address, usize> = HashMap::new();
    let mut pool_error_samples: HashMap<Address, String> = HashMap::new();

    for (i, &(address, flavor, tick_spacing, current_tick, _liquidity, old_bitmap)) in
        pools.iter().enumerate()
    {
        if tick_spacing == 0 {
            continue;
        }
        let bitmap_base = flavor.tick_bitmap_base_slot();
        let ticks_base = flavor.ticks_base_slot();
        let current_word = tick_to_word(current_tick, tick_spacing);
        let mut pool_ticks: Vec<i32> = Vec::new();

        for &word in &pool_scan_words[i] {
            let slot = v3_tick_bitmap_storage_key_with_base(word, bitmap_base);
            let fresh_bitmap = match backend.storage_ref(address, slot) {
                Ok(val) => val,
                Err(_) => continue,
            };

            let cached_val = old_bitmap.get(&word).copied().unwrap_or(U256::ZERO);
            let word_i32 = word as i32;
            let in_hot_zone =
                (word_i32 - current_word).unsigned_abs() < incremental_hot_zone_radius() as u32;

            // Re-fetch ticks for hot zone words and changed words
            if in_hot_zone || fresh_bitmap != cached_val {
                let limbs = fresh_bitmap.as_limbs();
                for (limb_idx, &limb) in limbs.iter().enumerate() {
                    if limb == 0 {
                        continue;
                    }
                    let base_bit = (limb_idx * 64) as i32;
                    let mut remaining = limb;
                    while remaining != 0 {
                        let bit_in_limb = remaining.trailing_zeros() as i32;
                        remaining &= remaining - 1;
                        let bit = base_bit + bit_in_limb;
                        let tick_index = (word_i32 * 256 + bit) * tick_spacing;
                        pool_ticks.push(tick_index);
                    }
                }
            }
        }

        // Only slots 0 and 3 per tick
        let pool_slot_count = pool_ticks.len() * 2;
        for &tick in &pool_ticks {
            let keys = v3_tick_info_storage_keys_with_base(tick, ticks_base);
            tick_info_requests.push((address, keys[0]));
            tick_info_requests.push((address, keys[3]));
        }
        if pool_slot_count > 0 {
            *total_requested_by_pool.entry(address).or_default() += pool_slot_count;
        }
        total_ticks += pool_ticks.len();
    }

    // Step 4b: Purge stale tick info slots before prefetching.
    // For NeedsIncrementalResync pools, old tick info from evm_state.json may still
    // be cached in BlockchainDb. Purging ensures storage_ref() fetches fresh values.
    if !tick_info_requests.is_empty() {
        let mut tick_purge_by_address: HashMap<Address, Vec<U256>> = HashMap::new();
        for &(addr, slot) in &tick_info_requests {
            tick_purge_by_address.entry(addr).or_default().push(slot);
        }
        for (addr, slots) in &tick_purge_by_address {
            cache.purge_pool_slots(*addr, slots);
        }
    }

    // Step 5: Parallel-prefetch tick info slots
    if !tick_info_requests.is_empty() {
        info!(
            total_ticks,
            total_slots = tick_info_requests.len(),
            "prefetching incremental resync tick info slots in parallel"
        );

        let tick_result = batch_fetch_storage_slots(
            cache,
            &tick_info_requests,
            "Prefetching incremental tick info",
        )
        .await;

        debug!(
            tick_result.success_count,
            tick_result.error_count,
            total_ticks,
            total_slots = tick_info_requests.len(),
            "incremental tick info prefetch complete"
        );

        pool_errors = tick_result.errors_by_address;
        pool_error_samples = tick_result.error_samples_by_address;
    }

    debug!(
        bitmap_slots = total_bitmap_slots,
        tick_info_ticks = total_ticks,
        "incremental resync prefetch complete"
    );

    Ok(V3PrefetchStats {
        errors_by_pool: pool_errors,
        total_requested_by_pool,
        error_samples_by_pool: pool_error_samples,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, RwLock};

    use alloy_eips::BlockId;
    use alloy_primitives::Bytes;
    use alloy_primitives::hex;
    use alloy_provider::{RootProvider, network::AnyNetwork};
    use alloy_rpc_client::RpcClient;
    use alloy_transport::mock::Asserter;
    use foundry_fork_db::{BlockchainDb, SharedBackend, cache::BlockchainDbMeta};
    use revm::primitives::hardfork::SpecId;
    use revm::state::{AccountInfo, Bytecode};

    use crate::cache_sync::sync_all_pool_state_parallel;

    use crate::amm_wrapper::LocalAMM;
    use crate::cache_sync::AMMRef;

    const V3_TEST_POOL_RUNTIME_HEX: &str = "5f3560e01c80631a68650214604d57633850c7bd14601b575f80fd5b5f5460018060a01b0381169060a01c60020b905f526020525f6040525f6060525f6080525f60a052600160c05260e05ff35b6004545f5260205ff3";

    async fn setup_mock_cache() -> (EvmCache, Asserter) {
        let asserter = Asserter::new();
        let client = RpcClient::mocked(asserter.clone());
        let provider = Arc::new(RootProvider::<AnyNetwork>::new(client));
        let blockchain_db = BlockchainDb::new(BlockchainDbMeta::default(), None);
        let backend = SharedBackend::spawn_backend(provider, blockchain_db.clone(), None).await;
        (
            EvmCache::from_backend(
                backend,
                blockchain_db,
                BlockId::latest(),
                42161,
                None,
                None,
                SpecId::CANCUN,
            ),
            asserter,
        )
    }

    fn install_stub_v3_pool(cache: &mut EvmCache, pool: Address, token0: Address, token1: Address) {
        let runtime = Bytecode::new_raw(Bytes::from(
            hex::decode(V3_TEST_POOL_RUNTIME_HEX).expect("valid V3 test runtime"),
        ));
        let code_hash = runtime.hash_slow();
        cache
            .db_mut()
            .insert_account_info(Address::ZERO, AccountInfo::default());
        cache
            .db_mut()
            .insert_account_info(token0, AccountInfo::default());
        cache
            .db_mut()
            .insert_account_info(token1, AccountInfo::default());
        cache.db_mut().insert_account_info(
            pool,
            AccountInfo {
                balance: U256::ZERO,
                nonce: 0,
                code: Some(runtime),
                code_hash,
                account_id: None,
            },
        );
    }

    fn seed_backend_storage(cache: &mut EvmCache, address: Address, slot: U256, value: U256) {
        cache.with_blockchain_db_mut(|db| {
            db.storage()
                .write()
                .entry(address)
                .or_default()
                .insert(slot, value);
        });
    }

    fn seed_v3_metadata(cache: &mut EvmCache, address: Address, token0: Address, token1: Address) {
        cache.immutable_cache_mut().set_v3_pool(
            address,
            V3PoolMetadata {
                token0,
                token1,
                fee: 3000,
                tick_spacing: 60,
            },
        );
    }

    fn encode_v3_slot0(sqrt_price: U256, tick: i32) -> U256 {
        let tick_bits = U256::from((tick as u32 & 0x00FF_FFFF) as u64);
        sqrt_price | (tick_bits << 160)
    }

    fn encode_tick_info_slots(info: &Info) -> (U256, U256) {
        let slot0 =
            U256::from(info.liquidity_gross) | (U256::from(info.liquidity_net as u128) << 128);
        let slot3 = if info.initialized {
            U256::from(1u64) << 248
        } else {
            U256::ZERO
        };
        (slot0, slot3)
    }

    fn bitmap_word_for_ticks(ticks: &[i32], tick_spacing: i32) -> (i16, U256) {
        let word = tick_to_word(*ticks.first().expect("at least one tick"), tick_spacing) as i16;
        let mut bitmap = U256::ZERO;
        for &tick in ticks {
            assert_eq!(tick_to_word(tick, tick_spacing) as i16, word);
            let compressed = tick.div_euclid(tick_spacing);
            let bit = compressed.rem_euclid(256) as u32;
            bitmap |= U256::from(1u64) << bit;
        }
        (word, bitmap)
    }

    fn assert_tick_info_eq(actual: &Info, expected: &Info) {
        assert_eq!(actual.liquidity_gross, expected.liquidity_gross);
        assert_eq!(actual.liquidity_net, expected.liquidity_net);
        assert_eq!(actual.initialized, expected.initialized);
    }

    fn make_test_pool(
        address: Address,
        token0: Address,
        token1: Address,
        tick: i32,
        liquidity: u128,
        tick_bitmap: HashMap<i16, U256>,
        ticks: HashMap<i32, Info>,
    ) -> UniswapV3Pool {
        UniswapV3Pool {
            address,
            token_a: Token::new_with_decimals(token0, 18),
            token_b: Token::new_with_decimals(token1, 18),
            fee: 3000,
            tick_spacing: 60,
            sqrt_price: U256::from(1_000u64),
            tick,
            liquidity,
            tick_bitmap,
            ticks,
        }
    }

    #[test]
    fn test_can_reuse_v3_tick_snapshot_when_liquidity_matches() {
        assert!(can_reuse_v3_tick_snapshot(1_000, 1_000, 12_000, 12_900));
        assert!(can_reuse_v3_tick_snapshot(
            1_000,
            1_000,
            12_000,
            12_000 + MAX_TICK_DRIFT_FOR_CACHE
        ));
    }

    #[test]
    fn test_cannot_reuse_v3_tick_snapshot_on_liquidity_or_large_tick_drift() {
        assert!(!can_reuse_v3_tick_snapshot(1_000, 999, 12_000, 12_000));
        assert!(!can_reuse_v3_tick_snapshot(
            1_000,
            1_000,
            12_000,
            12_001 + MAX_TICK_DRIFT_FOR_CACHE
        ));
    }

    #[test]
    fn test_collect_targeted_tick_words_includes_current_word_and_deduplicates() {
        let words = collect_targeted_tick_words(120, 60, &[(60, 120), (120, 240), (60, 120)]);

        let expected: HashSet<i16> = [
            tick_to_word(60, 60) as i16,
            tick_to_word(120, 60) as i16,
            tick_to_word(240, 60) as i16,
        ]
        .into_iter()
        .collect();

        assert_eq!(words, expected);
    }

    #[test]
    fn test_collect_targeted_tick_words_handles_negative_ranges() {
        let words = collect_targeted_tick_words(-180, 60, &[(-360, -60)]);

        assert!(words.contains(&(tick_to_word(-360, 60) as i16)));
        assert!(words.contains(&(tick_to_word(-180, 60) as i16)));
        assert!(words.contains(&(tick_to_word(-60, 60) as i16)));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_init_uniswap_v3_from_cache_reuses_snapshot_without_bitmap_refresh() -> Result<()>
    {
        let (mut cache, asserter) = setup_mock_cache().await;
        let pool_address = Address::repeat_byte(0x11);
        let token0 = Address::repeat_byte(0x22);
        let token1 = Address::repeat_byte(0x33);
        let current_tick = 90;
        let live_tick = 95;
        let live_liquidity = 500u128;

        install_stub_v3_pool(&mut cache, pool_address, token0, token1);
        seed_v3_metadata(&mut cache, pool_address, token0, token1);

        let (word, bitmap) = bitmap_word_for_ticks(&[60, 120], 60);
        let mut cached_bitmap = HashMap::new();
        cached_bitmap.insert(word, bitmap);

        let mut cached_ticks = HashMap::new();
        cached_ticks.insert(60, Info::new(111, 11, true));
        cached_ticks.insert(120, Info::new(222, -22, true));

        cache.tick_snapshot_cache_mut().set(
            pool_address,
            V3PoolTickSnapshot::from_pool_data(
                &cached_bitmap,
                &info_ticks_to_cache(&cached_ticks),
                live_liquidity,
                current_tick,
            ),
        );
        seed_backend_storage(
            &mut cache,
            pool_address,
            v3_tick_bitmap_storage_key_with_base(word, V3_TICK_BITMAP_BASE_SLOT),
            bitmap,
        );

        asserter.push_success(&encode_v3_slot0(U256::from(2_000u64), live_tick));
        asserter.push_success(&U256::from(live_liquidity));

        let pool = init_uniswap_v3_from_cache(&mut cache, pool_address).await?;

        assert_eq!(pool.tick, live_tick);
        assert_eq!(pool.liquidity, live_liquidity);
        assert_eq!(pool.tick_bitmap.len(), 1);
        assert_eq!(pool.tick_bitmap.get(&word), Some(&bitmap));
        assert_eq!(pool.ticks.len(), 2);
        assert_tick_info_eq(pool.ticks.get(&60).unwrap(), cached_ticks.get(&60).unwrap());
        assert_tick_info_eq(
            pool.ticks.get(&120).unwrap(),
            cached_ticks.get(&120).unwrap(),
        );
        assert!(asserter.read_q().is_empty());

        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_targeted_tick_resync_updates_only_affected_words_and_ticks() -> Result<()> {
        let (mut cache, asserter) = setup_mock_cache().await;
        let pool_address = Address::repeat_byte(0x44);
        let token0 = Address::repeat_byte(0x55);
        let token1 = Address::repeat_byte(0x66);

        install_stub_v3_pool(&mut cache, pool_address, token0, token1);

        let (word, old_bitmap) = bitmap_word_for_ticks(&[60, 120], 60);
        let mut tick_bitmap = HashMap::new();
        tick_bitmap.insert(word, old_bitmap);

        let old_tick_60 = Info::new(111, 11, true);
        let old_tick_120 = Info::new(222, -22, true);
        let mut ticks = HashMap::new();
        ticks.insert(60, old_tick_60);
        ticks.insert(120, old_tick_120);

        let mut pool = make_test_pool(pool_address, token0, token1, 90, 500, tick_bitmap, ticks);
        let (_, new_bitmap) = bitmap_word_for_ticks(&[120], 60);
        let new_tick_120 = Info::new(333, -44, true);
        let (tick_slot0, tick_slot3) = encode_tick_info_slots(&new_tick_120);

        asserter.push_success(&encode_v3_slot0(U256::from(3_000u64), 90));
        asserter.push_success(&U256::from(600u128));
        asserter.push_success(&new_bitmap);
        asserter.push_success(&tick_slot0);
        asserter.push_success(&tick_slot3);

        targeted_tick_resync(&mut cache, &mut pool, V3Flavor::UniswapV3, &[(60, 120)])?;

        assert_eq!(pool.liquidity, 600);
        assert_eq!(pool.sqrt_price, U256::from(3_000u64));
        assert_eq!(pool.tick_bitmap.get(&word), Some(&new_bitmap));
        assert!(!pool.ticks.contains_key(&60));
        assert_eq!(pool.ticks.len(), 1);
        assert_tick_info_eq(pool.ticks.get(&120).unwrap(), &new_tick_120);

        let snapshot = cache.tick_snapshot_cache().get(pool_address).unwrap();
        assert_eq!(snapshot.last_liquidity, 600);
        assert_eq!(snapshot.last_tick, 90);
        assert_eq!(snapshot.ticks.len(), 1);
        assert!(asserter.read_q().is_empty());

        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_sync_all_pool_state_parallel_skips_v3_hot_zone_when_liquidity_matches()
    -> Result<()> {
        let (mut cache, asserter) = setup_mock_cache().await;
        let pool_address = Address::repeat_byte(0x77);
        let token0 = Address::repeat_byte(0x88);
        let token1 = Address::repeat_byte(0x99);
        let current_tick = 90;
        let new_tick = 95;

        let (word, bitmap) = bitmap_word_for_ticks(&[60, 120], 60);
        let mut tick_bitmap = HashMap::new();
        tick_bitmap.insert(word, bitmap);

        let mut ticks = HashMap::new();
        ticks.insert(60, Info::new(111, 11, true));
        ticks.insert(120, Info::new(222, -22, true));

        let pool = make_test_pool(
            pool_address,
            token0,
            token1,
            current_tick,
            500,
            tick_bitmap.clone(),
            ticks.clone(),
        );

        asserter.push_success(&encode_v3_slot0(U256::from(4_000u64), new_tick));
        asserter.push_success(&U256::from(500u128));

        let amm_ref: AMMRef = Arc::new(RwLock::new(LocalAMM::UniswapV3(pool)));
        let mut amms = HashMap::new();
        amms.insert(pool_address, amm_ref.clone());

        sync_all_pool_state_parallel(&mut cache, &mut amms).await?;

        let guard = amm_ref.read().unwrap();
        let refreshed = match &*guard {
            LocalAMM::UniswapV3(pool) => pool,
            other => panic!("unexpected AMM type after refresh: {other:?}"),
        };
        assert_eq!(refreshed.tick, new_tick);
        assert_eq!(refreshed.liquidity, 500);
        assert_eq!(refreshed.sqrt_price, U256::from(4_000u64));
        assert_eq!(refreshed.tick_bitmap.get(&word), Some(&bitmap));
        assert_eq!(refreshed.ticks.len(), 2);
        assert_tick_info_eq(refreshed.ticks.get(&60).unwrap(), ticks.get(&60).unwrap());
        assert_tick_info_eq(refreshed.ticks.get(&120).unwrap(), ticks.get(&120).unwrap());

        let snapshot = cache.tick_snapshot_cache().get(pool_address).unwrap();
        assert_eq!(snapshot.last_liquidity, 500);
        assert_eq!(snapshot.last_tick, new_tick);
        assert!(asserter.read_q().is_empty());

        Ok(())
    }
}
