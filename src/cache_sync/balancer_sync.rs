use super::*;

/// Initialize a Balancer pool from the EVM cache.
///
/// **Important**: The caller must purge the vault's storage once before calling this
/// function in a batch. The vault is a shared contract across all Balancer pools, so
/// purging it per-pool wastes RPC calls. Use `cache.purge_pool_storage(vault)` once
/// before the init loop.
#[instrument(skip(cache), fields(pool_id = %pool_id, vault = %vault))]
pub async fn init_balancer_from_cache(
    cache: &mut EvmCache,
    pool_id: alloy_primitives::B256,
    vault: Address,
) -> Result<BalancerPool> {
    let fn_start = Instant::now();

    let t0 = Instant::now();
    let pool_addr = BalancerPool::address(pool_id);
    cache.ensure_account(pool_addr).await?;
    cache.ensure_account(vault).await?;
    let ensure_ms = t0.elapsed().as_millis();

    // Try to load immutable metadata from cache (tokens, weights, swap_fee)
    let (tokens_vec, weights_vec, swap_fee, cached_last_change_block) =
        if let Some(metadata) = cache.immutable_cache().get_balancer_pool(pool_id).cloned() {
            debug!("using cached Balancer pool metadata");
            (
                metadata.tokens,
                metadata.weights,
                metadata.swap_fee,
                Some(metadata.last_change_block),
            )
        } else {
            // No cached metadata - will fetch fresh below
            (Vec::new(), Vec::new(), U256::ZERO, None)
        };

    let has_cached_metadata = !tokens_vec.is_empty();

    // Only purge the pool contract storage when we don't have cached metadata.
    // When metadata is cached, we don't read from the pool contract at all (weights
    // and swap_fee come from cache), so purging it is unnecessary.
    if !has_cached_metadata && cache.has_pool_storage(pool_addr) {
        let purged = cache.purge_pool_storage(pool_addr);
        debug!(
            pool_id = %pool_id,
            pool_addr = %pool_addr,
            purged_slots = purged,
            "purged Balancer pool storage (no cached metadata)"
        );
    }

    // Fetch fresh pool tokens data from RPC.
    // The caller is responsible for purging the vault's storage once before the
    // init loop so that getPoolTokens reads fresh balances from RPC.
    let tokens_result = call_view(
        cache,
        vault,
        IBalancerVault::getPoolTokensCall { poolId: pool_id },
    )?;
    let fresh_last_change_block = tokens_result.lastChangeBlock;

    // Log if the lastChangeBlock changed (indicates balances were stale)
    if let Some(cached_block) = cached_last_change_block
        && cached_block != fresh_last_change_block
    {
        debug!(
            pool_id = %pool_id,
            cached_block = %cached_block,
            fresh_block = %fresh_last_change_block,
            "Balancer pool balances were stale (lastChangeBlock changed)"
        );
    }

    // Determine tokens, weights, and swap_fee
    let (final_tokens, final_weights, final_swap_fee) = if has_cached_metadata {
        // Use cached immutable data
        (tokens_vec, weights_vec, swap_fee)
    } else {
        // Fetch immutable data from RPC
        let weights = call_view(cache, pool_addr, IBalancerPool::getNormalizedWeightsCall {})?;
        let swap_fee = call_view(cache, pool_addr, IBalancerPool::getSwapFeePercentageCall {})?;
        debug!("fetched Balancer pool immutable metadata from RPC");
        (tokens_result.tokens.clone(), weights.clone(), swap_fee)
    };

    // Update immutable cache with fresh lastChangeBlock for future validation
    cache.immutable_cache_mut().set_balancer_pool(
        pool_id,
        BalancerPoolMetadata {
            tokens: final_tokens.clone(),
            weights: final_weights.clone(),
            swap_fee: final_swap_fee,
            last_change_block: fresh_last_change_block,
        },
    );

    // Use tokens/weights/swap_fee with fresh balances
    let params = PoolParams::new_from_parts(
        final_tokens,
        tokens_result.balances,
        final_weights,
        final_swap_fee,
    );

    let mut decimals = HashMap::with_capacity(params.tokens().len());
    for token in params.tokens() {
        let dec = cache.erc20_decimals(token).unwrap_or(18);
        decimals.insert(token, dec);
    }

    let inner = WeightedPool::from_params(&params, &decimals);

    let total_ms = fn_start.elapsed().as_millis();
    debug!(
        pool_id = %pool_id,
        ensure_ms,
        total_ms,
        "Balancer pool init breakdown"
    );

    Ok(BalancerPool::from_weights(pool_id, vault, inner, decimals))
}

/// Refresh the balances of a Balancer pool for per-cycle freshness.
///
/// Purges vault storage for getPoolTokens, re-fetches fresh balances,
/// and rebuilds the pool's WeightedPool with updated balances while
/// keeping immutable tokens, weights, and swap_fee from cache.
///
/// **Important**: The caller should purge the vault's storage once before calling
/// this in a batch (the vault is shared across Balancer pools).
#[instrument(skip(cache), fields(pool_id = %pool.pool_id))]
pub fn refresh_balancer_pool(cache: &mut EvmCache, pool: &mut BalancerPool) -> Result<()> {
    let fn_start = Instant::now();

    // Load immutable metadata from cache (tokens, weights, swap_fee)
    let metadata = cache
        .immutable_cache()
        .get_balancer_pool(pool.pool_id)
        .cloned()
        .ok_or_else(|| anyhow!("no cached metadata for Balancer pool {}", pool.pool_id))?;

    // Fetch fresh pool tokens data from vault (vault storage should already be purged by caller)
    let tokens_result = call_view(
        cache,
        pool.vault,
        IBalancerVault::getPoolTokensCall {
            poolId: pool.pool_id,
        },
    )?;

    // Build PoolParams with fresh balances + cached immutable data
    let params = PoolParams::new_from_parts(
        metadata.tokens.clone(),
        tokens_result.balances,
        metadata.weights.clone(),
        metadata.swap_fee,
    );

    // Get decimals for each token
    let mut decimals = HashMap::with_capacity(metadata.tokens.len());
    for &token in &metadata.tokens {
        let dec = cache.erc20_decimals(token).unwrap_or(18);
        decimals.insert(token, dec);
    }

    // Log balance changes for diagnostics
    let old_balances = pool.balances_u256();
    pool.refresh_from_params(&params, &decimals);
    let new_balances = pool.balances_u256();

    for ((token, old_bal), (_, new_bal)) in old_balances.iter().zip(new_balances.iter()) {
        if old_bal != new_bal {
            debug!(
                token = %token,
                old_balance = %old_bal,
                new_balance = %new_bal,
                "Balancer pool balance changed"
            );
        }
    }

    // Update immutable cache with fresh lastChangeBlock
    let fresh_last_change_block = tokens_result.lastChangeBlock;
    if metadata.last_change_block != fresh_last_change_block {
        debug!(
            cached_block = %metadata.last_change_block,
            fresh_block = %fresh_last_change_block,
            "Balancer pool lastChangeBlock changed"
        );
    }
    cache.immutable_cache_mut().set_balancer_pool(
        pool.pool_id,
        BalancerPoolMetadata {
            tokens: metadata.tokens,
            weights: metadata.weights,
            swap_fee: metadata.swap_fee,
            last_change_block: fresh_last_change_block,
        },
    );

    let total_ms = fn_start.elapsed().as_millis();
    debug!(total_ms, "Balancer pool refreshed");

    Ok(())
}
