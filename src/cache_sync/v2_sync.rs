use super::*;

#[instrument(skip(cache), fields(pool = %address))]
pub async fn init_uniswap_v2_from_cache(
    cache: &mut EvmCache,
    address: Address,
    fee: usize,
) -> Result<UniswapV2Pool> {
    let fn_start = Instant::now();

    let t0 = Instant::now();
    cache.ensure_account(address).await?;
    let ensure_ms = t0.elapsed().as_millis();

    // Try to load immutable metadata from cache (token0, token1)
    let t1 = Instant::now();
    let (token0, token1, cached_timestamp) =
        if let Some(metadata) = cache.immutable_cache().get_v2_pool(address) {
            debug!("using cached V2 pool metadata");
            (
                metadata.token0,
                metadata.token1,
                Some(metadata.last_block_timestamp),
            )
        } else {
            // Fetch immutable data from RPC
            let token0 = call_view(cache, address, IUniswapV2Pair::token0Call {})?;
            let token1 = call_view(cache, address, IUniswapV2Pair::token1Call {})?;
            debug!("fetched V2 pool immutable metadata from RPC");
            (token0, token1, None)
        };
    let metadata_ms = t1.elapsed().as_millis();

    // Purge all V2 pool storage to ensure fresh reserve fetch.
    // V2 pools have ~12 storage slots total, so full purge is nearly zero-cost
    // and eliminates any risk of stale non-reserve state affecting EVM execution.
    cache.purge_pool_storage(address);

    // Fetch fresh reserves from RPC (storage was purged above if it existed)
    let t2 = Instant::now();
    let reserves = call_view(cache, address, IUniswapV2Pair::getReservesCall {})?;
    let reserves_ms = t2.elapsed().as_millis();
    let fresh_timestamp = reserves.blockTimestampLast;

    // Log if the timestamp changed (indicates reserves were stale)
    if let Some(cached_ts) = cached_timestamp
        && cached_ts != fresh_timestamp
    {
        debug!(
            pool = %address,
            cached_timestamp = cached_ts,
            fresh_timestamp = fresh_timestamp,
            "V2 pool reserves were stale (timestamp changed)"
        );
    }

    // Update immutable cache with fresh timestamp for future validation
    cache.immutable_cache_mut().set_v2_pool(
        address,
        V2PoolMetadata {
            token0,
            token1,
            last_block_timestamp: fresh_timestamp,
        },
    );

    // Inject immutable metadata (token0, token1) into EVM storage for subsequent calls
    if let Some(metadata) = cache.immutable_cache().get_v2_pool(address).cloned()
        && let Err(e) = cache.inject_v2_pool_metadata(address, &metadata)
    {
        warn!(
            pool = %address,
            error = %e,
            "failed to inject V2 pool metadata into storage cache"
        );
    }

    let dec0 = cache.erc20_decimals(token0).unwrap_or(18);
    let dec1 = cache.erc20_decimals(token1).unwrap_or(18);

    let total_ms = fn_start.elapsed().as_millis();
    debug!(
        pool = %address,
        ensure_ms,
        metadata_ms,
        reserves_ms,
        total_ms,
        "V2 pool init breakdown"
    );

    Ok(UniswapV2Pool {
        address,
        token_a: Token::new_with_decimals(token0, dec0),
        token_b: Token::new_with_decimals(token1, dec1),
        reserve_0: reserves.reserve0.to::<u128>(),
        reserve_1: reserves.reserve1.to::<u128>(),
        fee,
    })
}

/// Refresh the reserves of a UniswapV2 pool for per-cycle freshness.
///
/// This purges the reserves slot and re-fetches fresh reserves from RPC,
/// then updates the pool's reserve_0 and reserve_1 fields.
#[instrument(skip(cache), fields(pool = %pool.address))]
pub fn refresh_uniswap_v2_reserves(cache: &mut EvmCache, pool: &mut UniswapV2Pool) -> Result<()> {
    // Purge all V2 pool storage to force a fresh RPC read.
    // V2 pools have ~12 slots total, so full purge is nearly zero-cost.
    cache.purge_pool_storage(pool.address);

    let reserves = call_view(cache, pool.address, IUniswapV2Pair::getReservesCall {})?;

    let old_r0 = pool.reserve_0;
    let old_r1 = pool.reserve_1;

    pool.reserve_0 = reserves.reserve0.to::<u128>();
    pool.reserve_1 = reserves.reserve1.to::<u128>();

    if old_r0 != pool.reserve_0 || old_r1 != pool.reserve_1 {
        debug!(
            old_r0,
            new_r0 = pool.reserve_0,
            old_r1,
            new_r1 = pool.reserve_1,
            "V2 reserves changed"
        );
    }

    Ok(())
}
