use super::*;
use crate::curve_pool::CurvePool;

sol!(
    #[sol(rpc)]
    contract ICurvePool {
        function balances(uint256 index) external view returns (uint256);
        function A() external view returns (uint256);
        function fee() external view returns (uint256);
    }
);

sol!(
    #[sol(rpc)]
    contract ICurveCryptoPool {
        function gamma() external view returns (uint256);
        function price_scale() external view returns (uint256);
        function mid_fee() external view returns (uint256);
        function out_fee() external view returns (uint256);
    }
);

sol!(
    #[sol(rpc)]
    contract ICurveCryptoPoolIndexed {
        function price_scale(uint256 k) external view returns (uint256);
    }
);

/// Initialize a Curve pool from the EVM cache.
///
/// Fetches per-coin reserves via `balances(i)`, amplification parameter `A()`,
/// and `fee()`. Computes precision multipliers from token decimals.
#[instrument(skip(cache), fields(pool = %address, n_coins = tokens.len()))]
pub async fn init_curve_from_cache(
    cache: &mut EvmCache,
    address: Address,
    tokens: &[Address],
    use_uint256: bool,
) -> Result<CurvePool> {
    cache.ensure_account(address).await?;

    let n = tokens.len();

    // Fetch per-coin reserves
    let mut reserves = Vec::with_capacity(n);
    for i in 0..n {
        let bal: U256 = call_view(
            cache,
            address,
            ICurvePool::balancesCall {
                index: U256::from(i),
            },
        )?;
        reserves.push(bal);
    }

    // Fetch amplification parameter — Curve's external A() returns the base value;
    // internally _A() = A() * A_PRECISION (100). Our simulation expects the internal form.
    let a_external: U256 = call_view(cache, address, ICurvePool::ACall {})?;
    let a = a_external * U256::from(100);

    // Fetch fee (parts-per-1e10)
    let fee: U256 = call_view(cache, address, ICurvePool::feeCall {})?;

    // Compute precision multipliers: 10^(18 - decimals[i])
    let mut precision_multipliers = Vec::with_capacity(n);
    for &token in tokens {
        let dec = cache.erc20_decimals(token).unwrap_or(18);
        let pm = if dec <= 18 {
            U256::from(10u64).pow(U256::from(18 - dec))
        } else {
            U256::from(1) // Should not happen, but protect against > 18 decimals
        };
        precision_multipliers.push(pm);
    }

    // For cryptoswap pools, fetch gamma, price_scale, and fee range
    let (gamma, price_scale, fee_out, effective_fee) = if use_uint256 {
        let gamma: U256 = call_view(cache, address, ICurveCryptoPool::gammaCall {})?;

        // Fetch price_scale: for 2-coin pools use no-arg version, for 3+ use indexed version
        let price_scale = if n > 2 {
            let mut scales = Vec::with_capacity(n - 1);
            for k in 0..(n - 1) {
                let ps: U256 = call_view(
                    cache,
                    address,
                    ICurveCryptoPoolIndexed::price_scaleCall { k: U256::from(k) },
                )?;
                scales.push(ps);
            }
            scales
        } else {
            let ps: U256 = call_view(cache, address, ICurveCryptoPool::price_scaleCall {})?;
            vec![ps]
        };

        // Try to read mid_fee/out_fee; fall back to single fee() if unavailable
        let mid_fee: U256 =
            call_view(cache, address, ICurveCryptoPool::mid_feeCall {}).unwrap_or(fee);
        let out_fee: U256 =
            call_view(cache, address, ICurveCryptoPool::out_feeCall {}).unwrap_or(fee);

        (Some(gamma), price_scale, Some(out_fee), mid_fee)
    } else {
        (None, vec![], None, fee)
    };

    debug!(
        pool = %address,
        n_coins = n,
        use_uint256,
        a = %a,
        fee = %effective_fee,
        ?gamma,
        reserves = ?reserves.iter().map(|r| format!("{}", r)).collect::<Vec<_>>(),
        "Curve pool initialized"
    );

    Ok(CurvePool {
        address,
        tokens: tokens.to_vec(),
        use_uint256,
        reserves,
        a,
        fee: effective_fee,
        precision_multipliers,
        gamma,
        price_scale,
        fee_out,
    })
}

/// Refresh the reserves of a Curve pool for per-cycle freshness.
///
/// Re-fetches `balances(i)` for each coin. A and fee are considered stable
/// enough to not refresh every cycle (they change very rarely via governance).
#[instrument(skip(cache), fields(pool = %pool.address))]
pub fn refresh_curve_reserves(cache: &mut EvmCache, pool: &mut CurvePool) -> Result<()> {
    cache.purge_pool_storage(pool.address);

    for i in 0..pool.tokens.len() {
        let bal: U256 = call_view(
            cache,
            pool.address,
            ICurvePool::balancesCall {
                index: U256::from(i),
            },
        )?;
        pool.reserves[i] = bal;
    }

    Ok(())
}
