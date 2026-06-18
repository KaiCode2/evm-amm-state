//! Offline triangular-arbitrage search driven by pool events.
//!
//! This example needs no RPC node. It builds a small synthetic set of pools,
//! wraps them in an [`EventRouter`], then:
//!
//!   1. searches for a 3-leg arbitrage and finds none (the market is balanced);
//!   2. applies a synthetic swap event that skews one pool's reserves —
//!      exactly what a live log subscription would deliver — so the in-memory
//!      pools immediately reflect the new state;
//!   3. takes an immutable snapshot and searches again, now in parallel and
//!      fully offline, and finds the opportunity the event opened up.
//!
//! Run with:
//!
//! ```bash
//! cargo run --example triangular_arbitrage
//! ```
//!
//! The live counterpart that subscribes to a real node is
//! `examples/event_subscription.rs`.

use std::collections::HashMap;
use std::time::Instant;

use alloy_primitives::{Address, LogData, U256};
use evm_amm_state::amm_wrapper::{LocalAMM, Variant};
use evm_amm_state::events::{EventRouter, event_topics_for};
use evm_amm_state::routing::{Route, find_triangular_arbitrages};

use amms::amms::{Token, uniswap_v2::UniswapV2Pool};

/// 1 token, with 18 decimals.
const ONE: u128 = 1_000_000_000_000_000_000;

fn addr(byte: u8) -> Address {
    Address::with_last_byte(byte)
}

/// Build a Uniswap-V2 pool (0.3% fee, 18-decimal tokens).
fn v2(addr_byte: u8, token0: Address, token1: Address, r0: u128, r1: u128) -> (Address, LocalAMM) {
    let address = Address::with_last_byte(addr_byte);
    let pool = UniswapV2Pool {
        address,
        token_a: Token::new_with_decimals(token0, 18),
        token_b: Token::new_with_decimals(token1, 18),
        reserve_0: r0,
        reserve_1: r1,
        fee: 300,
    };
    (address, LocalAMM::UniswapV2(pool))
}

/// Encode a Uniswap-V2 `Sync(uint112,uint112)` log for `pool`, as a live
/// subscription would deliver it.
fn v2_sync_log(pool: Address, reserve0: u128, reserve1: u128) -> alloy_rpc_types_eth::Log {
    let topic0 = event_topics_for(Variant::UniswapV2)[0];
    let mut data = Vec::with_capacity(64);
    data.extend_from_slice(&U256::from(reserve0).to_be_bytes::<32>());
    data.extend_from_slice(&U256::from(reserve1).to_be_bytes::<32>());
    let inner = alloy_primitives::Log {
        address: pool,
        data: LogData::new_unchecked(vec![topic0], data.into()),
    };
    alloy_rpc_types_eth::Log {
        inner,
        ..Default::default()
    }
}

fn describe_route(route: &Route, names: &HashMap<Address, &str>) -> String {
    let mut s = String::new();
    if let Some(start) = route.start_token() {
        s.push_str(names.get(&start).copied().unwrap_or("?"));
    }
    for leg in &route.legs {
        s.push_str(" -> ");
        s.push_str(names.get(&leg.token_out).copied().unwrap_or("?"));
    }
    s
}

fn main() {
    // Three synthetic tokens forming a triangle.
    let weth = addr(0x01);
    let usdc = addr(0x02);
    let dai = addr(0x03);
    let names: HashMap<Address, &str> = [(weth, "WETH"), (usdc, "USDC"), (dai, "DAI")]
        .into_iter()
        .collect();

    // Three balanced 1:1 pools — no arbitrage to start.
    let pools: HashMap<Address, LocalAMM> = [
        v2(0x10, weth, usdc, 1_000_000 * ONE, 1_000_000 * ONE),
        v2(0x11, usdc, dai, 1_000_000 * ONE, 1_000_000 * ONE),
        v2(0x12, dai, weth, 1_000_000 * ONE, 1_000_000 * ONE),
    ]
    .into_iter()
    .collect();

    let router = EventRouter::from_amms(pools);
    println!("Tracking {} pools.\n", router.len());

    let min_in = U256::from(ONE); // 1 WETH
    let max_in = U256::from(200_000) * U256::from(ONE); // up to 200k WETH

    // 1. Balanced market: no opportunity.
    let before = find_triangular_arbitrages(&router.snapshot(), weth, min_in, max_in);
    println!("Before event: {} profitable cycle(s).", before.len());

    // 2. A large swap hits the DAI/WETH pool, leaving it rich in WETH. A live
    //    bot would receive this as a `Sync` log; here we craft it by hand and
    //    feed it through the same router path.
    let dai_weth_pool = addr(0x12);
    let log = v2_sync_log(dai_weth_pool, 1_000_000 * ONE, 2_000_000 * ONE);
    match router.apply(&log) {
        Ok(Some(update)) => println!(
            "\nApplied event to pool {:.10} ({:?}); pools now reflect new state.",
            update.address, update.kind
        ),
        Ok(None) => println!("\nEvent did not match any tracked pool."),
        Err(e) => println!("\nFailed to apply event: {e}"),
    }

    // 3. Re-search on an immutable snapshot — parallel and fully offline.
    let snapshot = router.snapshot();
    let started = Instant::now();
    let results = find_triangular_arbitrages(&snapshot, weth, min_in, max_in);
    let elapsed = started.elapsed();

    println!(
        "\nAfter event: {} profitable cycle(s) found in {:?} (parallel, offline).",
        results.len(),
        elapsed
    );
    for (i, arb) in results.iter().take(5).enumerate() {
        let amount_in = arb.amount_in / U256::from(ONE);
        let profit = arb.profit / U256::from(ONE);
        println!(
            "  #{}: {}  | in ~{} WETH  -> profit ~{} WETH",
            i + 1,
            describe_route(&arb.route, &names),
            amount_in,
            profit,
        );
    }

    if let Some(best) = results.first() {
        assert!(best.profit > U256::ZERO);
        println!("\nBest cycle profit: {} wei", best.profit);
    }
}
