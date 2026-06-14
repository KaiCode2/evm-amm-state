//! Aerodrome/Velodrome classic (SolidlyV2) pool implementation.
//!
//! Supports both volatile (x*y=k) and stable (x³y+y³x=k) pool invariants.
//! Holds the stable/factory flags that a downstream swap encoder needs to
//! reconstruct the Solidly-style route struct.

use alloy_eips::BlockId;
use alloy_network::Network;
use alloy_primitives::{Address, B256, U256};
use alloy_provider::Provider;
use alloy_rpc_types_eth::Log;
use amms::amms::{amm::AutomatedMarketMaker, error::AMMError};

/// Fee denominator for Solidly V2 pools (fees are in basis points * 100).
/// Aerodrome/Velodrome use fee / 10_000 as the swap fee fraction.
const FEE_DENOMINATOR: u128 = 10_000;

#[derive(Debug, Clone)]
pub struct SolidlyV2Pool {
    pub address: Address,
    pub token_a: Address,
    pub token_b: Address,
    /// Whether this is a stable (true) or volatile (false) pool.
    pub stable: bool,
    /// Factory address used in the Solidly router route struct.
    pub factory: Address,
    pub reserve_0: u128,
    pub reserve_1: u128,
    /// Fee in basis points (e.g. 30 = 0.30%).
    pub fee: u32,
    /// Decimals for token_a (needed for stable swap normalization).
    pub decimals_0: u8,
    /// Decimals for token_b (needed for stable swap normalization).
    pub decimals_1: u8,
}

impl SolidlyV2Pool {
    /// Compute the output amount for a volatile (x*y=k) swap.
    fn get_amount_out_volatile(
        &self,
        amount_in: u128,
        reserve_in: u128,
        reserve_out: u128,
    ) -> u128 {
        let amount_in_after_fee = U256::from(amount_in)
            * U256::from(FEE_DENOMINATOR - self.fee as u128)
            / U256::from(FEE_DENOMINATOR);
        // x * y = k formula (use U256 to avoid u128 overflow on large reserves)
        let numerator = amount_in_after_fee * U256::from(reserve_out);
        let denominator = U256::from(reserve_in) + amount_in_after_fee;
        if denominator.is_zero() {
            return 0;
        }
        (numerator / denominator).to::<u128>()
    }

    /// Compute the output amount for a stable (x³y+y³x=k) swap.
    ///
    /// Uses the Solidly stable swap invariant with decimal normalization.
    /// The invariant is: x³y + y³x = k, where x and y are normalized to 18 decimals.
    fn get_amount_out_stable(
        &self,
        amount_in: u128,
        reserve_in: u128,
        reserve_out: u128,
        decimals_in: u8,
        decimals_out: u8,
    ) -> u128 {
        let amount_in_after_fee_u256 = U256::from(amount_in)
            * U256::from(FEE_DENOMINATOR - self.fee as u128)
            / U256::from(FEE_DENOMINATOR);
        let amount_in_after_fee: u128 = amount_in_after_fee_u256.to::<u128>();

        // Normalize reserves and amount to 18 decimals
        let dec_in = 10u128.pow(decimals_in as u32);
        let dec_out = 10u128.pow(decimals_out as u32);

        // normalized reserves (18 decimal precision)
        let _reserve_in_norm =
            U256::from(reserve_in) * U256::from(10u128.pow(18)) / U256::from(dec_in);
        let _reserve_out_norm =
            U256::from(reserve_out) * U256::from(10u128.pow(18)) / U256::from(dec_out);

        // Use binary search to find output amount (same approach as on-chain _get_y)
        let amount_in_norm =
            U256::from(amount_in_after_fee) * U256::from(10u128.pow(18)) / U256::from(dec_in);
        let new_reserve_in = _reserve_in_norm + amount_in_norm;

        // Compute k = f(reserve_in_norm, reserve_out_norm)
        let k = self.compute_k(_reserve_in_norm, _reserve_out_norm);

        // Binary search for y such that f(new_reserve_in, y) = k
        let y = self.get_y(new_reserve_in, k, _reserve_out_norm);

        // De-normalize the output
        let amount_out_norm = _reserve_out_norm.saturating_sub(y);
        let amount_out = amount_out_norm * U256::from(dec_out) / U256::from(10u128.pow(18));

        // Clamp to u128
        amount_out.try_into().unwrap_or(u128::MAX)
    }

    /// Compute the invariant k = x³y + y³x (all in 18-decimal normalized form).
    fn compute_k(&self, x: U256, y: U256) -> U256 {
        let e18 = U256::from(10u128.pow(18));
        // a = x * y / 1e18
        let a = x * y / e18;
        // b = x² + y² (each divided by 1e18 to keep scale)
        let b = (x * x / e18) + (y * y / e18);
        // k = a * b / 1e18
        a * b / e18
    }

    /// Binary search for y such that compute_k(x, y) >= k.
    fn get_y(&self, x: U256, k: U256, initial_y: U256) -> U256 {
        let mut y = initial_y;

        // Newton's method: y_new = y - (f(x,y) - k) / f'(x,y)_dy
        // f(x,y) = x³y + xy³  (normalized)
        // f'_y = x³ + 3xy²   (normalized)
        for _ in 0..255 {
            let k_current = self.compute_k(x, y);
            if k_current < k {
                // y is too small — increase
                let dy = (k - k_current) * U256::from(10u128.pow(18)) / self.d_y(x, y);
                if dy == U256::ZERO {
                    break;
                }
                y += dy;
            } else {
                // y is too large — decrease
                let dy = (k_current - k) * U256::from(10u128.pow(18)) / self.d_y(x, y);
                if dy == U256::ZERO {
                    break;
                }
                if dy > y {
                    break;
                }
                y -= dy;
            }
        }
        y
    }

    /// Derivative of k with respect to y: dk/dy = x³ + 3xy²  (normalized).
    fn d_y(&self, x: U256, y: U256) -> U256 {
        let e18 = U256::from(10u128.pow(18));
        // x³/1e18² + 3*x*y²/1e18²
        let x3 = x * x / e18 * x / e18;
        let y2 = y * y / e18;
        let three_xy2 = U256::from(3u64) * x * y2 / e18;
        x3 + three_xy2
    }
}

impl AutomatedMarketMaker for SolidlyV2Pool {
    fn address(&self) -> Address {
        self.address
    }

    fn sync_events(&self) -> Vec<B256> {
        // Solidly V2 pools emit Sync events identical to UniswapV2
        vec![alloy_primitives::keccak256("Sync(uint112,uint112)")]
    }

    fn sync(&mut self, _log: &Log) -> Result<(), AMMError> {
        Ok(())
    }

    fn tokens(&self) -> Vec<Address> {
        vec![self.token_a, self.token_b]
    }

    fn calculate_price(&self, base: Address, _quote: Address) -> Result<f64, AMMError> {
        if self.reserve_0 == 0 || self.reserve_1 == 0 {
            return Ok(0.0);
        }
        if base == self.token_a {
            Ok(self.reserve_1 as f64 / self.reserve_0 as f64)
        } else {
            Ok(self.reserve_0 as f64 / self.reserve_1 as f64)
        }
    }

    fn simulate_swap(
        &self,
        base: Address,
        _quote: Address,
        amount_in: U256,
    ) -> Result<U256, AMMError> {
        if self.reserve_0 == 0 || self.reserve_1 == 0 {
            return Ok(U256::ZERO);
        }

        let amount_in_u128: u128 = amount_in.try_into().unwrap_or(u128::MAX);

        let (reserve_in, reserve_out, dec_in, dec_out) = if base == self.token_a {
            (
                self.reserve_0,
                self.reserve_1,
                self.decimals_0,
                self.decimals_1,
            )
        } else {
            (
                self.reserve_1,
                self.reserve_0,
                self.decimals_1,
                self.decimals_0,
            )
        };

        let amount_out = if self.stable {
            self.get_amount_out_stable(amount_in_u128, reserve_in, reserve_out, dec_in, dec_out)
        } else {
            self.get_amount_out_volatile(amount_in_u128, reserve_in, reserve_out)
        };

        Ok(U256::from(amount_out))
    }

    fn simulate_swap_mut(
        &mut self,
        base: Address,
        quote: Address,
        amount_in: U256,
    ) -> Result<U256, AMMError> {
        let amount_out = self.simulate_swap(base, quote, amount_in)?;

        let amount_in_u128: u128 = amount_in.try_into().unwrap_or(u128::MAX);
        let amount_out_u128: u128 = amount_out.try_into().unwrap_or(u128::MAX);

        if base == self.token_a {
            self.reserve_0 = self.reserve_0.saturating_add(amount_in_u128);
            self.reserve_1 = self.reserve_1.saturating_sub(amount_out_u128);
        } else {
            self.reserve_1 = self.reserve_1.saturating_add(amount_in_u128);
            self.reserve_0 = self.reserve_0.saturating_sub(amount_out_u128);
        }

        Ok(amount_out)
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
    use super::*;

    fn make_volatile_pool(r0: u128, r1: u128, fee: u32) -> SolidlyV2Pool {
        SolidlyV2Pool {
            address: Address::ZERO,
            token_a: Address::with_last_byte(1),
            token_b: Address::with_last_byte(2),
            stable: false,
            factory: Address::ZERO,
            reserve_0: r0,
            reserve_1: r1,
            fee,
            decimals_0: 18,
            decimals_1: 18,
        }
    }

    fn make_stable_pool(r0: u128, r1: u128, fee: u32, dec0: u8, dec1: u8) -> SolidlyV2Pool {
        SolidlyV2Pool {
            address: Address::ZERO,
            token_a: Address::with_last_byte(1),
            token_b: Address::with_last_byte(2),
            stable: true,
            factory: Address::ZERO,
            reserve_0: r0,
            reserve_1: r1,
            fee,
            decimals_0: dec0,
            decimals_1: dec1,
        }
    }

    #[test]
    fn test_volatile_swap_basic() {
        let pool = make_volatile_pool(
            1_000_000_000_000_000_000, // 1 ETH
            2_000_000_000,             // 2000 USDC (6 decimals)
            30,                        // 0.3%
        );

        let amount_in = U256::from(100_000_000_000_000_000u128); // 0.1 ETH
        let result = pool
            .simulate_swap(pool.token_a, pool.token_b, amount_in)
            .unwrap();

        // With 0.3% fee and 1:2000 ratio, ~0.1 ETH should give ~181 USDC
        assert!(result > U256::ZERO);
        assert!(result < U256::from(200_000_000u128)); // Less than 200 USDC
    }

    #[test]
    fn test_volatile_swap_matches_uniswap_v2_formula() {
        let pool = make_volatile_pool(1_000_000, 2_000_000, 30);

        let amount_in = U256::from(10_000u128);
        let result = pool
            .simulate_swap(pool.token_a, pool.token_b, amount_in)
            .unwrap();

        // Manual calculation: fee = 10000 * 9970 / 10000 = 9970
        // out = 9970 * 2000000 / (1000000 + 9970) = 19940000000 / 1009970 = 19743
        let expected = U256::from(19_743u128);
        assert_eq!(result, expected);
    }

    #[test]
    fn test_volatile_swap_reverse_direction() {
        let pool = make_volatile_pool(1_000_000, 2_000_000, 30);

        let amount_in = U256::from(10_000u128);
        let result = pool
            .simulate_swap(pool.token_b, pool.token_a, amount_in)
            .unwrap();

        // Swapping token_b for token_a with double reserves
        assert!(result > U256::ZERO);
        assert!(result < amount_in); // Should get less token_a since it's more valuable
    }

    #[test]
    fn test_volatile_swap_mut_updates_reserves() {
        let mut pool = make_volatile_pool(1_000_000, 2_000_000, 30);

        let amount_in = U256::from(10_000u128);
        let result = pool
            .simulate_swap_mut(pool.token_a, pool.token_b, amount_in)
            .unwrap();

        assert_eq!(pool.reserve_0, 1_010_000); // Added 10k
        assert_eq!(pool.reserve_1, 2_000_000 - result.to::<u128>()); // Removed output
    }

    #[test]
    fn test_volatile_zero_reserves_returns_zero() {
        let pool = make_volatile_pool(0, 0, 30);
        let result = pool
            .simulate_swap(pool.token_a, pool.token_b, U256::from(1000u64))
            .unwrap();
        assert_eq!(result, U256::ZERO);
    }

    #[test]
    fn test_stable_swap_same_decimals() {
        // Two stablecoins with same decimals (e.g. USDC/USDT both 6)
        let pool = make_stable_pool(
            1_000_000_000, // 1000 USDC (6 dec)
            1_000_000_000, // 1000 USDT (6 dec)
            5,             // 0.05% fee
            6,
            6,
        );

        let amount_in = U256::from(1_000_000u128); // 1 USDC
        let result = pool
            .simulate_swap(pool.token_a, pool.token_b, amount_in)
            .unwrap();

        // Stable swap near 1:1 should give close to 1 USDT
        let result_u128: u128 = result.try_into().unwrap();
        assert!(
            result_u128 > 990_000,
            "Expected > 0.99 USDT, got {}",
            result_u128
        );
        assert!(
            result_u128 < 1_000_000,
            "Expected < 1 USDT, got {}",
            result_u128
        );
    }

    #[test]
    fn test_stable_swap_different_decimals() {
        // WETH (18 dec) / USDC (6 dec) stable pair — uncommon but tests normalization
        let pool = make_stable_pool(
            1_000_000_000_000_000_000, // 1 WETH (18 dec)
            1_000_000,                 // 1 "unit" (6 dec)
            5,
            18,
            6,
        );

        let amount_in = U256::from(100_000_000_000_000_000u128); // 0.1 WETH
        let result = pool
            .simulate_swap(pool.token_a, pool.token_b, amount_in)
            .unwrap();

        // Should return some amount, not zero
        assert!(result > U256::ZERO);
    }

    #[test]
    fn test_calculate_price() {
        let pool = make_volatile_pool(1_000_000, 2_000_000, 30);

        let price_ab = pool.calculate_price(pool.token_a, pool.token_b).unwrap();
        assert!((price_ab - 2.0).abs() < 0.001);

        let price_ba = pool.calculate_price(pool.token_b, pool.token_a).unwrap();
        assert!((price_ba - 0.5).abs() < 0.001);
    }
}
