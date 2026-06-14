use alloy_primitives::{Address, B256};
use alloy_provider::{MulticallError, Provider};
use alloy_sol_types::sol;

pub use amm_math::data::{PoolParams, PoolTokenParams};

sol! {
    #[sol(rpc)]
    contract IBalancerPool {
        function getNormalizedWeights() external view returns (uint256[] memory);
        function getSwapFeePercentage() external view returns (uint256);
    }
}

sol! {
    #[sol(rpc)]
    contract IBalancerVault {
        function getPoolTokens(bytes32 poolId)
            external
            view
            returns (address[] memory tokens, uint256[] memory balances, uint256 lastChangeBlock);
    }
}

sol! {
    #[sol(rpc)]
    contract IUniswapV3Pool {
        function fee() external view returns (uint24);
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
    }
}

pub async fn get_bal_pool_params<P: Provider + Send + Sync + 'static>(
    provider: &P,
    vault: Address,
    pool_id: B256,
) -> Result<PoolParams, MulticallError> {
    let pool_addr = Address::from_slice(&pool_id.0[0..20]);
    let pool = IBalancerPool::IBalancerPoolInstance::new(pool_addr, provider);
    let vault = IBalancerVault::IBalancerVaultInstance::new(vault, provider);

    let multicall = provider
        .multicall()
        .add(vault.getPoolTokens(pool_id))
        .add(pool.getNormalizedWeights())
        .add(pool.getSwapFeePercentage());

    let (pool_tokens, normalized_weights, swap_fee) = multicall.aggregate().await?;

    Ok(PoolParams::new_from_parts(
        pool_tokens.tokens,
        pool_tokens.balances,
        normalized_weights,
        swap_fee,
    ))
}

pub async fn get_uniswap_v3_slot0<P: Provider + Send + Sync + 'static>(
    provider: &P,
    pool_address: Address,
) -> Result<(IUniswapV3Pool::slot0Return, f64), MulticallError> {
    let pool = IUniswapV3Pool::IUniswapV3PoolInstance::new(pool_address, provider);
    let multicall = provider.multicall().add(pool.fee()).add(pool.slot0());
    let (fee, slot0) = multicall.aggregate().await?;
    let fee_fraction = fee.to::<u64>() as f64 / 1_000_000.0_f64;
    Ok((slot0, fee_fraction))
}
