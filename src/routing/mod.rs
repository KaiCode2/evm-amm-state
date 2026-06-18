//! Offline, parallelizable multi-leg routing and triangular-arbitrage search.
//!
//! Everything here operates on an immutable snapshot of pool states
//! (`HashMap<Address, LocalAMM>`) and uses only [`LocalAMM::simulate_swap`],
//! which is a pure function of each pool's in-memory fields. No RPC, no cache,
//! no async. That makes the search fully deterministic and trivially
//! parallelizable: candidate routes are evaluated concurrently with `rayon`,
//! each borrowing the shared snapshot immutably.
//!
//! The intended pairing is with [`crate::events::EventRouter`]: keep pools
//! current from a log stream, take [`EventRouter::snapshot`] when something
//! changes, then call [`find_triangular_arbitrage`] on the snapshot.
//!
//! [`LocalAMM::simulate_swap`]: crate::amm_wrapper::LocalAMM::simulate_swap
//! [`EventRouter::snapshot`]: crate::events::EventRouter::snapshot

use std::collections::HashMap;

use alloy_primitives::{Address, U256};
use amms::amms::amm::AutomatedMarketMaker;
use rayon::prelude::*;

use crate::amm_wrapper::LocalAMM;

/// One directed hop: swap `token_in` for `token_out` through `pool`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Leg {
    /// Address of the pool used for this hop.
    pub pool: Address,
    /// Token sent into the pool.
    pub token_in: Address,
    /// Token received from the pool.
    pub token_out: Address,
}

/// An ordered sequence of hops. A route is *cyclic* when the first leg's
/// `token_in` equals the last leg's `token_out`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    /// The hops, in execution order.
    pub legs: Vec<Leg>,
}

impl Route {
    /// The token fed into the first hop.
    pub fn start_token(&self) -> Option<Address> {
        self.legs.first().map(|l| l.token_in)
    }

    /// The token produced by the last hop.
    pub fn end_token(&self) -> Option<Address> {
        self.legs.last().map(|l| l.token_out)
    }

    /// Whether the route returns to its starting token.
    pub fn is_cycle(&self) -> bool {
        match (self.start_token(), self.end_token()) {
            (Some(a), Some(b)) => a == b && !self.legs.is_empty(),
            _ => false,
        }
    }

    /// Number of hops.
    pub fn len(&self) -> usize {
        self.legs.len()
    }

    /// Whether the route has no hops.
    pub fn is_empty(&self) -> bool {
        self.legs.is_empty()
    }
}

/// A profitable (or break-even) sizing of a cyclic route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArbResult {
    /// The cyclic route evaluated.
    pub route: Route,
    /// Input amount (in the start token's units) that produced this result.
    pub amount_in: U256,
    /// Output amount after traversing every hop.
    pub amount_out: U256,
    /// `amount_out - amount_in` (same token). Always `> 0` for results returned
    /// by the search functions.
    pub profit: U256,
}

/// Index every token to the addresses of pools that hold it.
pub fn token_pool_index(pools: &HashMap<Address, LocalAMM>) -> HashMap<Address, Vec<Address>> {
    let mut index: HashMap<Address, Vec<Address>> = HashMap::new();
    for (addr, amm) in pools {
        for token in amm.tokens() {
            index.entry(token).or_default().push(*addr);
        }
    }
    index
}

/// Enumerate every simple 3-leg cycle `start → A → B → start` using three
/// distinct pools, where `A` and `B` are distinct from `start` and each other.
///
/// Pools with more than two tokens (Curve, Balancer) contribute every valid
/// directed pair of their tokens, so multi-token pools are fully explored.
///
/// The number of cycles grows quickly with the pool set, so prefer a focused
/// snapshot (e.g. pools touching a few tokens of interest) for large universes.
pub fn triangular_routes(pools: &HashMap<Address, LocalAMM>, start: Address) -> Vec<Route> {
    let index = token_pool_index(pools);
    let mut routes = Vec::new();

    let Some(first_pools) = index.get(&start) else {
        return routes;
    };

    for &p1 in first_pools {
        let Some(amm1) = pools.get(&p1) else { continue };
        for a in amm1.tokens() {
            if a == start {
                continue;
            }
            // leg 2: A -> B through a different pool
            let Some(second_pools) = index.get(&a) else {
                continue;
            };
            for &p2 in second_pools {
                if p2 == p1 {
                    continue;
                }
                let Some(amm2) = pools.get(&p2) else { continue };
                for b in amm2.tokens() {
                    if b == a || b == start {
                        continue;
                    }
                    // leg 3: B -> start through a third pool
                    let Some(third_pools) = index.get(&b) else {
                        continue;
                    };
                    for &p3 in third_pools {
                        if p3 == p1 || p3 == p2 {
                            continue;
                        }
                        let Some(amm3) = pools.get(&p3) else { continue };
                        if !amm3.tokens().contains(&start) {
                            continue;
                        }
                        routes.push(Route {
                            legs: vec![
                                Leg {
                                    pool: p1,
                                    token_in: start,
                                    token_out: a,
                                },
                                Leg {
                                    pool: p2,
                                    token_in: a,
                                    token_out: b,
                                },
                                Leg {
                                    pool: p3,
                                    token_in: b,
                                    token_out: start,
                                },
                            ],
                        });
                    }
                }
            }
        }
    }

    routes
}

/// Simulate a route's output for `amount_in`, fully offline.
///
/// Returns `None` if any hop is missing, errors, or yields zero — i.e. the
/// route is not viable at this size.
pub fn simulate_route(
    pools: &HashMap<Address, LocalAMM>,
    route: &Route,
    amount_in: U256,
) -> Option<U256> {
    let mut amount = amount_in;
    for leg in &route.legs {
        let pool = pools.get(&leg.pool)?;
        amount = pool
            .simulate_swap(leg.token_in, leg.token_out, amount)
            .ok()?;
        if amount.is_zero() {
            return None;
        }
    }
    Some(amount)
}

/// Profit of a cyclic route at `amount_in`, floored at zero.
fn route_profit(pools: &HashMap<Address, LocalAMM>, route: &Route, amount_in: U256) -> U256 {
    match simulate_route(pools, route, amount_in) {
        Some(out) if out > amount_in => out - amount_in,
        _ => U256::ZERO,
    }
}

/// Find the input size in `[min_in, max_in]` maximizing the profit of a cyclic
/// route, via ternary search over the profit curve.
///
/// Returns the best strictly-profitable result, or `None` if the route is never
/// profitable in the range. The route must be a cycle (same start/end token);
/// non-cyclic routes (and an empty range) return `None`.
///
/// Ternary search assumes the profit curve `out(x) - x` is unimodal. This holds
/// exactly for constant-product (Uniswap V2 / Solidly volatile) pools and is a
/// strong approximation for concentrated-liquidity and stable pools, whose
/// piecewise curves can in rare cases hide a better size near a tick/segment
/// boundary. For exhaustive sizing, feed your own candidate set to
/// [`simulate_route`].
pub fn optimize_route(
    pools: &HashMap<Address, LocalAMM>,
    route: &Route,
    min_in: U256,
    max_in: U256,
) -> Option<ArbResult> {
    if !route.is_cycle() || min_in > max_in {
        return None;
    }

    let mut lo = min_in.max(U256::from(1u64));
    let mut hi = max_in;
    // After clamping `lo` up to 1, the range can be empty (e.g. `max_in == 0`).
    if hi < lo {
        return None;
    }

    // Ternary search. Each step shrinks the interval by ~1/3; 256 steps covers
    // the full U256 range with margin. Stop once the interval is small enough
    // that the final endpoint sweep below covers every remaining integer.
    let two = U256::from(2u64);
    for _ in 0..256 {
        if hi <= lo + two {
            break;
        }
        let third = (hi - lo) / U256::from(3u64);
        let m1 = lo + third;
        let m2 = hi - third;
        if route_profit(pools, route, m1) < route_profit(pools, route, m2) {
            lo = m1;
        } else {
            hi = m2;
        }
    }

    // Evaluate the collapsed interval endpoints and midpoint; keep the best.
    let mid = lo + (hi - lo) / two;
    [lo, mid, hi]
        .into_iter()
        .filter_map(|amount_in| {
            let out = simulate_route(pools, route, amount_in)?;
            (out > amount_in).then(|| ArbResult {
                route: route.clone(),
                amount_in,
                amount_out: out,
                profit: out - amount_in,
            })
        })
        .max_by(arb_ordering)
}

/// A stable, deterministic key for a route: its sequence of pool addresses.
fn route_key(route: &Route) -> Vec<Address> {
    route.legs.iter().map(|l| l.pool).collect()
}

/// Total order over arbitrage results: higher profit is "greater"; ties break
/// toward a smaller input, then a lexicographically smaller route. Used so the
/// search returns a deterministic winner regardless of parallel iteration order.
fn arb_ordering(a: &ArbResult, b: &ArbResult) -> std::cmp::Ordering {
    a.profit
        .cmp(&b.profit)
        .then_with(|| b.amount_in.cmp(&a.amount_in))
        .then_with(|| route_key(&b.route).cmp(&route_key(&a.route)))
}

/// Search for the single most profitable triangular arbitrage starting and
/// ending at `start`, evaluating candidate cycles in parallel.
///
/// `min_in`/`max_in` bound the trade size searched per cycle. Returns `None`
/// when no cycle is profitable. The winner is deterministic across runs (ties
/// broken by smaller input, then route order).
pub fn find_triangular_arbitrage(
    pools: &HashMap<Address, LocalAMM>,
    start: Address,
    min_in: U256,
    max_in: U256,
) -> Option<ArbResult> {
    let routes = triangular_routes(pools, start);
    routes
        .par_iter()
        .filter_map(|route| optimize_route(pools, route, min_in, max_in))
        .max_by(arb_ordering)
}

/// Like [`find_triangular_arbitrage`] but returns every profitable cycle,
/// sorted best-first (by profit, then deterministic tie-breaks).
pub fn find_triangular_arbitrages(
    pools: &HashMap<Address, LocalAMM>,
    start: Address,
    min_in: U256,
    max_in: U256,
) -> Vec<ArbResult> {
    let routes = triangular_routes(pools, start);
    let mut results: Vec<ArbResult> = routes
        .par_iter()
        .filter_map(|route| optimize_route(pools, route, min_in, max_in))
        .collect();
    results.sort_by(|a, b| arb_ordering(b, a));
    results
}

#[cfg(test)]
mod tests {
    use super::*;
    use amms::amms::{Token, uniswap_v2::UniswapV2Pool};

    fn token(byte: u8) -> Address {
        Address::with_last_byte(byte)
    }

    /// Build a Uniswap-V2 pool with 18-decimal tokens and a 0.3% fee.
    fn v2(addr: u8, t0: Address, t1: Address, r0: u128, r1: u128) -> (Address, LocalAMM) {
        let address = Address::with_last_byte(addr);
        let pool = UniswapV2Pool {
            address,
            token_a: Token::new_with_decimals(t0, 18),
            token_b: Token::new_with_decimals(t1, 18),
            reserve_0: r0,
            reserve_1: r1,
            fee: 300,
        };
        (address, LocalAMM::UniswapV2(pool))
    }

    const E18: u128 = 1_000_000_000_000_000_000;

    #[test]
    fn enumerates_triangular_cycles() {
        let (a, b, c) = (token(1), token(2), token(3));
        let pools: HashMap<Address, LocalAMM> = [
            v2(10, a, b, 100 * E18, 100 * E18),
            v2(11, b, c, 100 * E18, 100 * E18),
            v2(12, c, a, 100 * E18, 100 * E18),
        ]
        .into_iter()
        .collect();

        let routes = triangular_routes(&pools, a);
        // a->b->c->a and a->c->b->a are both valid 3-pool cycles.
        assert_eq!(routes.len(), 2);
        for route in &routes {
            assert!(route.is_cycle());
            assert_eq!(route.len(), 3);
            assert_eq!(route.start_token(), Some(a));
        }
    }

    #[test]
    fn finds_profitable_triangle() {
        let (a, b, c) = (token(1), token(2), token(3));
        // a->b and b->c are 1:1; c->a pays out 2 a per c. Going a->b->c->a
        // roughly doubles, so an arbitrage exists.
        let pools: HashMap<Address, LocalAMM> = [
            v2(10, a, b, 1000 * E18, 1000 * E18),
            v2(11, b, c, 1000 * E18, 1000 * E18),
            v2(12, c, a, 1000 * E18, 2000 * E18),
        ]
        .into_iter()
        .collect();

        let min_in = U256::from(E18); // 1 token
        let max_in = U256::from(100_000) * U256::from(E18); // generously wide
        let result = find_triangular_arbitrage(&pools, a, min_in, max_in)
            .expect("expected a profitable cycle");

        assert!(result.profit > U256::ZERO);
        assert_eq!(result.route.start_token(), Some(a));
        assert_eq!(result.amount_out, result.amount_in + result.profit);
        // The optimum is interior: strictly above the min bound and strictly
        // below the (deliberately generous) max bound — so the search actually
        // optimized rather than pinning to an edge.
        assert!(result.amount_in > min_in);
        assert!(result.amount_in < max_in);
    }

    #[test]
    fn empty_range_is_handled() {
        let (a, b, c) = (token(1), token(2), token(3));
        let pools: HashMap<Address, LocalAMM> = [
            v2(10, a, b, 1000 * E18, 1000 * E18),
            v2(11, b, c, 1000 * E18, 1000 * E18),
            v2(12, c, a, 1000 * E18, 2000 * E18),
        ]
        .into_iter()
        .collect();

        // max_in == 0 must not underflow or panic — it simply finds nothing.
        assert!(find_triangular_arbitrage(&pools, a, U256::ZERO, U256::ZERO).is_none());
        // min_in > max_in is also a no-op.
        assert!(
            find_triangular_arbitrage(&pools, a, U256::from(10u64), U256::from(5u64)).is_none()
        );
    }

    #[test]
    fn search_is_deterministic() {
        let (a, b, c) = (token(1), token(2), token(3));
        let pools: HashMap<Address, LocalAMM> = [
            v2(10, a, b, 1000 * E18, 1000 * E18),
            v2(11, b, c, 1000 * E18, 1000 * E18),
            v2(12, c, a, 1000 * E18, 2000 * E18),
        ]
        .into_iter()
        .collect();

        let min_in = U256::from(E18);
        let max_in = U256::from(100_000) * U256::from(E18);
        let first = find_triangular_arbitrage(&pools, a, min_in, max_in);
        for _ in 0..8 {
            assert_eq!(find_triangular_arbitrage(&pools, a, min_in, max_in), first);
        }
    }

    #[test]
    fn no_arbitrage_in_balanced_market() {
        let (a, b, c) = (token(1), token(2), token(3));
        let pools: HashMap<Address, LocalAMM> = [
            v2(10, a, b, 1000 * E18, 1000 * E18),
            v2(11, b, c, 1000 * E18, 1000 * E18),
            v2(12, c, a, 1000 * E18, 1000 * E18),
        ]
        .into_iter()
        .collect();

        // A perfectly balanced 1:1:1 triangle only loses money to fees.
        let result = find_triangular_arbitrage(&pools, a, U256::from(E18), U256::from(100 * E18));
        assert!(result.is_none());
    }
}
