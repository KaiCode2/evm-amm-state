use super::*;
use crate::slipstream_pool::SlipstreamPool;

sol!(
    #[sol(rpc)]
    contract ISlipstreamPool {
        function token0() external view returns (address);
        function token1() external view returns (address);
        function slot0() external view returns (uint160 sqrtPriceX96, int24 tick, uint16 observationIndex, uint16 observationCardinality, uint16 observationCardinalityNext, bool unlocked);
        function liquidity() external view returns (uint128);
        function fee() external view returns (uint24);
        function tickSpacing() external view returns (int24);
    }
);

/// Initialize a Slipstream (Aerodrome/Velodrome CL) pool from the EVM cache.
///
/// Reads slot0, liquidity, fee, and token metadata. Tick data is deferred
/// to the V3 tick resync phase (Slipstream uses the same storage layout).
#[instrument(skip(cache), fields(pool = %address))]
pub async fn init_slipstream_from_cache(
    cache: &mut EvmCache,
    address: Address,
    config_tick_spacing: i32,
) -> Result<SlipstreamPool> {
    cache.ensure_account(address).await?;

    let token0 = call_view(cache, address, ISlipstreamPool::token0Call {})?;
    let token1 = call_view(cache, address, ISlipstreamPool::token1Call {})?;

    // Read fee from contract (Slipstream fee is dynamic, not derived from tick_spacing)
    let fee: u32 = match call_view(cache, address, ISlipstreamPool::feeCall {}) {
        Ok(f) => f.to::<u32>(),
        Err(_) => {
            warn!(pool = %address, "Failed to read Slipstream fee, defaulting to 3000");
            3000
        }
    };

    // Read tick_spacing from contract (overrides config if available)
    let tick_spacing: i32 = match call_view(cache, address, ISlipstreamPool::tickSpacingCall {}) {
        Ok(ts) => ts.unchecked_into(),
        Err(_) => config_tick_spacing,
    };

    // Read slot0 for current tick and sqrt_price
    // Slipstream CL pools have shifted storage: slot0 at slot 6, liquidity at slot 17
    cache.purge_pool_slots(address, &[SLIPSTREAM_SLOT0_SLOT, SLIPSTREAM_LIQUIDITY_SLOT]);
    let slot0 = call_view(cache, address, ISlipstreamPool::slot0Call {})?;
    let liquidity_result = call_view(cache, address, ISlipstreamPool::liquidityCall {})?;

    let sqrt_price = U256::from(slot0.sqrtPriceX96);
    let tick: i32 = slot0.tick.unchecked_into();
    let liquidity: u128 = liquidity_result;

    let dec0 = cache.erc20_decimals(token0).unwrap_or(18);
    let dec1 = cache.erc20_decimals(token1).unwrap_or(18);

    debug!(
        pool = %address,
        token0 = %token0,
        token1 = %token1,
        tick,
        liquidity,
        fee,
        tick_spacing,
        "Slipstream pool initialized (tick data deferred)"
    );

    Ok(SlipstreamPool {
        address,
        token_a: token0,
        token_b: token1,
        tick_spacing,
        tick,
        sqrt_price,
        liquidity,
        fee,
        ticks: HashMap::new(),
        tick_bitmap: HashMap::new(),
        decimals_a: dec0,
        decimals_b: dec1,
    })
}
