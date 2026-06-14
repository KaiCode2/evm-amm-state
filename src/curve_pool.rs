//! Curve pool with local StableSwap and CryptoSwap simulation.
//!
//! Supports both stableswap (int128 indices) and cryptoswap (uint256 indices)
//! pool variants. StableSwap uses Newton's method on the constant-sum/product
//! invariant; CryptoSwap uses a gamma-corrected variant for volatile pairs.

use alloy_eips::BlockId;
use alloy_network::Network;
use alloy_primitives::{Address, B256, U256};
use alloy_provider::Provider;
use alloy_rpc_types_eth::Log;
use amms::amms::{amm::AutomatedMarketMaker, balancer::BalancerError, error::AMMError};

use super::{cryptoswap_math, stableswap_math};

/// A Curve pool entry for direct `exchange()` calls.
///
/// Tokens are listed in coin-index order: `tokens[0]` has coin index 0,
/// `tokens[1]` has coin index 1, etc. Swaps between any pair are supported.
#[derive(Debug, Clone)]
pub struct CurvePool {
    pub address: Address,
    /// Ordered list of tokens — position equals Curve coin index.
    pub tokens: Vec<Address>,
    /// `false` = stableswap (int128 indices), `true` = cryptoswap (uint256 indices).
    pub use_uint256: bool,
    /// Per-coin reserves in raw token units.
    pub reserves: Vec<U256>,
    /// Amplification coefficient in Curve's internal format.
    /// StableSwap: `_A() = A() * A_PRECISION` (multiply external A by 100).
    /// CryptoSwap: on-chain A value (includes A_MULTIPLIER = 10000).
    pub a: U256,
    /// Swap fee in parts-per-1e10 (e.g. 4000000 = 0.04%).
    pub fee: U256,
    /// Precision multipliers to normalize each coin to 18 decimals.
    /// `precision_multipliers[i] = 10^(18 - decimals[i])`.
    pub precision_multipliers: Vec<U256>,
    /// CryptoSwap gamma parameter (1e18 fixed point). None for stableswap pools.
    pub gamma: Option<U256>,
    /// CryptoSwap price_scale (1e18 fixed point per additional coin).
    /// `price_scale[k-1]` = price of coin k relative to coin 0.
    /// Empty for stableswap pools.
    pub price_scale: Vec<U256>,
    /// CryptoSwap out_fee (upper bound of dynamic fee range), parts-per-1e10.
    /// For stableswap pools this is None and the flat `fee` is used.
    pub fee_out: Option<U256>,
}

impl CurvePool {
    /// Look up the coin index for a given token address.
    pub fn coin_index(&self, token: Address) -> Option<u32> {
        self.tokens
            .iter()
            .position(|t| *t == token)
            .map(|i| i as u32)
    }

    /// Dispatch to cryptoswap math for uint256-indexed pools.
    fn cryptoswap_get_dy(&self, i: usize, j: usize, dx: U256) -> Option<U256> {
        let gamma = self.gamma?;
        let fee_out = self.fee_out.unwrap_or(self.fee);
        cryptoswap_math::cryptoswap_get_dy(
            &self.reserves,
            &self.precision_multipliers,
            self.a,
            gamma,
            &self.price_scale,
            self.fee, // mid_fee
            fee_out,
            i,
            j,
            dx,
        )
    }

    /// Returns true if the pool has reserves populated.
    pub fn is_initialized(&self) -> bool {
        let base = !self.reserves.is_empty()
            && self.reserves.len() == self.tokens.len()
            && !self.a.is_zero()
            && self.reserves.iter().any(|r| !r.is_zero());

        if self.use_uint256 {
            // CryptoSwap needs gamma + price_scale
            base && self.gamma.is_some() && !self.price_scale.is_empty()
        } else {
            base
        }
    }
}

impl AutomatedMarketMaker for CurvePool {
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
        self.tokens.clone()
    }

    fn calculate_price(&self, base: Address, quote: Address) -> Result<f64, AMMError> {
        if !self.is_initialized() {
            return Err(AMMError::from(BalancerError::InitializationError));
        }
        let i = self
            .coin_index(base)
            .ok_or(AMMError::from(BalancerError::TokenInDoesNotExist))? as usize;
        let j = self
            .coin_index(quote)
            .ok_or(AMMError::from(BalancerError::TokenOutDoesNotExist))? as usize;

        // 1 unit in base token's native decimals
        let pm_i: u128 = self.precision_multipliers[i].try_into().unwrap_or(1);
        let one_unit = U256::from(1_000_000_000_000_000_000u128 / pm_i);

        let out = if self.use_uint256 {
            self.cryptoswap_get_dy(i, j, one_unit).unwrap_or(U256::ZERO)
        } else {
            stableswap_math::stableswap_get_dy(
                &self.reserves,
                &self.precision_multipliers,
                self.a,
                self.fee,
                i,
                j,
                one_unit,
            )
            .unwrap_or(U256::ZERO)
        };

        let pm_j: u128 = self.precision_multipliers[j].try_into().unwrap_or(1);
        let out_18: u128 = (out * U256::from(pm_j)).try_into().unwrap_or(0);
        Ok(out_18 as f64 / 1e18)
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

        let i = self
            .coin_index(base)
            .ok_or(AMMError::from(BalancerError::TokenInDoesNotExist))? as usize;
        let j = self
            .coin_index(quote)
            .ok_or(AMMError::from(BalancerError::TokenOutDoesNotExist))? as usize;

        if self.use_uint256 {
            self.cryptoswap_get_dy(i, j, amount_in)
                .ok_or_else(|| AMMError::from(BalancerError::InitializationError))
        } else {
            stableswap_math::stableswap_get_dy(
                &self.reserves,
                &self.precision_multipliers,
                self.a,
                self.fee,
                i,
                j,
                amount_in,
            )
            .ok_or_else(|| AMMError::from(BalancerError::InitializationError))
        }
    }

    fn simulate_swap_mut(
        &mut self,
        base: Address,
        quote: Address,
        amount_in: U256,
    ) -> Result<U256, AMMError> {
        let result = self.simulate_swap(base, quote, amount_in)?;

        // Update reserves
        let i = self.coin_index(base).unwrap() as usize;
        let j = self.coin_index(quote).unwrap() as usize;
        self.reserves[i] += amount_in;
        self.reserves[j] = self.reserves[j].saturating_sub(result);

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

#[cfg(test)]
mod tests {
    use super::stableswap_math::{get_d, stableswap_get_dy};
    use super::*;

    #[test]
    fn test_stableswap_d_symmetric() {
        // 2-pool with equal balances at 1e18 each, _A()=10000 (A=100)
        let xp = vec![
            U256::from(1_000_000_000_000_000_000u128),
            U256::from(1_000_000_000_000_000_000u128),
        ];
        let d = get_d(&xp, U256::from(10_000)); // A=100 → _A()=10000
        // D should be approximately 2e18
        let d_val: u128 = d.try_into().unwrap();
        assert!(d_val > 1_999_000_000_000_000_000, "D too low: {}", d_val);
        assert!(d_val < 2_001_000_000_000_000_000, "D too high: {}", d_val);
    }

    #[test]
    fn test_stableswap_get_dy_small() {
        // Two stablecoins: USDC (6 dec) and USDT (6 dec), 1M each
        // _A() = A * A_PRECISION = 200 * 100 = 20000
        let reserves = vec![
            U256::from(1_000_000_000_000u128), // 1M USDC
            U256::from(1_000_000_000_000u128), // 1M USDT
        ];
        let pm = vec![
            U256::from(1_000_000_000_000u128), // 10^12 (18-6)
            U256::from(1_000_000_000_000u128),
        ];
        let a = U256::from(20_000); // _A() = A(200) * 100
        let fee = U256::from(4_000_000u64); // 0.04% fee

        // Swap 1000 USDC -> USDT
        let dx = U256::from(1_000_000_000u128); // 1000 USDC
        let dy = stableswap_get_dy(&reserves, &pm, a, fee, 0, 1, dx).unwrap();
        let dy_val: u128 = dy.try_into().unwrap();
        assert!(dy_val > 999_000_000, "expected ~999 USDT, got {}", dy_val);
        assert!(
            dy_val < 1_000_000_000,
            "expected <1000 USDT, got {}",
            dy_val
        );
    }

    #[test]
    fn test_stableswap_imbalanced() {
        // Pool with 2M USDC but only 500K USDT — price should deviate
        let reserves = vec![
            U256::from(2_000_000_000_000u128), // 2M USDC
            U256::from(500_000_000_000u128),   // 500K USDT
        ];
        let pm = vec![
            U256::from(1_000_000_000_000u128),
            U256::from(1_000_000_000_000u128),
        ];
        let a = U256::from(20_000); // _A() = A(200) * 100
        let fee = U256::from(4_000_000u64);

        // Swap 10K USDC -> USDT (selling the heavy coin)
        let dx = U256::from(10_000_000_000u128); // 10K USDC
        let dy = stableswap_get_dy(&reserves, &pm, a, fee, 0, 1, dx).unwrap();
        let dy_val: u128 = dy.try_into().unwrap();
        // Should get less than 10K USDT due to imbalance
        assert!(
            dy_val < 10_000_000_000,
            "should be less than input: {}",
            dy_val
        );
        assert!(
            dy_val > 9_000_000_000,
            "should still be reasonable: {}",
            dy_val
        );
    }

    #[test]
    fn test_cryptoswap_simulate_swap_dispatch_weth_usdc() {
        // Mirror the parameters proven in
        // `cryptoswap_math::tests::test_cryptoswap_get_dy_weth_usdc`, but drive
        // them through `CurvePool::simulate_swap` to confirm a `use_uint256`
        // pool reaches the cryptoswap dispatch (not the stableswap path).
        // USDC (6 dec) = coin 0, WETH (18 dec) = coin 1.
        let usdc = Address::repeat_byte(0x01);
        let weth = Address::repeat_byte(0x02);

        let pool = CurvePool {
            address: Address::repeat_byte(0xCC),
            tokens: vec![usdc, weth],
            use_uint256: true,
            reserves: vec![
                U256::from(200_000_000_000u128),             // 200K USDC (6 dec)
                U256::from(100_000_000_000_000_000_000u128), // 100 WETH (18 dec)
            ],
            a: U256::from(400_000u64),     // stored _A = A_ext * 100
            fee: U256::from(3_000_000u64), // mid_fee
            precision_multipliers: vec![
                U256::from(1_000_000_000_000u128), // 10^12 for USDC
                U256::from(1u64),                  // 1 for WETH
            ],
            gamma: Some(U256::from(145_000_000_000_000u128)),
            price_scale: vec![U256::from(2_000_000_000_000_000_000_000u128)], // 2000e18
            fee_out: Some(U256::from(30_000_000u64)),
        };

        // A fully-specified cryptoswap pool must report itself initialized so
        // simulation does not short-circuit with an error.
        assert!(pool.is_initialized());

        // Swap 1 WETH -> USDC. This must dispatch to the cryptoswap path
        // because `use_uint256` is true.
        let dx = U256::from(1_000_000_000_000_000_000u128); // 1 WETH
        let dy = pool
            .simulate_swap(weth, usdc, dx)
            .expect("cryptoswap simulate_swap should produce output");

        let dy_val: u128 = dy.try_into().unwrap();
        assert!(dy_val > 0, "output should be non-zero");
        // ~2000 USDC in 6 dec = ~2_000_000_000.
        assert!(
            dy_val > 1_500_000_000,
            "too low: {} (expected ~2000 USDC)",
            dy_val
        );
        assert!(
            dy_val < 2_500_000_000,
            "too high: {} (expected ~2000 USDC)",
            dy_val
        );
    }
}
