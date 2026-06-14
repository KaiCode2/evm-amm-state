use super::*;
use crate::solidly_v2_pool::SolidlyV2Pool;

sol!(
    #[sol(rpc)]
    contract ISolidlyV2Pool {
        function token0() external view returns (address);
        function token1() external view returns (address);
        function getReserves() external view returns (uint256 reserve0, uint256 reserve1, uint256 blockTimestampLast);
        function stable() external view returns (bool);
    }
);

sol!(
    #[sol(rpc)]
    contract IERC20Decimals {
        function decimals() external view returns (uint8);
    }
);

/// Initialize a SolidlyV2 pool by reading on-chain state from the EVM cache.
///
/// Reads token0/token1, reserves, and decimal metadata from the pool contract.
#[instrument(skip(cache), fields(pool = %address))]
pub async fn init_solidly_v2_from_cache(
    cache: &mut EvmCache,
    address: Address,
    stable: bool,
    factory: Address,
    fee: u32,
) -> Result<SolidlyV2Pool> {
    cache.ensure_account(address).await?;

    let token0 = call_view(cache, address, ISolidlyV2Pool::token0Call {})?;
    let token1 = call_view(cache, address, ISolidlyV2Pool::token1Call {})?;

    // Purge and re-fetch reserves
    cache.purge_pool_storage(address);
    let reserves = call_view(cache, address, ISolidlyV2Pool::getReservesCall {})?;

    let dec0 = cache.erc20_decimals(token0).unwrap_or(18);
    let dec1 = cache.erc20_decimals(token1).unwrap_or(18);

    debug!(
        pool = %address,
        token0 = %token0,
        token1 = %token1,
        reserve_0 = ?reserves.reserve0,
        reserve_1 = ?reserves.reserve1,
        stable,
        dec0,
        dec1,
        "SolidlyV2 pool initialized"
    );

    Ok(SolidlyV2Pool {
        address,
        token_a: token0,
        token_b: token1,
        stable,
        factory,
        reserve_0: reserves.reserve0.try_into().unwrap_or(u128::MAX),
        reserve_1: reserves.reserve1.try_into().unwrap_or(u128::MAX),
        fee,
        decimals_0: dec0,
        decimals_1: dec1,
    })
}

/// Refresh the reserves of a SolidlyV2 pool for per-cycle freshness.
#[instrument(skip(cache), fields(pool = %pool.address))]
pub fn refresh_solidly_v2_reserves(cache: &mut EvmCache, pool: &mut SolidlyV2Pool) -> Result<()> {
    cache.purge_pool_storage(pool.address);
    let reserves = call_view(cache, pool.address, ISolidlyV2Pool::getReservesCall {})?;

    pool.reserve_0 = reserves.reserve0.try_into().unwrap_or(u128::MAX);
    pool.reserve_1 = reserves.reserve1.try_into().unwrap_or(u128::MAX);

    Ok(())
}
