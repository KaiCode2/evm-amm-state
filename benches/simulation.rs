//! Offline benchmarks for the simulation, event-apply, and routing hot paths.
//!
//! None of these touch RPC or the cache — they exercise the in-memory pool
//! models and the parallel routing search, which is exactly what runs on the
//! latency-sensitive path of a live bot.
//!
//! Run with: `cargo bench`

use std::collections::HashMap;

use alloy_primitives::{Address, LogData, U256};
use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use std::hint::black_box;

use amms::amms::{Token, amm::AutomatedMarketMaker, uniswap_v2::UniswapV2Pool};
use evm_amm_state::amm_wrapper::{LocalAMM, Variant};
use evm_amm_state::curve_pool::CurvePool;
use evm_amm_state::events::{apply_log, event_topics_for};
use evm_amm_state::routing::{find_triangular_arbitrages, simulate_route, triangular_routes};
use evm_amm_state::solidly_v2_pool::SolidlyV2Pool;

const ONE: u128 = 1_000_000_000_000_000_000;

fn token(byte: u8) -> Address {
    Address::with_last_byte(byte)
}

fn v2_pool(addr_byte: u8, t0: Address, t1: Address, r0: u128, r1: u128) -> LocalAMM {
    LocalAMM::UniswapV2(UniswapV2Pool {
        address: Address::with_last_byte(addr_byte),
        token_a: Token::new_with_decimals(t0, 18),
        token_b: Token::new_with_decimals(t1, 18),
        reserve_0: r0,
        reserve_1: r1,
        fee: 300,
    })
}

fn solidly_pool(t0: Address, t1: Address) -> LocalAMM {
    LocalAMM::SolidlyV2(SolidlyV2Pool {
        address: Address::with_last_byte(0xA0),
        token_a: t0,
        token_b: t1,
        stable: false,
        factory: Address::ZERO,
        reserve_0: 1_000_000 * ONE,
        reserve_1: 2_000_000 * ONE,
        fee: 30,
        decimals_0: 18,
        decimals_1: 18,
    })
}

fn curve_stable_pool(t0: Address, t1: Address) -> LocalAMM {
    // 2-coin USDC/USDT-style stableswap (6 decimals), 1M each.
    LocalAMM::Curve(CurvePool {
        address: Address::with_last_byte(0xC0),
        tokens: vec![t0, t1],
        use_uint256: false,
        reserves: vec![
            U256::from(1_000_000_000_000u128),
            U256::from(1_000_000_000_000u128),
        ],
        a: U256::from(20_000u64),
        fee: U256::from(4_000_000u64),
        precision_multipliers: vec![
            U256::from(1_000_000_000_000u128),
            U256::from(1_000_000_000_000u128),
        ],
        gamma: None,
        price_scale: vec![],
        fee_out: None,
    })
}

fn bench_simulate_swap(c: &mut Criterion) {
    let (a, b) = (token(1), token(2));
    let mut group = c.benchmark_group("simulate_swap");

    let v2 = v2_pool(0x10, a, b, 1_000_000 * ONE, 3_000_000 * ONE);
    let amount = U256::from(ONE);
    group.bench_function("uniswap_v2", |bench| {
        bench.iter(|| black_box(v2.simulate_swap(black_box(a), b, amount)))
    });

    let solidly = solidly_pool(a, b);
    group.bench_function("solidly_v2", |bench| {
        bench.iter(|| black_box(solidly.simulate_swap(black_box(a), b, amount)))
    });

    let curve = curve_stable_pool(a, b);
    let curve_amount = U256::from(1_000_000_000u128); // 1000 units (6 dec)
    group.bench_function("curve_stableswap", |bench| {
        bench.iter(|| black_box(curve.simulate_swap(black_box(a), b, curve_amount)))
    });

    group.finish();
}

fn bench_event_apply(c: &mut Criterion) {
    let (a, b) = (token(1), token(2));
    let pool_addr = Address::with_last_byte(0x10);
    let topic0 = event_topics_for(Variant::UniswapV2)[0];

    // A Uniswap-V2 Sync log with fresh reserves.
    let mut data = Vec::with_capacity(64);
    data.extend_from_slice(&U256::from(1_100_000u128 * ONE).to_be_bytes::<32>());
    data.extend_from_slice(&U256::from(2_900_000u128 * ONE).to_be_bytes::<32>());
    let log = alloy_rpc_types_eth::Log {
        inner: alloy_primitives::Log {
            address: pool_addr,
            data: LogData::new_unchecked(vec![topic0], data.into()),
        },
        ..Default::default()
    };

    c.bench_function("event_apply/uniswap_v2_sync", |bench| {
        let mut pool = v2_pool(0x10, a, b, 1_000_000 * ONE, 3_000_000 * ONE);
        bench.iter(|| {
            let _ = black_box(apply_log(black_box(&mut pool), &log));
        })
    });
}

/// Build a fully-connected mesh of `k` tokens with one V2 pool per pair, with
/// slightly varied reserves so the market is not perfectly balanced.
fn mesh(k: u8) -> (HashMap<Address, LocalAMM>, Address) {
    let tokens: Vec<Address> = (1..=k).map(token).collect();
    let mut pools = HashMap::new();
    let mut addr_byte = 0x20u8;
    for i in 0..tokens.len() {
        for j in (i + 1)..tokens.len() {
            // Vary reserves deterministically to create price differences.
            let r0 = (1_000_000 + (i as u128) * 7_000) * ONE;
            let r1 = (1_000_000 + (j as u128) * 11_000) * ONE;
            let amm = v2_pool(addr_byte, tokens[i], tokens[j], r0, r1);
            pools.insert(amm.address(), amm);
            addr_byte = addr_byte.wrapping_add(1);
        }
    }
    (pools, tokens[0])
}

fn bench_triangular_search(c: &mut Criterion) {
    let mut group = c.benchmark_group("triangular_search");
    let min_in = U256::from(ONE);
    let max_in = U256::from(50_000u64) * U256::from(ONE);

    for k in [5u8, 8, 11] {
        let (pools, start) = mesh(k);
        let n_routes = triangular_routes(&pools, start).len();
        group.bench_with_input(
            BenchmarkId::new("tokens", format!("{k}_routes_{n_routes}")),
            &(pools, start),
            |bench, (pools, start)| {
                bench.iter(|| {
                    black_box(find_triangular_arbitrages(
                        black_box(pools),
                        *start,
                        min_in,
                        max_in,
                    ))
                })
            },
        );
    }
    group.finish();
}

fn bench_simulate_route(c: &mut Criterion) {
    let (pools, start) = mesh(8);
    let routes = triangular_routes(&pools, start);
    let route = routes.into_iter().next().expect("at least one route");
    let amount = U256::from(ONE);
    c.bench_function("simulate_route/3_leg", |bench| {
        bench.iter(|| black_box(simulate_route(black_box(&pools), &route, amount)))
    });
}

criterion_group!(
    benches,
    bench_simulate_swap,
    bench_event_apply,
    bench_triangular_search,
    bench_simulate_route,
);
criterion_main!(benches);
