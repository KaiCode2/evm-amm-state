use super::*;
use crate::balancer_v3_pool::{BalancerV3Pool, BalancerV3PoolType};

// Balancer V3 vault ABI — different from V2 (uses address, not bytes32 poolId).
sol!(
    #[sol(rpc)]
    contract IBalancerV3Vault {
        function getCurrentLiveBalances(address pool) external view returns (uint256[] memory balancesLiveScaled18);
        function getStaticSwapFeePercentage(address pool) external view returns (uint256);
    }
);

sol!(
    #[sol(rpc)]
    contract IBalancerV3Pool {
        function getNormalizedWeights() external view returns (uint256[] memory);
    }
);

sol!(
    #[sol(rpc)]
    contract IBalancerV3StablePool {
        function getAmplificationParameter() external view returns (uint256 value, bool isUpdating, uint256 precision);
    }
);

/// Initialize a Balancer V3 pool from the EVM cache.
///
/// Fetches live balances from the V3 vault, weights and swap fee from the pool
/// contract. Live balances are returned in 18-decimal scaled format and must be
/// downscaled to native token decimals.
///
/// **Important**: The caller should purge the vault's storage once before calling
/// this in a batch — the vault is shared across pools.
/// `pool_type_hint`: optional config-driven hint. When provided, skips the trial
/// `getNormalizedWeights()` call and directly fetches the appropriate parameters.
/// Falls back to auto-detection if `None`.
#[instrument(skip(cache, pool_type_hint), fields(pool = %address, vault = %vault))]
pub async fn init_balancer_v3_from_cache(
    cache: &mut EvmCache,
    address: Address,
    vault: Address,
    token_a: Address,
    token_b: Address,
    pool_type_hint: Option<BalancerV3PoolType>,
) -> Result<BalancerV3Pool> {
    cache.ensure_account(address).await?;
    cache.ensure_account(vault).await?;

    // Fetch live balances (always 18-decimal scaled in V3)
    let live_balances: Vec<U256> = call_view(
        cache,
        vault,
        IBalancerV3Vault::getCurrentLiveBalancesCall { pool: address },
    )?;

    if live_balances.len() < 2 {
        return Err(anyhow!(
            "BalancerV3 pool {} returned {} balances, expected >= 2",
            address,
            live_balances.len()
        ));
    }

    // Fetch swap fee percentage (1e18 fixed point)
    let swap_fee: U256 = call_view(
        cache,
        vault,
        IBalancerV3Vault::getStaticSwapFeePercentageCall { pool: address },
    )?;

    // Detect pool type — use config hint when available, otherwise auto-detect.
    let (pool_type, weights, amplification_factor) = match pool_type_hint {
        Some(BalancerV3PoolType::Stable) => {
            // Config says stable — go straight to fetching A parameter
            fetch_stable_params(cache, address)?
        }
        Some(BalancerV3PoolType::Weighted) => {
            // Config says weighted — go straight to fetching weights
            fetch_weighted_params(cache, address)?
        }
        None => {
            // Auto-detect: try weights first, fall back to stable
            match call_view(cache, address, IBalancerV3Pool::getNormalizedWeightsCall {}) {
                Ok(w) if w.len() >= 2 => (BalancerV3PoolType::Weighted, [w[0], w[1]], None),
                _ => fetch_stable_params(cache, address)?,
            }
        }
    };

    // Get token decimals for downscaling live balances
    let dec_a = cache.erc20_decimals(token_a).unwrap_or(18);
    let dec_b = cache.erc20_decimals(token_b).unwrap_or(18);

    // Downscale from 18 decimals to native token decimals
    let balance_a = downscale_18(live_balances[0], dec_a);
    let balance_b = downscale_18(live_balances[1], dec_b);

    debug!(
        pool = %address,
        balance_a = %balance_a,
        balance_b = %balance_b,
        weight_a = %weights[0],
        weight_b = %weights[1],
        pool_type = ?pool_type,
        swap_fee = %swap_fee,
        dec_a,
        dec_b,
        "BalancerV3 pool initialized"
    );

    Ok(BalancerV3Pool {
        address,
        vault,
        token_a,
        token_b,
        balances: [balance_a, balance_b],
        weights: [weights[0], weights[1]],
        swap_fee,
        decimals: [dec_a, dec_b],
        pool_type,
        amplification_factor,
    })
}

/// Fetch weights for a known-weighted Balancer V3 pool.
fn fetch_weighted_params(
    cache: &mut EvmCache,
    address: Address,
) -> Result<(BalancerV3PoolType, [U256; 2], Option<U256>)> {
    match call_view(cache, address, IBalancerV3Pool::getNormalizedWeightsCall {}) {
        Ok(w) if w.len() >= 2 => Ok((BalancerV3PoolType::Weighted, [w[0], w[1]], None)),
        _ => {
            let half = U256::from(500_000_000_000_000_000u64);
            debug!(pool = %address, "Weighted pool hint but getNormalizedWeights failed, using 50/50");
            Ok((BalancerV3PoolType::Weighted, [half, half], None))
        }
    }
}

/// Fetch amplification parameter for a known-stable (or auto-detected stable) Balancer V3 pool.
fn fetch_stable_params(
    cache: &mut EvmCache,
    address: Address,
) -> Result<(BalancerV3PoolType, [U256; 2], Option<U256>)> {
    let half = U256::from(500_000_000_000_000_000u64); // 0.5e18
    match call_view(
        cache,
        address,
        IBalancerV3StablePool::getAmplificationParameterCall {},
    ) {
        Ok(ret) if !ret.precision.is_zero() => {
            let (value, precision) = (ret.value, ret.precision);
            // Balancer: value = A * precision (precision=1000)
            // Curve expects: _A() = A * 100 (A_PRECISION=100)
            let curve_a = value * U256::from(100) / precision;
            debug!(
                pool = %address,
                balancer_a = %value,
                precision = %precision,
                curve_a = %curve_a,
                "Stable pool: fetched amplification parameter"
            );
            Ok((BalancerV3PoolType::Stable, [half, half], Some(curve_a)))
        }
        _ => {
            // Couldn't fetch A — fall back to weighted with 50/50
            debug!(pool = %address, "No weights or A parameter, falling back to weighted 50/50");
            Ok((BalancerV3PoolType::Weighted, [half, half], None))
        }
    }
}

/// Refresh the balances of a Balancer V3 pool for per-cycle freshness.
///
/// Re-fetches live balances from the vault and downscales to native decimals.
/// Weights and swap fee are immutable for V3 pools.
///
/// **Important**: The caller should purge the vault's storage once before calling
/// this in a batch.
#[instrument(skip(cache), fields(pool = %pool.address))]
pub fn refresh_balancer_v3_pool(cache: &mut EvmCache, pool: &mut BalancerV3Pool) -> Result<()> {
    let live_balances: Vec<U256> = call_view(
        cache,
        pool.vault,
        IBalancerV3Vault::getCurrentLiveBalancesCall { pool: pool.address },
    )?;

    if live_balances.len() < 2 {
        return Err(anyhow!(
            "BalancerV3 pool {} returned {} balances on refresh",
            pool.address,
            live_balances.len()
        ));
    }

    let new_a = downscale_18(live_balances[0], pool.decimals[0]);
    let new_b = downscale_18(live_balances[1], pool.decimals[1]);

    if pool.balances[0] != new_a || pool.balances[1] != new_b {
        debug!(
            pool = %pool.address,
            old_a = %pool.balances[0],
            new_a = %new_a,
            old_b = %pool.balances[1],
            new_b = %new_b,
            "BalancerV3 pool balances changed"
        );
    }

    pool.balances[0] = new_a;
    pool.balances[1] = new_b;

    Ok(())
}

/// Downscale a value from 18 decimals to the given native decimal count.
fn downscale_18(value: U256, decimals: u8) -> U256 {
    if decimals >= 18 {
        value * U256::from(10u64).pow(U256::from(decimals - 18))
    } else {
        value / U256::from(10u64).pow(U256::from(18 - decimals))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downscale_18_to_6_decimals() {
        // 1.0 token in 18 decimals → 1_000_000 in 6 decimals (USDC)
        let value = U256::from(1_000_000_000_000_000_000u128);
        assert_eq!(downscale_18(value, 6), U256::from(1_000_000u64));
    }

    #[test]
    fn downscale_18_to_8_decimals() {
        // 1.0 token in 18 decimals → 100_000_000 in 8 decimals (WBTC)
        let value = U256::from(1_000_000_000_000_000_000u128);
        assert_eq!(downscale_18(value, 8), U256::from(100_000_000u64));
    }

    #[test]
    fn downscale_18_noop_at_18_decimals() {
        let value = U256::from(1_000_000_000_000_000_000u128);
        assert_eq!(downscale_18(value, 18), value);
    }

    #[test]
    fn downscale_18_zero_value() {
        assert_eq!(downscale_18(U256::ZERO, 6), U256::ZERO);
        assert_eq!(downscale_18(U256::ZERO, 18), U256::ZERO);
    }

    #[test]
    fn downscale_18_to_0_decimals() {
        // 1.0 token in 18 decimals → 1 in 0 decimals
        let value = U256::from(1_000_000_000_000_000_000u128);
        assert_eq!(downscale_18(value, 0), U256::from(1u64));
    }

    #[test]
    fn downscale_18_fractional_truncates() {
        // 0.5 USDC worth in 18 decimals → should truncate to 500000
        let value = U256::from(500_000_000_000_000_000u128);
        assert_eq!(downscale_18(value, 6), U256::from(500_000u64));
    }

    #[test]
    fn downscale_18_above_18_decimals_upscales() {
        // Edge case: if a token had 20 decimals, should multiply by 100
        let value = U256::from(1_000_000_000_000_000_000u128);
        assert_eq!(
            downscale_18(value, 20),
            U256::from(100_000_000_000_000_000_000u128)
        );
    }
}
