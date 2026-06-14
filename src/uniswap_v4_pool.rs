//! Uniswap V4 pool stub.
//!
//! Holds the PoolKey fields (fee, tickSpacing, hooks) that a downstream swap
//! encoder needs to address a Uniswap V4 pool. Full simulation is not yet
//! supported.

use alloy_eips::BlockId;
use alloy_network::Network;
use alloy_primitives::{Address, B256, U256};
use alloy_provider::Provider;
use alloy_rpc_types_eth::Log;
use amms::amms::{amm::AutomatedMarketMaker, balancer::BalancerError, error::AMMError};

#[derive(Debug, Clone)]
pub struct UniswapV4Pool {
    pub address: Address,
    pub currency0: Address,
    pub currency1: Address,
    pub fee: u32,
    pub tick_spacing: i32,
    pub hooks: Address,
    pub tick: i32,
    pub sqrt_price: U256,
    pub liquidity: u128,
}

impl AutomatedMarketMaker for UniswapV4Pool {
    fn address(&self) -> Address {
        self.address
    }

    fn sync_events(&self) -> Vec<B256> {
        vec![]
    }

    fn sync(&mut self, _log: &Log) -> Result<(), AMMError> {
        Ok(())
    }

    fn tokens(&self) -> Vec<Address> {
        vec![self.currency0, self.currency1]
    }

    fn calculate_price(&self, _base: Address, _quote: Address) -> Result<f64, AMMError> {
        Err(AMMError::from(BalancerError::InitializationError))
    }

    fn simulate_swap(&self, _base: Address, _quote: Address, _in: U256) -> Result<U256, AMMError> {
        Err(AMMError::from(BalancerError::InitializationError))
    }

    fn simulate_swap_mut(
        &mut self,
        _base: Address,
        _quote: Address,
        _in: U256,
    ) -> Result<U256, AMMError> {
        Err(AMMError::from(BalancerError::InitializationError))
    }

    async fn init<N, P>(self, _block: BlockId, _provider: P) -> Result<Self, AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        Ok(self)
    }
}
