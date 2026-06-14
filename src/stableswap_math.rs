//! Shared StableSwap invariant math (Newton's method).
//!
//! Used by both Curve pools and Balancer V3 stable pools. All math operates
//! on 18-decimal scaled values internally; callers handle decimal conversion.

use alloy_primitives::U256;

pub const FEE_DENOMINATOR: u64 = 10_000_000_000; // 1e10

/// Compute the StableSwap invariant D using Newton's method.
///
/// The invariant (for Curve's convention where A is already multiplied by n^(n-1)):
///   Ann * sum(x_i) + D = Ann * D + D^(n+1) / (n^n * prod(x_i))
pub fn get_d(xp: &[U256], a: U256) -> U256 {
    let n = U256::from(xp.len());
    let s: U256 = xp.iter().sum();
    if s.is_zero() {
        return U256::ZERO;
    }

    let ann = a * n; // A * n^n

    let mut d = s;
    for _ in 0..256u32 {
        // D_P = D^(n+1) / (n^n * prod(x_i))
        let mut d_p = d;
        for x in xp {
            // d_p = d_p * d / (x * n)  — add 1 to avoid division by zero
            d_p = d_p * d / (*x * n + U256::from(1));
        }

        let d_prev = d;
        // D = (Ann * S + D_P * n) * D / ((Ann - 1) * D + (n + 1) * D_P)
        let numerator = (ann * s + d_p * n) * d;
        let denominator = (ann - U256::from(1)) * d + (n + U256::from(1)) * d_p;
        if denominator.is_zero() {
            break;
        }
        d = numerator / denominator;

        if d > d_prev {
            if d - d_prev <= U256::from(1) {
                return d;
            }
        } else if d_prev - d <= U256::from(1) {
            return d;
        }
    }
    d
}

/// Compute the new balance of token j after changing token i, given the invariant.
pub fn get_y(xp: &[U256], a: U256, i: usize, j: usize, x_new_i: U256) -> U256 {
    let n = U256::from(xp.len());
    let d = get_d(xp, a);
    let ann = a * n;

    let mut s = U256::ZERO;
    let mut c = d;
    for (k, &xp_k) in xp.iter().enumerate() {
        let x = if k == i { x_new_i } else { xp_k };
        if k != j {
            s += x;
            // No +1 here (unlike get_d) — matches Curve's get_y exactly
            c = c * d / (x * n);
        }
    }
    c = c * d / (ann * n);

    let b = s + d / ann;

    // Newton iteration to solve for y
    let mut y = d;
    for _ in 0..256u32 {
        let y_prev = y;
        // y = (y^2 + c) / (2*y + b - d)
        let denom = U256::from(2) * y + b - d;
        if denom.is_zero() {
            break;
        }
        y = (y * y + c) / denom;
        if y > y_prev {
            if y - y_prev <= U256::from(1) {
                return y;
            }
        } else if y_prev - y <= U256::from(1) {
            return y;
        }
    }
    y
}

/// Simulate a stableswap exchange: returns the output amount after fees.
///
/// - `reserves`: per-coin reserves in raw token units (native decimals)
/// - `precision_multipliers`: per-coin `10^(18 - decimals[i])` for scaling to 18 decimals
/// - `a`: amplification coefficient in Curve's internal format (_A() = A * A_PRECISION)
/// - `fee`: swap fee in parts-per-1e10 (e.g. 4_000_000 = 0.04%)
/// - `i`, `j`: coin indices (input and output)
/// - `dx`: amount in (native decimals)
pub fn stableswap_get_dy(
    reserves: &[U256],
    precision_multipliers: &[U256],
    a: U256,
    fee: U256,
    i: usize,
    j: usize,
    dx: U256,
) -> Option<U256> {
    let n = reserves.len();
    if i >= n || j >= n || i == j {
        return None;
    }

    // Scale reserves to 18 decimals
    let xp: Vec<U256> = reserves
        .iter()
        .zip(precision_multipliers.iter())
        .map(|(r, m)| *r * *m)
        .collect();

    let dx_scaled = dx * precision_multipliers[i];
    let x_new = xp[i] + dx_scaled;

    let y_new = get_y(&xp, a, i, j, x_new);
    let dy = xp[j].saturating_sub(y_new).saturating_sub(U256::from(1));

    // Apply fee: dy_after_fee = dy * (1 - fee/FEE_DENOMINATOR)
    let fee_amount = dy * fee / U256::from(FEE_DENOMINATOR);
    let dy_after_fee = dy - fee_amount;

    // Unscale from 18 decimals
    let pm_j = precision_multipliers[j];
    if pm_j.is_zero() {
        return None;
    }
    Some(dy_after_fee / pm_j)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stableswap_d_symmetric() {
        // 2-pool with equal balances at 1e18 each, _A()=10000 (A=100)
        let xp = vec![
            U256::from(1_000_000_000_000_000_000u128),
            U256::from(1_000_000_000_000_000_000u128),
        ];
        let d = get_d(&xp, U256::from(10_000));
        let d_val: u128 = d.try_into().unwrap();
        assert!(d_val > 1_999_000_000_000_000_000, "D too low: {}", d_val);
        assert!(d_val < 2_001_000_000_000_000_000, "D too high: {}", d_val);
    }

    #[test]
    fn test_stableswap_get_dy_small() {
        // Two stablecoins: USDC (6 dec) and USDT (6 dec), 1M each
        let reserves = vec![
            U256::from(1_000_000_000_000u128), // 1M USDC
            U256::from(1_000_000_000_000u128), // 1M USDT
        ];
        let pm = vec![
            U256::from(1_000_000_000_000u128), // 10^12 (18-6)
            U256::from(1_000_000_000_000u128),
        ];
        let a = U256::from(20_000); // _A() = A(200) * 100
        let fee = U256::from(4_000_000u64); // 0.04%

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
        // Pool with 2M USDC but only 500K USDT
        let reserves = vec![
            U256::from(2_000_000_000_000u128), // 2M USDC
            U256::from(500_000_000_000u128),   // 500K USDT
        ];
        let pm = vec![
            U256::from(1_000_000_000_000u128),
            U256::from(1_000_000_000_000u128),
        ];
        let a = U256::from(20_000);
        let fee = U256::from(4_000_000u64);

        let dx = U256::from(10_000_000_000u128); // 10K USDC
        let dy = stableswap_get_dy(&reserves, &pm, a, fee, 0, 1, dx).unwrap();
        let dy_val: u128 = dy.try_into().unwrap();
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
    fn test_fee_conversion_balancer_to_curve() {
        // Balancer V3: 0.05% = 5e14 (1e18 fixed point)
        // Curve: 0.05% = 500_000 (parts per 1e10)
        let balancer_fee = U256::from(500_000_000_000_000u64); // 5e14
        let curve_fee = balancer_fee / U256::from(100_000_000u64); // / 1e8
        assert_eq!(curve_fee, U256::from(5_000_000u64)); // 0.05% in 1e10
    }

    #[test]
    fn test_18_decimal_pool_no_precision_scaling() {
        // Two 18-decimal tokens (like WETH/rETH), precision_multiplier = 1
        let reserves = vec![
            U256::from(100_000_000_000_000_000_000u128), // 100 WETH
            U256::from(100_000_000_000_000_000_000u128), // 100 rETH
        ];
        let pm = vec![U256::from(1), U256::from(1)]; // 10^(18-18) = 1
        let a = U256::from(200_000); // High A for LST pair
        let fee = U256::from(400_000u64); // 0.004%

        // Swap 1 WETH -> rETH
        let dx = U256::from(1_000_000_000_000_000_000u128); // 1e18
        let dy = stableswap_get_dy(&reserves, &pm, a, fee, 0, 1, dx).unwrap();
        let dy_val: u128 = dy.try_into().unwrap();
        // Should be very close to 1e18 (near parity)
        assert!(
            dy_val > 999_900_000_000_000_000,
            "expected ~1 rETH, got {}",
            dy_val
        );
        assert!(dy_val < 1_000_000_000_000_000_000, "should be < 1 rETH");
    }
}
