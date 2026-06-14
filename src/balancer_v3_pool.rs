//! Balancer V3 weighted pool with local simulation.
//!
//! Reuses the weighted-pool math from `balancer_math.rs`. Pools must be
//! populated with balances, weights, and swap fee before `simulate_swap`
//! returns meaningful results.

use alloy_eips::BlockId;
use alloy_network::Network;
use alloy_primitives::{Address, B256, U256};
use alloy_provider::Provider;
use alloy_rpc_types_eth::Log;
use amms::amms::{amm::AutomatedMarketMaker, balancer::BalancerError, error::AMMError};

use super::balancer_math::{WeightedPool, WeightedPoolError, u256_to_f64_lossy};
use super::stableswap_math;

/// Discriminates between Balancer V3 weighted and stable pool types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BalancerV3PoolType {
    Weighted,
    Stable,
}

#[derive(Debug, Clone)]
pub struct BalancerV3Pool {
    pub address: Address,
    pub vault: Address,
    pub token_a: Address,
    pub token_b: Address,
    /// Token balances in raw units (token_a at index 0, token_b at index 1).
    pub balances: [U256; 2],
    /// Normalized weights (1e18 fixed point). Must sum to ~1e18.
    /// For stable pools these are set to 50/50 but unused by simulation.
    pub weights: [U256; 2],
    /// Swap fee in 1e18 fixed point (e.g. 5e14 = 0.05% = 5 bps).
    pub swap_fee: U256,
    /// Decimals for each token.
    pub decimals: [u8; 2],
    /// Pool type: Weighted uses power-law math, Stable uses StableSwap invariant.
    pub pool_type: BalancerV3PoolType,
    /// Amplification factor in Curve convention: `_A() = A * A_PRECISION`.
    /// Only set for Stable pools. Balancer returns `(value, isUpdating, precision)`
    /// where `value = A * precision` (precision=1000); we convert to Curve's
    /// `_A() = A * 100` via `value * 100 / precision`.
    pub amplification_factor: Option<U256>,
}

impl BalancerV3Pool {
    /// Returns true if the pool has been populated with state for simulation.
    pub fn is_initialized(&self) -> bool {
        let balances_ok = !self.balances[0].is_zero() && !self.balances[1].is_zero();
        match self.pool_type {
            BalancerV3PoolType::Weighted => {
                balances_ok && !self.weights[0].is_zero() && !self.weights[1].is_zero()
            }
            BalancerV3PoolType::Stable => balances_ok && self.amplification_factor.is_some(),
        }
    }

    /// Build a `WeightedPool` from the current state for simulation (weighted pools only).
    fn as_weighted_pool(&self) -> WeightedPool {
        let bal_a = u256_to_f64_lossy(self.balances[0], self.decimals[0]);
        let bal_b = u256_to_f64_lossy(self.balances[1], self.decimals[1]);
        let w_a = u256_to_f64_lossy(self.weights[0], 18);
        let w_b = u256_to_f64_lossy(self.weights[1], 18);
        let sum_w = w_a + w_b;
        WeightedPool {
            tokens: vec![self.token_a, self.token_b],
            balances: vec![bal_a, bal_b],
            weights: vec![w_a / sum_w, w_b / sum_w],
            swap_fee: u256_to_f64_lossy(self.swap_fee, 18),
        }
    }

    /// Compute precision multipliers for StableSwap math: `10^(18 - decimals[i])`.
    ///
    /// Decimals originate from an on-chain `decimals()` read (defaulting to 18),
    /// so a misbehaving or non-standard token can report a value above 18. In
    /// that case `18 - decimals[i]` would underflow (panicking in debug, wrapping
    /// in release), so we saturate the multiplier to 1 (i.e. skip upscaling)
    /// rather than panicking. Mirrors the guard in `cache_sync::curve_sync`.
    fn precision_multipliers(&self) -> [U256; 2] {
        let multiplier = |decimals: u8| -> U256 {
            if decimals <= 18 {
                U256::from(10u64).pow(U256::from(18 - decimals))
            } else {
                U256::from(1)
            }
        };
        [multiplier(self.decimals[0]), multiplier(self.decimals[1])]
    }

    /// Convert Balancer V3's 1e18 fixed-point fee to Curve's parts-per-1e10 format.
    /// E.g. 5e14 (0.05% in 1e18) → 5_000_000 (0.05% in 1e10).
    fn fee_as_curve_parts(&self) -> U256 {
        self.swap_fee / U256::from(100_000_000u64) // / 1e8
    }

    /// Resolve token addresses to (i, j) indices.
    fn token_indices(&self, base: Address, quote: Address) -> Result<(usize, usize), AMMError> {
        let i = if base == self.token_a {
            0
        } else if base == self.token_b {
            1
        } else {
            return Err(AMMError::from(BalancerError::TokenInDoesNotExist));
        };
        let j = if quote == self.token_a {
            0
        } else if quote == self.token_b {
            1
        } else {
            return Err(AMMError::from(BalancerError::TokenOutDoesNotExist));
        };
        Ok((i, j))
    }
}

impl AutomatedMarketMaker for BalancerV3Pool {
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
        vec![self.token_a, self.token_b]
    }

    fn calculate_price(&self, base: Address, quote: Address) -> Result<f64, AMMError> {
        if !self.is_initialized() {
            return Err(AMMError::from(BalancerError::InitializationError));
        }
        match self.pool_type {
            BalancerV3PoolType::Weighted => self
                .as_weighted_pool()
                .spot_price(base, quote)
                .map_err(map_weighted_pool_error),
            BalancerV3PoolType::Stable => {
                let (i, j) = self.token_indices(base, quote)?;
                let pm = self.precision_multipliers();
                let a = self.amplification_factor.unwrap(); // checked by is_initialized
                let fee = self.fee_as_curve_parts();
                // 1 unit in base token's native decimals
                let pm_i: u128 = pm[i].try_into().unwrap_or(1);
                let one_unit = U256::from(1_000_000_000_000_000_000u128 / pm_i);
                let out = stableswap_math::stableswap_get_dy(
                    &self.balances,
                    pm.as_ref(),
                    a,
                    fee,
                    i,
                    j,
                    one_unit,
                )
                .unwrap_or(U256::ZERO);
                let pm_j: u128 = pm[j].try_into().unwrap_or(1);
                let out_18: u128 = (out * U256::from(pm_j)).try_into().unwrap_or(0);
                Ok(out_18 as f64 / 1e18)
            }
        }
    }

    fn simulate_swap(
        &self,
        base: Address,
        quote: Address,
        amount_in: U256,
    ) -> Result<U256, AMMError> {
        if !self.is_initialized() {
            return Err(AMMError::from(BalancerError::InitializationError));
        }
        match self.pool_type {
            BalancerV3PoolType::Weighted => {
                let mut wp = self.as_weighted_pool();
                let in_decimals = if base == self.token_a {
                    self.decimals[0]
                } else {
                    self.decimals[1]
                };
                let amount_in_f64 = u256_to_f64_lossy(amount_in, in_decimals);
                let out_f64 = wp
                    .swap_out_given_in(base, quote, amount_in_f64)
                    .map_err(map_weighted_pool_error)?;
                let out_decimals = if quote == self.token_a {
                    self.decimals[0]
                } else {
                    self.decimals[1]
                };
                let scale = 10f64.powi(out_decimals as i32);
                Ok(U256::from((out_f64 * scale) as u128))
            }
            BalancerV3PoolType::Stable => {
                let (i, j) = self.token_indices(base, quote)?;
                let a = self.amplification_factor.unwrap();
                let pm = self.precision_multipliers();
                let fee = self.fee_as_curve_parts();
                stableswap_math::stableswap_get_dy(
                    &self.balances,
                    pm.as_ref(),
                    a,
                    fee,
                    i,
                    j,
                    amount_in,
                )
                .ok_or_else(|| AMMError::from(BalancerError::InitializationError))
            }
        }
    }

    fn simulate_swap_mut(
        &mut self,
        base: Address,
        quote: Address,
        amount_in: U256,
    ) -> Result<U256, AMMError> {
        if !self.is_initialized() {
            return Err(AMMError::from(BalancerError::InitializationError));
        }
        match self.pool_type {
            BalancerV3PoolType::Weighted => {
                let mut wp = self.as_weighted_pool();
                let in_decimals = if base == self.token_a {
                    self.decimals[0]
                } else {
                    self.decimals[1]
                };
                let out_decimals = if quote == self.token_a {
                    self.decimals[0]
                } else {
                    self.decimals[1]
                };
                let amount_in_f64 = u256_to_f64_lossy(amount_in, in_decimals);
                let out_f64 = wp
                    .swap_out_given_in(base, quote, amount_in_f64)
                    .map_err(map_weighted_pool_error)?;

                // Write back updated balances
                let idx_a = wp.index_of(self.token_a).unwrap_or(0);
                let idx_b = wp.index_of(self.token_b).unwrap_or(1);
                let scale_a = 10f64.powi(self.decimals[0] as i32);
                let scale_b = 10f64.powi(self.decimals[1] as i32);
                self.balances[0] = U256::from((wp.balances[idx_a] * scale_a) as u128);
                self.balances[1] = U256::from((wp.balances[idx_b] * scale_b) as u128);

                let scale = 10f64.powi(out_decimals as i32);
                Ok(U256::from((out_f64 * scale) as u128))
            }
            BalancerV3PoolType::Stable => {
                let (i, j) = self.token_indices(base, quote)?;
                let result = self.simulate_swap(base, quote, amount_in)?;
                // Update reserves
                self.balances[i] += amount_in;
                self.balances[j] = self.balances[j].saturating_sub(result);
                Ok(result)
            }
        }
    }

    async fn init<N, P>(self, _block: BlockId, _provider: P) -> Result<Self, AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        Ok(self)
    }
}

fn map_weighted_pool_error(e: WeightedPoolError) -> AMMError {
    match e {
        WeightedPoolError::TokenInDoesNotExist => {
            AMMError::from(BalancerError::TokenInDoesNotExist)
        }
        WeightedPoolError::TokenOutDoesNotExist => {
            AMMError::from(BalancerError::TokenOutDoesNotExist)
        }
    }
}
