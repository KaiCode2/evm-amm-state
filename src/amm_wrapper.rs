//! Unified AMM enum wrapping all supported pool types.

use std::hash::{Hash, Hasher};

use alloy_eips::BlockId;
use alloy_network::Network;
use alloy_primitives::{Address, U256};
use alloy_provider::Provider;
use amms::amms::{
    amm::AutomatedMarketMaker, erc_4626::ERC4626Vault, error::AMMError, uniswap_v2::UniswapV2Pool,
    uniswap_v3::UniswapV3Pool,
};

use super::balancer_pool::BalancerPool;
use super::balancer_v3_pool::BalancerV3Pool;
use super::curve_pool::CurvePool;
use super::slipstream_pool::SlipstreamPool;
use super::solidly_v2_pool::SolidlyV2Pool;
use super::uniswap_v4_pool::UniswapV4Pool;

/// Unified AMM enum replacing `amms::amms::amm::AMM` directly,
/// includes our custom BalancerPool implementation.
#[derive(Debug, Clone)]
pub enum LocalAMM {
    UniswapV2(UniswapV2Pool),
    UniswapV3(UniswapV3Pool),
    PancakeSwapV3(UniswapV3Pool),
    ERC4626(ERC4626Vault),
    Balancer(BalancerPool),
    BalancerV3(BalancerV3Pool),
    Curve(CurvePool),
    SolidlyV2(SolidlyV2Pool),
    Slipstream(SlipstreamPool),
    UniswapV4(UniswapV4Pool),
}

/// Helper enum for type discrimination.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Variant {
    UniswapV2,
    UniswapV3,
    PancakeSwapV3,
    ERC4626,
    Balancer,
    BalancerV3,
    Curve,
    SolidlyV2,
    Slipstream,
    UniswapV4,
}

impl LocalAMM {
    pub fn variant(&self) -> Variant {
        match self {
            LocalAMM::UniswapV2(_) => Variant::UniswapV2,
            LocalAMM::UniswapV3(_) => Variant::UniswapV3,
            LocalAMM::PancakeSwapV3(_) => Variant::PancakeSwapV3,
            LocalAMM::ERC4626(_) => Variant::ERC4626,
            LocalAMM::Balancer(_) => Variant::Balancer,
            LocalAMM::BalancerV3(_) => Variant::BalancerV3,
            LocalAMM::Curve(_) => Variant::Curve,
            LocalAMM::SolidlyV2(_) => Variant::SolidlyV2,
            LocalAMM::Slipstream(_) => Variant::Slipstream,
            LocalAMM::UniswapV4(_) => Variant::UniswapV4,
        }
    }
}

impl AutomatedMarketMaker for LocalAMM {
    fn address(&self) -> Address {
        match self {
            LocalAMM::UniswapV2(pool) => pool.address(),
            LocalAMM::UniswapV3(pool) | LocalAMM::PancakeSwapV3(pool) => pool.address(),
            LocalAMM::ERC4626(pool) => pool.address(),
            LocalAMM::Balancer(pool) => pool.address(),
            LocalAMM::BalancerV3(pool) => pool.address(),
            LocalAMM::Curve(pool) => pool.address(),
            LocalAMM::SolidlyV2(pool) => pool.address(),
            LocalAMM::Slipstream(pool) => pool.address(),
            LocalAMM::UniswapV4(pool) => pool.address(),
        }
    }

    fn sync_events(&self) -> Vec<alloy_primitives::B256> {
        match self {
            LocalAMM::UniswapV2(pool) => pool.sync_events(),
            LocalAMM::UniswapV3(pool) | LocalAMM::PancakeSwapV3(pool) => pool.sync_events(),
            LocalAMM::ERC4626(pool) => pool.sync_events(),
            LocalAMM::Balancer(pool) => pool.sync_events(),
            LocalAMM::BalancerV3(pool) => pool.sync_events(),
            LocalAMM::Curve(pool) => pool.sync_events(),
            LocalAMM::SolidlyV2(pool) => pool.sync_events(),
            LocalAMM::Slipstream(pool) => pool.sync_events(),
            LocalAMM::UniswapV4(pool) => pool.sync_events(),
        }
    }

    fn sync(&mut self, log: &alloy_rpc_types_eth::Log) -> Result<(), AMMError> {
        match self {
            LocalAMM::UniswapV2(pool) => pool.sync(log),
            LocalAMM::UniswapV3(pool) | LocalAMM::PancakeSwapV3(pool) => pool.sync(log),
            LocalAMM::ERC4626(pool) => pool.sync(log),
            LocalAMM::Balancer(pool) => pool.sync(log),
            LocalAMM::BalancerV3(pool) => pool.sync(log),
            LocalAMM::Curve(pool) => pool.sync(log),
            LocalAMM::SolidlyV2(pool) => pool.sync(log),
            LocalAMM::Slipstream(pool) => pool.sync(log),
            LocalAMM::UniswapV4(pool) => pool.sync(log),
        }
    }

    fn tokens(&self) -> Vec<Address> {
        match self {
            LocalAMM::UniswapV2(pool) => pool.tokens(),
            LocalAMM::UniswapV3(pool) | LocalAMM::PancakeSwapV3(pool) => pool.tokens(),
            LocalAMM::ERC4626(pool) => pool.tokens(),
            LocalAMM::Balancer(pool) => pool.tokens(),
            LocalAMM::BalancerV3(pool) => pool.tokens(),
            LocalAMM::Curve(pool) => pool.tokens(),
            LocalAMM::SolidlyV2(pool) => pool.tokens(),
            LocalAMM::Slipstream(pool) => pool.tokens(),
            LocalAMM::UniswapV4(pool) => pool.tokens(),
        }
    }

    fn calculate_price(&self, base_token: Address, quote_token: Address) -> Result<f64, AMMError> {
        match self {
            LocalAMM::UniswapV2(pool) => pool.calculate_price(base_token, quote_token),
            LocalAMM::UniswapV3(pool) | LocalAMM::PancakeSwapV3(pool) => {
                pool.calculate_price(base_token, quote_token)
            }
            LocalAMM::ERC4626(pool) => pool.calculate_price(base_token, quote_token),
            LocalAMM::Balancer(pool) => pool.calculate_price(base_token, quote_token),
            LocalAMM::BalancerV3(pool) => pool.calculate_price(base_token, quote_token),
            LocalAMM::Curve(pool) => pool.calculate_price(base_token, quote_token),
            LocalAMM::SolidlyV2(pool) => pool.calculate_price(base_token, quote_token),
            LocalAMM::Slipstream(pool) => pool.calculate_price(base_token, quote_token),
            LocalAMM::UniswapV4(pool) => pool.calculate_price(base_token, quote_token),
        }
    }

    fn simulate_swap(
        &self,
        base_token: Address,
        quote_token: Address,
        amount_in: U256,
    ) -> Result<U256, AMMError> {
        match self {
            LocalAMM::UniswapV2(pool) => pool.simulate_swap(base_token, quote_token, amount_in),
            LocalAMM::UniswapV3(pool) | LocalAMM::PancakeSwapV3(pool) => {
                pool.simulate_swap(base_token, quote_token, amount_in)
            }
            LocalAMM::ERC4626(pool) => pool.simulate_swap(base_token, quote_token, amount_in),
            LocalAMM::Balancer(pool) => pool.simulate_swap(base_token, quote_token, amount_in),
            LocalAMM::BalancerV3(pool) => pool.simulate_swap(base_token, quote_token, amount_in),
            LocalAMM::Curve(pool) => pool.simulate_swap(base_token, quote_token, amount_in),
            LocalAMM::SolidlyV2(pool) => pool.simulate_swap(base_token, quote_token, amount_in),
            LocalAMM::Slipstream(pool) => pool.simulate_swap(base_token, quote_token, amount_in),
            LocalAMM::UniswapV4(pool) => pool.simulate_swap(base_token, quote_token, amount_in),
        }
    }

    fn simulate_swap_mut(
        &mut self,
        base_token: Address,
        quote_token: Address,
        amount_in: U256,
    ) -> Result<U256, AMMError> {
        match self {
            LocalAMM::UniswapV2(pool) => pool.simulate_swap_mut(base_token, quote_token, amount_in),
            LocalAMM::UniswapV3(pool) | LocalAMM::PancakeSwapV3(pool) => {
                pool.simulate_swap_mut(base_token, quote_token, amount_in)
            }
            LocalAMM::ERC4626(pool) => pool.simulate_swap_mut(base_token, quote_token, amount_in),
            LocalAMM::Balancer(pool) => pool.simulate_swap_mut(base_token, quote_token, amount_in),
            LocalAMM::BalancerV3(pool) => {
                pool.simulate_swap_mut(base_token, quote_token, amount_in)
            }
            LocalAMM::Curve(pool) => pool.simulate_swap_mut(base_token, quote_token, amount_in),
            LocalAMM::SolidlyV2(pool) => pool.simulate_swap_mut(base_token, quote_token, amount_in),
            LocalAMM::Slipstream(pool) => {
                pool.simulate_swap_mut(base_token, quote_token, amount_in)
            }
            LocalAMM::UniswapV4(pool) => pool.simulate_swap_mut(base_token, quote_token, amount_in),
        }
    }

    async fn init<N, P>(self, block_number: BlockId, provider: P) -> Result<Self, AMMError>
    where
        Self: Sized,
        N: Network,
        P: Provider<N> + Clone,
    {
        match self {
            LocalAMM::UniswapV2(pool) => pool
                .init(block_number, provider)
                .await
                .map(LocalAMM::UniswapV2),
            LocalAMM::UniswapV3(pool) => pool
                .init(block_number, provider)
                .await
                .map(LocalAMM::UniswapV3),
            LocalAMM::PancakeSwapV3(pool) => pool
                .init(block_number, provider)
                .await
                .map(LocalAMM::PancakeSwapV3),
            LocalAMM::ERC4626(pool) => pool
                .init(block_number, provider)
                .await
                .map(LocalAMM::ERC4626),
            LocalAMM::Balancer(pool) => pool
                .init(block_number, provider)
                .await
                .map(LocalAMM::Balancer),
            LocalAMM::BalancerV3(pool) => pool
                .init(block_number, provider)
                .await
                .map(LocalAMM::BalancerV3),
            LocalAMM::Curve(pool) => pool.init(block_number, provider).await.map(LocalAMM::Curve),
            LocalAMM::SolidlyV2(pool) => pool
                .init(block_number, provider)
                .await
                .map(LocalAMM::SolidlyV2),
            LocalAMM::Slipstream(pool) => pool
                .init(block_number, provider)
                .await
                .map(LocalAMM::Slipstream),
            LocalAMM::UniswapV4(pool) => pool
                .init(block_number, provider)
                .await
                .map(LocalAMM::UniswapV4),
        }
    }
}

// Hash & Eq by address, like the original AMM did.
impl Hash for LocalAMM {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.address().hash(state);
    }
}

impl PartialEq for LocalAMM {
    fn eq(&self, other: &Self) -> bool {
        self.address() == other.address()
    }
}

impl Eq for LocalAMM {}
