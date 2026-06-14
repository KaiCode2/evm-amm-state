//! Aerodrome/Velodrome CL (Slipstream) pool implementation.
//!
//! Slipstream pools are concentrated liquidity pools with the same math as
//! UniswapV3 but use `tick_spacing` (not `fee`) for pool identification.
//! The fee is dynamic and read from the pool contract.
//!
//! This type wraps a UniswapV3Pool from the `amms` crate for simulation,
//! while maintaining the Slipstream-specific fields (tick spacing, dynamic
//! fee) that a downstream swap encoder and event handling need.

use std::collections::HashMap;

use alloy_eips::BlockId;
use alloy_network::Network;
use alloy_primitives::{Address, B256, U256};
use alloy_provider::Provider;
use alloy_rpc_types_eth::Log;
use amms::amms::{
    Token,
    amm::AutomatedMarketMaker,
    error::AMMError,
    uniswap_v3::{Info, UniswapV3Pool},
};

#[derive(Debug, Clone)]
pub struct SlipstreamPool {
    pub address: Address,
    pub token_a: Address,
    pub token_b: Address,
    /// Tick spacing used for pool identification (NOT fee like UniswapV3).
    pub tick_spacing: i32,
    pub tick: i32,
    pub sqrt_price: U256,
    pub liquidity: u128,
    /// Dynamic fee read from the pool contract (in hundredths of a bip, e.g. 3000 = 0.3%).
    pub fee: u32,
    /// Tick data for simulation (same format as UniswapV3).
    pub ticks: HashMap<i32, Info>,
    /// Tick bitmap for simulation (same format as UniswapV3).
    pub tick_bitmap: HashMap<i16, U256>,
    /// Decimals for token_a.
    pub decimals_a: u8,
    /// Decimals for token_b.
    pub decimals_b: u8,
}

impl SlipstreamPool {
    /// Construct a SlipstreamPool from a V3 pool (used after tick resync).
    pub fn from_v3_pool(v3: UniswapV3Pool, tick_spacing: i32) -> Self {
        let tokens = v3.tokens();
        Self {
            address: v3.address,
            token_a: tokens[0],
            token_b: tokens[1],
            tick_spacing,
            tick: v3.tick,
            sqrt_price: v3.sqrt_price,
            liquidity: v3.liquidity,
            fee: v3.fee,
            ticks: v3.ticks,
            tick_bitmap: v3.tick_bitmap,
            decimals_a: v3.token_a.decimals,
            decimals_b: v3.token_b.decimals,
        }
    }

    /// Convert to a UniswapV3Pool for simulation purposes.
    /// The V3 math is identical — only the fee source differs.
    pub fn as_v3_pool(&self) -> UniswapV3Pool {
        UniswapV3Pool {
            address: self.address,
            token_a: Token::new_with_decimals(self.token_a, self.decimals_a),
            token_b: Token::new_with_decimals(self.token_b, self.decimals_b),
            fee: self.fee,
            tick: self.tick,
            tick_spacing: self.tick_spacing,
            liquidity: self.liquidity,
            sqrt_price: self.sqrt_price,
            ticks: self.ticks.clone(),
            tick_bitmap: self.tick_bitmap.clone(),
        }
    }

    /// Update state from a V3 pool after simulation.
    pub fn apply_v3_state(&mut self, v3: &UniswapV3Pool) {
        self.tick = v3.tick;
        self.sqrt_price = v3.sqrt_price;
        self.liquidity = v3.liquidity;
        self.ticks = v3.ticks.clone();
        self.tick_bitmap = v3.tick_bitmap.clone();
    }
}

impl AutomatedMarketMaker for SlipstreamPool {
    fn address(&self) -> Address {
        self.address
    }

    fn sync_events(&self) -> Vec<B256> {
        // Slipstream uses the same event signatures as UniswapV3
        vec![alloy_primitives::keccak256(
            "Swap(address,address,int256,int256,uint160,uint128,int24)",
        )]
    }

    fn sync(&mut self, _log: &Log) -> Result<(), AMMError> {
        Ok(())
    }

    fn tokens(&self) -> Vec<Address> {
        vec![self.token_a, self.token_b]
    }

    fn calculate_price(&self, base: Address, quote: Address) -> Result<f64, AMMError> {
        self.as_v3_pool().calculate_price(base, quote)
    }

    fn simulate_swap(
        &self,
        base: Address,
        quote: Address,
        amount_in: U256,
    ) -> Result<U256, AMMError> {
        if self.liquidity == 0 || self.sqrt_price.is_zero() || self.ticks.is_empty() {
            return Ok(U256::ZERO);
        }
        self.as_v3_pool().simulate_swap(base, quote, amount_in)
    }

    fn simulate_swap_mut(
        &mut self,
        base: Address,
        quote: Address,
        amount_in: U256,
    ) -> Result<U256, AMMError> {
        if self.liquidity == 0 || self.sqrt_price.is_zero() || self.ticks.is_empty() {
            return Ok(U256::ZERO);
        }
        let mut v3 = self.as_v3_pool();
        let result = v3.simulate_swap_mut(base, quote, amount_in)?;
        self.apply_v3_state(&v3);
        Ok(result)
    }

    async fn init<N, P>(self, _block: BlockId, _provider: P) -> Result<Self, AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        Ok(self)
    }
}
