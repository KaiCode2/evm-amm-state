//! Curve CryptoSwap (tricrypto/twocrypto) simulation math.
//!
//! Implements the cryptoswap `get_dy` calculation for 2-coin pools.
//! Uses Newton's method to solve the modified invariant that blends
//! constant-product with constant-sum behavior via the gamma parameter.
//!
//! Reference: Curve's `CurveTwocryptoOptimized` and `CurveCryptoMathOptimized2`.

use alloy_primitives::U256;

const N_COINS_MAX: usize = 3;
const FEE_DENOMINATOR: u64 = 10_000_000_000; // 1e10

fn e18() -> U256 {
    U256::from(10u64).pow(U256::from(18))
}

/// Safe division returning zero on zero denominator.
fn sdiv(a: U256, b: U256) -> U256 {
    if b.is_zero() { U256::ZERO } else { a / b }
}

/// Compute the CryptoSwap invariant D for a 2-coin pool.
///
/// Closely follows Curve's Vyper `newton_D` from `CurveCryptoMathOptimized2`.
///
/// The invariant equation is:
///   K * D * (sum_x) + prod_x = K * D^N + (D/N)^N
/// where K depends on A, gamma, and the balance ratios (K0).
pub fn newton_d(xp: &[U256; 2], a_gamma: [U256; 2]) -> U256 {
    let one = e18();
    let a = a_gamma[0];
    let gamma = a_gamma[1];

    if xp[0].is_zero() || xp[1].is_zero() {
        return U256::ZERO;
    }

    let s = xp[0] + xp[1];
    let mut d = s;

    // Use the mean as a better initial guess
    // D = N * geometric_mean(x_i) is a good starting point
    // For 2 coins: D ≈ 2 * sqrt(x0 * x1) — approximate via Newton
    // Actually just start with S, the Vyper code does the same.

    let n = U256::from(2);
    let a_mul = U256::from(10_000u64); // A_MULTIPLIER

    for _ in 0..256u32 {
        let d_prev = d;
        if d.is_zero() {
            return s;
        }

        // K0 = N^N * prod(x_i) * 1e18 / D^N
        // For N=2: K0 = 4 * x0 * x1 * 1e18 / D^2
        // To avoid overflow, compute step by step:
        // K0 = (4 * x0 * 1e18 / D) * x1 / D
        let k0 = sdiv(U256::from(4) * xp[0] * one, d);
        let k0 = sdiv(k0 * xp[1], d);
        if k0.is_zero() {
            return d;
        }

        // _g1k0 = |gamma + 1e18 - K0| + 1
        let gpo = gamma + one;
        let g1k0 = if gpo > k0 {
            gpo - k0 + U256::from(1)
        } else {
            k0 - gpo + U256::from(1)
        };

        // mul1 = 1e18 * D / gamma * g1k0 / gamma * g1k0 * A_MUL / A
        let mul1 = sdiv(one * d, gamma);
        let mul1 = sdiv(mul1 * g1k0, gamma);
        let mul1 = sdiv(mul1 * g1k0 * a_mul, a);

        // mul2 = (2e18 * N_COINS * K0) / g1k0
        let mul2 = sdiv(U256::from(2) * one * n * k0, g1k0);

        // neg_fprime = S + S*mul2/1e18 + mul1*N/K0 - mul2*D/1e18
        let term_a = s + sdiv(s * mul2, one);
        let term_b = sdiv(mul1 * n, k0);
        let term_c = sdiv(mul2 * d, one);

        if term_a + term_b < term_c + U256::from(1) {
            return d;
        }
        let neg_fprime = term_a + term_b - term_c;
        if neg_fprime.is_zero() {
            return d;
        }

        // D = D * (neg_fprime + S) / neg_fprime - D*D/neg_fprime
        // with adjustment for K0 < 1e18
        let d_plus = sdiv(d * (neg_fprime + s), neg_fprime);
        let mut d_minus = sdiv(d * d, neg_fprime);
        if one > k0 {
            d_minus += sdiv(sdiv(d * mul1, neg_fprime) * (one - k0), k0);
        }

        d = if d_plus > d_minus {
            d_plus - d_minus
        } else {
            (d_prev + U256::from(1)) / U256::from(2)
        };

        // Convergence check
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

/// Compute new balance `y[j]` given `D` and the other balance `xp[1-j]` already updated.
///
/// Closely follows Curve's Vyper `newton_y` from `CurveCryptoSwap2`.
/// Uses a specialized Newton iteration to solve for y directly, avoiding the
/// numerical instability of `newton_d` for imbalanced inputs.
pub fn newton_y(a_gamma: [U256; 2], xp: &[U256; 2], d: U256, j: usize) -> U256 {
    let x_j = xp[1 - j]; // the OTHER coin's balance
    if x_j.is_zero() || d.is_zero() {
        return U256::ZERO;
    }

    let one = e18();
    let a = a_gamma[0];
    let gamma = a_gamma[1];
    let n = U256::from(2u64);
    let n_sq = U256::from(4u64); // N_COINS^2
    let a_mul = U256::from(10_000u64); // A_MULTIPLIER

    // Initial guess: y = D^2 / (x_j * N^2)
    let mut y = sdiv(d * d, x_j * n_sq);
    if y.is_zero() {
        y = d / n;
    }

    // K0_i = 1e18 * N * x_j / D  (partial K0 without y term)
    let k0_i = sdiv(one * n * x_j, d);

    // Convergence limit
    let conv = U256::from(100)
        .max(x_j / U256::from(10u64).pow(U256::from(14)))
        .max(d / U256::from(10u64).pow(U256::from(14)));

    let gpo = gamma + one; // gamma + 1e18

    for _ in 0..255u32 {
        let y_prev = y;

        // K0 = K0_i * y * N / D
        let k0 = sdiv(k0_i * y * n, d);
        if k0.is_zero() {
            return y;
        }

        // S = x_j + y
        let s = x_j + y;

        // g1k0 = |gamma + 1e18 - K0| + 1
        let g1k0 = if gpo > k0 {
            gpo - k0 + U256::from(1)
        } else {
            k0 - gpo + U256::from(1)
        };

        // mul1 = 1e18 * D / gamma * g1k0 / gamma * g1k0 * A_MULTIPLIER / ANN
        let mul1 = sdiv(one * d, gamma);
        let mul1 = sdiv(mul1 * g1k0, gamma);
        let mul1 = sdiv(mul1 * g1k0 * a_mul, a);

        // mul2 = (1e18 + 2e18 * K0) / g1k0
        // Note: in newton_y the ENTIRE expression is divided by g1k0
        let mul2 = sdiv(one + U256::from(2) * one * k0, g1k0);

        // yfprime = 1e18 * y + S * mul2 + mul1
        let yfprime_full = one * y + s * mul2 + mul1;
        // dyfprime = D * mul2
        let dyfprime = d * mul2;

        if yfprime_full < dyfprime {
            y = y_prev / U256::from(2);
            continue;
        }

        // yfprime is modified in-place: yfprime -= dyfprime
        let yfprime = yfprime_full - dyfprime;

        // fprime = yfprime / y  (CRITICAL: divided by y, not just the difference!)
        let fprime = sdiv(yfprime, y);
        if fprime.is_zero() {
            return y;
        }

        // Newton step:
        // y_minus = mul1 / fprime
        // y_plus = (yfprime + 1e18*D) / fprime + y_minus * 1e18 / K0
        // y_minus += 1e18 * S / fprime
        let y_minus_base = sdiv(mul1, fprime);
        let y_plus = sdiv(yfprime + one * d, fprime) + sdiv(y_minus_base * one, k0);
        let y_minus = y_minus_base + sdiv(one * s, fprime);

        if y_plus < y_minus {
            y = y_prev / U256::from(2);
            continue;
        }
        y = y_plus - y_minus;

        // Convergence check
        let diff = if y > y_prev { y - y_prev } else { y_prev - y };
        if diff < conv.max(sdiv(y, U256::from(10u64).pow(U256::from(14)))) {
            return y;
        }
    }
    y
}

/// Simulate a cryptoswap exchange for a 2-coin pool.
///
/// - `reserves`: per-coin in native decimals
/// - `precision_multipliers`: per-coin `10^(18 - decimals[i])`
/// - `a`, `gamma`: pool parameters
/// - `price_scale[0]`: price of coin 1 relative to coin 0 (1e18)
/// - `fee_mid`, `fee_out`: fee range in parts-per-1e10
/// - `i`, `j`: input/output indices
/// - `dx`: input amount in native decimals
#[allow(clippy::too_many_arguments)]
pub fn cryptoswap_get_dy(
    reserves: &[U256],
    precision_multipliers: &[U256],
    a: U256,
    gamma: U256,
    price_scale: &[U256],
    fee_mid: U256,
    fee_out: U256,
    i: usize,
    j: usize,
    dx: U256,
) -> Option<U256> {
    let n = reserves.len();
    if !(2..=N_COINS_MAX).contains(&n) || i >= n || j >= n || i == j {
        return None;
    }
    // This implementation only supports 2-coin cryptoswap pools.
    // Tricrypto (3+ coins) requires a generalized newton_d/newton_y which is not yet implemented.
    if n != 2 {
        return None;
    }
    if dx.is_zero() {
        return Some(U256::ZERO);
    }
    if price_scale.is_empty() {
        return None;
    }

    let one = e18();

    // Convert A from stored convention (_A = A_ext * 100, stableswap A_PRECISION)
    // to CryptoSwap convention (ANN = A_ext * A_MULTIPLIER = A_ext * 10000).
    // Since stored a = A_ext * 100, ANN = a * 100.
    let ann = a * U256::from(100);

    // Scale to internal 1e18 precision, normalized by price_scale
    let scale = |k: usize, bal: U256| -> U256 {
        let s = bal * precision_multipliers[k];
        if k == 0 {
            s
        } else {
            s * price_scale[k - 1] / one
        }
    };

    // Pre-swap scaled balances
    let xp_before = [scale(0, reserves[0]), scale(1, reserves[1])];

    // Compute D from pre-swap state
    let d = newton_d(&xp_before, [ann, gamma]);
    if d.is_zero() {
        return None;
    }

    // Post-swap xp: add dx to coin i
    let mut xp = xp_before;
    xp[i] += scale(i, dx);

    // Solve for new y[j]
    let y_new = newton_y([ann, gamma], &xp, d, j);

    // dy (scaled) = old_y - new_y - 1
    let dy_scaled = xp_before[j].checked_sub(y_new)?;
    let dy_scaled = dy_scaled.saturating_sub(U256::from(1));

    // Dynamic fee
    let fee = compute_dynamic_fee(&xp, fee_mid, fee_out);
    let fee_amount = dy_scaled * fee / U256::from(FEE_DENOMINATOR);
    let dy_after_fee = dy_scaled.saturating_sub(fee_amount);

    // Unscale to native decimals
    let dy_native = if j == 0 {
        if precision_multipliers[j].is_zero() {
            return None;
        }
        dy_after_fee / precision_multipliers[j]
    } else {
        if precision_multipliers[j].is_zero() || price_scale[j - 1].is_zero() {
            return None;
        }
        dy_after_fee * one / price_scale[j - 1] / precision_multipliers[j]
    };

    Some(dy_native)
}

/// Dynamic fee: linear interpolation between fee_mid (balanced) and fee_out (imbalanced).
fn compute_dynamic_fee(xp: &[U256; 2], fee_mid: U256, fee_out: U256) -> U256 {
    let one = e18();
    let s = xp[0] + xp[1];
    if s.is_zero() {
        return fee_mid;
    }

    // K0 = 4 * x0 * x1 * 1e18 / S^2  ∈ [0, 1e18]
    let k0 = sdiv(sdiv(U256::from(4) * xp[0] * one, s) * xp[1], s).min(one);

    if fee_out <= fee_mid {
        return fee_mid;
    }
    fee_mid + (fee_out - fee_mid) * (one - k0) / one
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(val: u128) -> U256 {
        U256::from(val)
    }

    #[test]
    fn test_newton_d_symmetric() {
        let one = u(1_000_000_000_000_000_000);
        let xp = [one, one];
        let d = newton_d(&xp, [u(400_000), u(145_000_000_000_000)]);
        let d_val: u128 = d.try_into().unwrap();
        assert!(d_val > 1_900_000_000_000_000_000, "D too low: {}", d_val);
        assert!(d_val < 2_100_000_000_000_000_000, "D too high: {}", d_val);
    }

    #[test]
    fn test_newton_d_balanced() {
        // newton_d should produce D ≈ 2000e18 for balanced 1000/1000 pool
        let one = u(1_000_000_000_000_000_000);
        let d = newton_d(
            &[one * u(1000), one * u(1000)],
            [u(4_000_000_000), u(145_000_000_000_000)], // ANN=4e9, gamma=1.45e14
        );
        let d_val: u128 = d.try_into().unwrap();
        assert!(
            d_val > 1_999_000_000_000_000_000_000u128,
            "D too low: {}",
            d_val
        );
        assert!(
            d_val < 2_001_000_000_000_000_000_000u128,
            "D too high: {}",
            d_val
        );
    }

    #[test]
    fn test_newton_y_small_swap() {
        // Swap 1 token in a 1000/1000 pool with high A — dy should be ~1 token
        let one = u(1_000_000_000_000_000_000);
        let xp_before = [one * u(1000), one * u(1000)];
        let a_gamma = [u(4_000_000_000), u(145_000_000_000_000)];
        let d = newton_d(&xp_before, a_gamma);

        // Add 1 token to coin 0, solve for new coin 1 balance
        let xp = [xp_before[0] + one, xp_before[1]];
        let y_new = newton_y(a_gamma, &xp, d, 1);
        let dy = xp_before[1].checked_sub(y_new).unwrap_or(U256::ZERO);
        // For a high-A pool, dy ≈ 1e18 (close to dx)
        assert!(dy > u(990_000_000_000_000_000), "dy too low: {}", dy);
        assert!(dy < u(1_010_000_000_000_000_000), "dy too high: {}", dy);
    }

    #[test]
    fn test_cryptoswap_symmetric_pool() {
        // 2-coin pool, both 18 dec, price_scale=1e18 (parity), large reserves
        let reserves = vec![
            u(1_000_000_000_000_000_000_000), // 1000 tokens
            u(1_000_000_000_000_000_000_000),
        ];
        let pm = vec![u(1), u(1)];
        let ps = vec![u(1_000_000_000_000_000_000)]; // 1:1
        let a = u(400_000);
        let gamma = u(145_000_000_000_000);

        let dx = u(1_000_000_000_000_000_000); // 1 token
        let result = cryptoswap_get_dy(
            &reserves,
            &pm,
            a,
            gamma,
            &ps,
            u(3_000_000),
            u(30_000_000),
            0,
            1,
            dx,
        );
        assert!(result.is_some(), "should produce output");
        let dy_val: u128 = result.unwrap().try_into().unwrap();
        // Near 1:1 for balanced pool with small trade
        assert!(dy_val > 900_000_000_000_000_000, "too low: {}", dy_val);
        assert!(
            dy_val < 1_000_000_000_000_000_000,
            "should be < 1 due to fee"
        );
    }

    #[test]
    fn test_cryptoswap_get_dy_weth_usdc() {
        // USDC (6 dec) = coin 0, WETH (18 dec) = coin 1
        // price_scale = price of WETH in USDC = 2000 (in 1e18 fixed point)
        // This makes xp balanced: xp[0]=200000e18, xp[1]=100*2000=200000e18
        let reserves = vec![
            u(200_000_000_000),             // 200K USDC (6 dec)
            u(100_000_000_000_000_000_000), // 100 WETH (18 dec)
        ];
        let pm = vec![u(1_000_000_000_000), u(1)]; // 10^12 for USDC, 1 for WETH
        let ps = vec![u(2_000_000_000_000_000_000_000)]; // 2000e18
        let a = u(400_000); // stored _A = A_ext * 100
        let gamma = u(145_000_000_000_000);

        // Swap 1 WETH (coin 1) → USDC (coin 0)
        let dx = u(1_000_000_000_000_000_000); // 1 WETH
        let result = cryptoswap_get_dy(
            &reserves,
            &pm,
            a,
            gamma,
            &ps,
            u(3_000_000),
            u(30_000_000),
            1,
            0,
            dx,
        );
        assert!(result.is_some(), "should produce output");
        let dy_val: u128 = result.unwrap().try_into().unwrap();
        // ~2000 USDC in 6 dec = ~2_000_000_000
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

    #[test]
    fn test_cryptoswap_zero_input() {
        let reserves = vec![u(1_000_000_000_000_000_000), u(1_000_000_000_000_000_000)];
        let pm = vec![u(1), u(1)];
        let ps = vec![u(1_000_000_000_000_000_000)];
        assert_eq!(
            cryptoswap_get_dy(
                &reserves,
                &pm,
                u(400_000),
                u(145_000_000_000_000),
                &ps,
                u(3_000_000),
                u(30_000_000),
                0,
                1,
                U256::ZERO,
            ),
            Some(U256::ZERO)
        );
    }

    #[test]
    fn test_cryptoswap_invalid_indices() {
        let reserves = vec![u(1_000_000_000_000_000_000), u(1_000_000_000_000_000_000)];
        let pm = vec![u(1), u(1)];
        let ps = vec![u(1_000_000_000_000_000_000)];
        assert!(
            cryptoswap_get_dy(
                &reserves,
                &pm,
                u(400_000),
                u(145_000_000_000_000),
                &ps,
                u(3_000_000),
                u(30_000_000),
                0,
                0,
                u(1_000),
            )
            .is_none()
        );
    }

    #[test]
    fn test_dynamic_fee_balanced() {
        let one = u(1_000_000_000_000_000_000);
        let fee = compute_dynamic_fee(&[one, one], u(3_000_000), u(30_000_000));
        let fee_val: u128 = fee.try_into().unwrap();
        assert!(fee_val <= 5_000_000, "should be near mid_fee: {}", fee_val);
    }

    #[test]
    fn test_dynamic_fee_imbalanced() {
        let one = u(1_000_000_000_000_000_000);
        let fee = compute_dynamic_fee(&[one * u(10), one], u(3_000_000), u(30_000_000));
        let fee_val: u128 = fee.try_into().unwrap();
        assert!(fee_val > 3_000_000, "should be > mid_fee: {}", fee_val);
        assert!(fee_val <= 30_000_000, "should be <= out_fee: {}", fee_val);
    }
}
