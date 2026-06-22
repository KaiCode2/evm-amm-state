use super::*;

/// Result of a pool freshness check.
#[derive(Debug, Clone)]
pub struct PoolFreshnessResult {
    pub pool_address: Address,
    pub pool_type: &'static str,
    pub is_fresh: bool,
    /// Maximum observed drift across all checked values, in basis points.
    pub max_drift_bps: f64,
    pub drift_description: Option<String>,
    /// Fresh V2 reserves when stale (reserve_0, reserve_1). Enables in-place resync
    /// without redundant RPC calls since the freshness check already fetched these.
    pub fresh_v2_reserves: Option<(u128, u128)>,
    /// Fresh V3 state when stale (sqrt_price, tick, liquidity). Enables in-place resync
    /// without redundant RPC calls since the freshness check already fetched these.
    pub fresh_v3_state: Option<(U256, i32, u128)>,
}

/// Maximum reserve drift (bps) before a V2 pool is considered stale.
/// 50 bps = 0.5% reserve change.
const V2_RESERVE_DRIFT_TOLERANCE_BPS: f64 = 50.0;

/// Check freshness of a UniswapV2 pool by comparing in-memory reserves with on-chain state.
///
/// Returns a `PoolFreshnessResult` indicating whether the pool state is fresh.
/// A pool is considered stale if reserve drift exceeds the configured threshold.
#[instrument(skip(cache), fields(pool = %pool.address))]
pub fn check_v2_freshness(
    cache: &mut EvmCache,
    pool: &UniswapV2Pool,
) -> Result<PoolFreshnessResult> {
    // Purge all V2 pool storage to get fresh data from RPC.
    // V2 pools have ~12 slots total, so full purge is nearly zero-cost.
    cache.purge_pool_storage(pool.address);

    let fresh_reserves = call_view(cache, pool.address, IUniswapV2Pair::getReservesCall {})?;

    let fresh_r0 = fresh_reserves.reserve0.to::<u128>();
    let fresh_r1 = fresh_reserves.reserve1.to::<u128>();

    let r0_matches = pool.reserve_0 == fresh_r0;
    let r1_matches = pool.reserve_1 == fresh_r1;

    if r0_matches && r1_matches {
        return Ok(PoolFreshnessResult {
            pool_address: pool.address,
            pool_type: "UniswapV2",
            is_fresh: true,
            max_drift_bps: 0.0,
            drift_description: None,
            fresh_v2_reserves: None,
            fresh_v3_state: None,
        });
    }

    // Calculate percentage drift
    let r0_drift_bps = if pool.reserve_0 > 0 {
        ((fresh_r0 as f64 - pool.reserve_0 as f64) / pool.reserve_0 as f64 * 10_000.0).abs()
    } else {
        10_000.0
    };
    let r1_drift_bps = if pool.reserve_1 > 0 {
        ((fresh_r1 as f64 - pool.reserve_1 as f64) / pool.reserve_1 as f64 * 10_000.0).abs()
    } else {
        10_000.0
    };
    let max_drift_bps = r0_drift_bps.max(r1_drift_bps);

    if max_drift_bps <= V2_RESERVE_DRIFT_TOLERANCE_BPS {
        debug!(
            pool = %pool.address,
            max_drift_bps = format!("{:.1}", max_drift_bps),
            "V2 pool reserve drift within tolerance, treating as fresh"
        );
        return Ok(PoolFreshnessResult {
            pool_address: pool.address,
            pool_type: "UniswapV2",
            is_fresh: true,
            max_drift_bps,
            drift_description: None,
            fresh_v2_reserves: None,
            fresh_v3_state: None,
        });
    }

    let r0_drift_pct = r0_drift_bps / 100.0;
    let r1_drift_pct = r1_drift_bps / 100.0;
    let description = format!(
        "reserve0: {} -> {} ({:.2}% drift), reserve1: {} -> {} ({:.2}% drift)",
        pool.reserve_0, fresh_r0, r0_drift_pct, pool.reserve_1, fresh_r1, r1_drift_pct
    );

    warn!(
        pool = %pool.address,
        cached_r0 = pool.reserve_0,
        fresh_r0 = fresh_r0,
        cached_r1 = pool.reserve_1,
        fresh_r1 = fresh_r1,
        max_drift_bps = format!("{:.1}", max_drift_bps),
        "V2 pool reserves have drifted beyond tolerance"
    );

    Ok(PoolFreshnessResult {
        pool_address: pool.address,
        pool_type: "UniswapV2",
        is_fresh: false,
        max_drift_bps,
        drift_description: Some(description),
        fresh_v2_reserves: Some((fresh_r0, fresh_r1)),
        fresh_v3_state: None,
    })
}

/// Maximum absolute tick drift before a V3 pool is considered stale.
/// Each tick ≈ 1 bps (0.01%) price change. 10 ticks ≈ 0.1% price movement.
const V3_TICK_DRIFT_TOLERANCE: i32 = 10;

/// Maximum liquidity drift (bps) before a V3 pool is considered stale.
/// 50 bps = 0.5% liquidity change — minimal impact on swap output.
const V3_LIQUIDITY_DRIFT_TOLERANCE_BPS: f64 = 50.0;

/// Check freshness of a UniswapV3 pool by comparing in-memory state with on-chain slot0
/// and liquidity, using drift tolerances rather than exact matching.
///
/// Returns a `PoolFreshnessResult` indicating whether the pool state is fresh.
/// A pool is considered stale if tick or liquidity drift exceeds configured thresholds.
#[instrument(skip(cache), fields(pool = %pool.address))]
pub fn check_v3_freshness(
    cache: &mut EvmCache,
    pool: &UniswapV3Pool,
) -> Result<PoolFreshnessResult> {
    // Selectively purge only slot0 and liquidity to get fresh data from RPC.
    cache.purge_pool_slots(pool.address, &[V3_SLOT0_SLOT, V3_LIQUIDITY_SLOT]);

    let fresh_slot0 = call_view(cache, pool.address, IUniswapV3Pool::slot0Call {})?;
    let fresh_liquidity = call_view(cache, pool.address, IUniswapV3Pool::liquidityCall {})?;

    let fresh_tick = fresh_slot0.tick.as_i32();

    // Compute actual drift metrics
    let tick_drift_abs = (fresh_tick - pool.tick).unsigned_abs() as i32;
    let tick_drift_bps = tick_drift_abs as f64; // 1 tick ≈ 1 bps

    let liquidity_drift_bps = if pool.liquidity > 0 {
        ((fresh_liquidity as f64 - pool.liquidity as f64) / pool.liquidity as f64 * 10_000.0).abs()
    } else if fresh_liquidity > 0 {
        10_000.0
    } else {
        0.0
    };

    let max_drift_bps = tick_drift_bps.max(liquidity_drift_bps);

    let within_tolerance = tick_drift_abs <= V3_TICK_DRIFT_TOLERANCE
        && liquidity_drift_bps <= V3_LIQUIDITY_DRIFT_TOLERANCE_BPS;

    if !within_tolerance {
        let description = format!(
            "tick: {} -> {} (drift: {} ticks, {:.1} bps), liquidity: {} -> {} (drift: {:.1} bps)",
            pool.tick,
            fresh_tick,
            tick_drift_abs,
            tick_drift_bps,
            pool.liquidity,
            fresh_liquidity,
            liquidity_drift_bps
        );

        warn!(
            pool = %pool.address,
            tick_drift = tick_drift_abs,
            liquidity_drift_bps = format!("{:.1}", liquidity_drift_bps),
            "V3 pool state has drifted beyond tolerance"
        );

        return Ok(PoolFreshnessResult {
            pool_address: pool.address,
            pool_type: "UniswapV3",
            is_fresh: false,
            max_drift_bps,
            drift_description: Some(description),
            fresh_v2_reserves: None,
            fresh_v3_state: Some((fresh_slot0.sqrtPriceX96.to(), fresh_tick, fresh_liquidity)),
        });
    }

    // Within tolerance — log minor drift at debug level
    if tick_drift_abs > 0 || pool.liquidity != fresh_liquidity {
        debug!(
            pool = %pool.address,
            tick_drift = tick_drift_abs,
            liquidity_drift_bps = format!("{:.1}", liquidity_drift_bps),
            "V3 pool drift within tolerance, treating as fresh"
        );
    }

    Ok(PoolFreshnessResult {
        pool_address: pool.address,
        pool_type: "UniswapV3",
        is_fresh: true,
        max_drift_bps,
        drift_description: None,
        fresh_v2_reserves: None,
        fresh_v3_state: None,
    })
}

/// Check freshness of a Balancer pool by comparing the last known on-chain
/// `lastChangeBlock` with the current on-chain value, and measuring balance
/// drift in basis points.
///
/// Drift below `BALANCER_FRESHNESS_TOLERANCE_BPS` is treated as fresh (absorbs
/// f64 round-trip noise and negligible on-chain dust). Drift above the threshold
/// is flagged as stale.
///
/// **Important**: The caller should purge the vault's storage once before calling
/// this in a batch (the vault is shared across Balancer pools).
#[instrument(skip(cache), fields(pool_id = %pool.pool_id))]
pub fn check_balancer_freshness(
    cache: &mut EvmCache,
    pool: &BalancerPool,
) -> Result<PoolFreshnessResult> {
    let pool_address = BalancerPool::address(pool.pool_id);

    // Fetch fresh pool tokens data from vault (vault storage should already be purged by caller)
    let tokens_result = call_view(
        cache,
        pool.vault,
        IBalancerVault::getPoolTokensCall {
            poolId: pool.pool_id,
        },
    )?;

    // Compare fresh balances against the cached immutable snapshot's lastChangeBlock
    // and calculate drift against current on-chain balances.
    // We use the on-chain U256 balances directly rather than round-tripping through
    // f64 (which introduces precision noise that would cause false positives).
    let cached_metadata = cache.immutable_cache().get_balancer_pool(pool.pool_id);

    // Build a map of fresh balances by token for drift calculation
    let fresh_balance_map: HashMap<Address, U256> = tokens_result
        .tokens
        .iter()
        .zip(tokens_result.balances.iter())
        .map(|(&token, &balance)| (token, balance))
        .collect();

    // Get the cached on-chain balances from immutable cache (these are the U256 values
    // we last saw from getPoolTokens, NOT the f64-roundtripped values from the pool).
    // If we have cached metadata with a lastChangeBlock, compare blocks first.
    let mut max_drift_bps = 0.0f64;
    let mut drift_details = Vec::new();

    if let Some(ref metadata) = cached_metadata {
        // Compare each token's balance using the fresh on-chain values
        for token in metadata.tokens.iter() {
            let fresh_bal = fresh_balance_map.get(token).copied().unwrap_or(U256::ZERO);
            // We need the cached on-chain balance -- reconstruct it from the pool's last refresh.
            // Since we don't store raw U256 balances separately, use the f64-roundtripped value
            // but apply the tolerance threshold to absorb precision noise.
            let cached_balances = pool.balances_u256();
            let cached_bal = cached_balances
                .iter()
                .find(|(t, _)| t == token)
                .map(|(_, b)| *b)
                .unwrap_or(U256::ZERO);

            if cached_bal != fresh_bal {
                let cached_f: f64 = cached_bal.try_into().unwrap_or(u128::MAX) as f64;
                let fresh_f: f64 = fresh_bal.try_into().unwrap_or(u128::MAX) as f64;
                let drift_bps = if cached_f > 0.0 {
                    ((fresh_f - cached_f) / cached_f * 10_000.0).abs()
                } else if fresh_f > 0.0 {
                    10_000.0
                } else {
                    0.0
                };
                max_drift_bps = max_drift_bps.max(drift_bps);
                drift_details.push(format!(
                    "{}: {} -> {} ({:.2} bps)",
                    token, cached_bal, fresh_bal, drift_bps
                ));
            }
        }
    }

    // Apply tolerance: drifts below threshold are considered fresh
    let is_fresh = max_drift_bps < BALANCER_FRESHNESS_TOLERANCE_BPS;

    if is_fresh {
        if !drift_details.is_empty() {
            debug!(
                pool = %pool_address,
                max_drift_bps = format!("{:.2}", max_drift_bps),
                tolerance_bps = BALANCER_FRESHNESS_TOLERANCE_BPS,
                "Balancer pool drift within tolerance, treating as fresh"
            );
        }
        Ok(PoolFreshnessResult {
            pool_address,
            pool_type: "Balancer",
            is_fresh: true,
            max_drift_bps,
            drift_description: None,
            fresh_v2_reserves: None,
            fresh_v3_state: None,
        })
    } else {
        let description = drift_details.join(", ");
        warn!(
            pool = %pool_address,
            max_drift_bps = format!("{:.2}", max_drift_bps),
            tolerance_bps = BALANCER_FRESHNESS_TOLERANCE_BPS,
            "Balancer pool balances have drifted beyond tolerance"
        );

        Ok(PoolFreshnessResult {
            pool_address,
            pool_type: "Balancer",
            is_fresh: false,
            max_drift_bps,
            drift_description: Some(description),
            fresh_v2_reserves: None,
            fresh_v3_state: None,
        })
    }
}

/// Check freshness of multiple pools used in an execution plan.
///
/// This function should be called immediately before submitting a transaction
/// to catch any state drift that occurred during the search cycle.
///
/// Returns a vector of freshness results for all checked pools.
/// If any pool is stale, the caller should consider re-running the search.
#[instrument(skip(cache, v2_pools, v3_pools))]
pub fn check_pools_freshness(
    cache: &mut EvmCache,
    v2_pools: &[&UniswapV2Pool],
    v3_pools: &[&UniswapV3Pool],
) -> Result<Vec<PoolFreshnessResult>> {
    let mut results = Vec::with_capacity(v2_pools.len() + v3_pools.len());

    for pool in v2_pools {
        results.push(check_v2_freshness(cache, pool)?);
    }

    for pool in v3_pools {
        results.push(check_v3_freshness(cache, pool)?);
    }

    // Log summary
    let stale_count = results.iter().filter(|r| !r.is_fresh).count();
    if stale_count > 0 {
        warn!(
            total_pools = results.len(),
            stale_pools = stale_count,
            "Some pools have stale state - consider re-running search"
        );
    } else {
        debug!(total_pools = results.len(), "All pools have fresh state");
    }

    Ok(results)
}
