//! RPC-warmed micro-benchmarks for the crate's **offline hot path**.
//!
//! The whole point of `evm-amm-state` is that, once a pool is cold-started into
//! the cache, quoting and event-application are fully offline (no RPC). These
//! benches measure exactly that steady state:
//!
//! - `simulate_swap/<protocol>` — one offline quote (the pool's own on-chain
//!   quote entrypoint executed in revm against the warmed cache). This is the
//!   number that matters for repeated work like arbitrage scanning.
//! - `reactive_apply/v2_sync` — decode + route + apply one `Sync` event (the
//!   exact-write reactive path).
//!
//! Cold-start latency is also reported once, but it is **network-bound** (it
//! reads storage from the archive node), so it is timed coarsely rather than as
//! a criterion micro-bench.
//!
//! Env-gated: the warm-up forks mainnet/Base at a pinned block via
//! `E2E_RPC_URL`, so without it `cargo bench` is a clean no-op. To run:
//! ```text
//! E2E_RPC_URL=<archive-url> cargo bench --bench swap_sim
//! ```
//! Solidly uses Base — `E2E_BASE_RPC_URL`, or `E2E_RPC_URL` with the Alchemy
//! `eth-mainnet` host swapped to `base-mainnet`.

use std::sync::Arc;
use std::time::Instant;

use alloy_eips::{BlockId, BlockNumberOrTag};
use alloy_network::AnyNetwork;
use alloy_primitives::{Address, B256, Bytes, Log, U256, address, b256, keccak256};
use alloy_provider::RootProvider;
use criterion::{Criterion, black_box};

use evm_amm_state::adapters::driver::AdapterDriver;
use evm_amm_state::adapters::storage::{SolidlyStorageLayout, V3StorageLayout};
use evm_amm_state::adapters::{
    AdapterRegistry, AmmAdapter, BalancerV2Adapter, BalancerV2Metadata, ColdStartPolicy,
    CurveAdapter, CurveMetadata, CurveVariant, PoolKey, PoolRegistration, ProtocolMetadata,
    SimConfig, SolidlyV2Adapter, SolidlyV2Metadata, UniswapV2Adapter, UniswapV2Metadata,
    UniswapV3Adapter, V3Metadata,
};
use evm_fork_cache::cache::EvmCache;

// --- Pinned fork + pool addresses (same as tests/adapter_swap_sim_rpc.rs) ---
const FORK_BLOCK: u64 = 20_000_000;

const USDC: Address = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
const WETH: Address = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");

const V3_QUOTER_V2: Address = address!("61fFE014bA17989E743c5F6cB21bF9697530B21e");
const V3_USDC_WETH_005: Address = address!("88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640");

const V2_ROUTER_02: Address = address!("7a250d5630B4cF539739dF2C5dAcb4c659F2488D");
const V2_USDC_WETH_PAIR: Address = address!("B4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc");

const BALANCER_VAULT: Address = address!("BA12222222228d8Ba445958a75a0704d566BF2C8");
const BALANCER_BAL_WETH_POOL_ID: B256 =
    b256!("5c6ee304399dbdb9c8ef030ab642b10820db8f56000200000000000000000014");
const BAL: Address = address!("ba100000625a3754423978a60c9317c58a424e3D");

const CURVE_3POOL: Address = address!("bEbc44782C7dB0a1A60Cb6fe97d0b483032FF1C7");
const DAI: Address = address!("6B175474E89094C44Da98b954EedeAC495271d0F");
const USDT: Address = address!("dAC17F958D2ee523a2206206994597C13D831ec7");

const TRICRYPTO2: Address = address!("D51a44d3FaE010294C616388b506AcdA1bfAAE46");
const WBTC: Address = address!("2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599");

const SOLIDLY_FORK_BLOCK: u64 = 47_700_000;
const BASE_WETH: Address = address!("4200000000000000000000000000000000000006");
const BASE_USDC: Address = address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913");
const AERODROME_WETH_USDC: Address = address!("cDAC0d6c6C59727a65F871236188350531885C43");

type Rt = tokio::runtime::Runtime;

fn main() {
    let Ok(url) = std::env::var("E2E_RPC_URL") else {
        eprintln!(
            "E2E_RPC_URL unset — swap_sim benches are RPC-warmed; skipping.\n\
             Run with: E2E_RPC_URL=<archive-url> cargo bench --bench swap_sim"
        );
        return;
    };

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    // Cold-start reads storage from the archive node via evm-fork-cache, which
    // needs a live tokio reactor on the calling thread. Entering the runtime for
    // the whole bench gives every (sync) cold_start that context — the same
    // context the `#[tokio::test]` RPC-parity tests run inside.
    let _guard = rt.enter();
    let mut c = Criterion::default().configure_from_args();

    // Headline: the offline quote, per protocol.
    bench_v3(&mut c, &rt, &url);
    bench_v2(&mut c, &rt, &url);
    bench_balancer(&mut c, &rt, &url);
    bench_curve_stable(&mut c, &rt, &url);
    bench_curve_crypto(&mut c, &rt, &url);

    // Solidly lives on Base.
    if let Some(base) = base_url(&url) {
        bench_solidly(&mut c, &rt, &base);
    } else {
        eprintln!("no Base RPC (E2E_BASE_RPC_URL / Alchemy E2E_RPC_URL); skipping Solidly bench");
    }

    // Reactive: one exact-write event apply.
    bench_reactive_v2_sync(&mut c, &rt, &url);

    // One-time, network-bound: report coarsely.
    report_cold_start_latency(&rt, &url);

    c.final_summary();
}

async fn fork_cache(url: &str, block: u64) -> EvmCache {
    let provider = RootProvider::<AnyNetwork>::connect(url)
        .await
        .expect("connect RPC url");
    EvmCache::at_block(
        Arc::new(provider),
        BlockId::Number(BlockNumberOrTag::Number(block)),
    )
    .await
}

/// Base RPC url: explicit `E2E_BASE_RPC_URL`, else derive from an Alchemy
/// `E2E_RPC_URL` by host-swapping `eth-mainnet` -> `base-mainnet`.
fn base_url(eth_url: &str) -> Option<String> {
    if let Ok(url) = std::env::var("E2E_BASE_RPC_URL") {
        return Some(url);
    }
    eth_url
        .contains("eth-mainnet")
        .then(|| eth_url.replace("eth-mainnet", "base-mainnet"))
}

fn quote_config() -> SimConfig {
    SimConfig::default()
        .with_v3_quoter(V3_QUOTER_V2)
        .with_v2_router(V2_ROUTER_02)
}

fn bench_v3(c: &mut Criterion, rt: &Rt, url: &str) {
    let adapter = UniswapV3Adapter::default();
    let mut cache = rt.block_on(fork_cache(url, FORK_BLOCK));
    let mut reg = PoolRegistration::new(PoolKey::UniswapV3(V3_USDC_WETH_005))
        .with_state_address(V3_USDC_WETH_005)
        .with_metadata(ProtocolMetadata::UniswapV3(V3Metadata {
            token0: Some(USDC),
            token1: Some(WETH),
            fee: Some(500),
            tick_spacing: Some(10),
            storage_layout: Some(V3StorageLayout::uniswap(10)),
        }));
    cold_start(&mut reg, &mut cache);

    let cfg = quote_config();
    let amount_in = U256::from(1_000_000_u64); // 1 USDC
    c.bench_function("simulate_swap/uniswap_v3", |b| {
        b.iter(|| {
            let q = adapter
                .simulate_swap(&reg, &mut cache, USDC, WETH, amount_in, &cfg)
                .expect("v3 quote");
            black_box(q.amount_out)
        })
    });
}

fn bench_v2(c: &mut Criterion, rt: &Rt, url: &str) {
    let adapter = UniswapV2Adapter::default();
    let mut cache = rt.block_on(fork_cache(url, FORK_BLOCK));
    let mut reg = PoolRegistration::new(PoolKey::UniswapV2(V2_USDC_WETH_PAIR))
        .with_state_address(V2_USDC_WETH_PAIR)
        .with_metadata(ProtocolMetadata::UniswapV2(UniswapV2Metadata {
            token0: Some(USDC),
            token1: Some(WETH),
            fee_bps: Some(30),
        }));
    cold_start(&mut reg, &mut cache);

    let cfg = quote_config();
    let amount_in = U256::from(1_000_000_u64); // 1 USDC
    c.bench_function("simulate_swap/uniswap_v2", |b| {
        b.iter(|| {
            let q = adapter
                .simulate_swap(&reg, &mut cache, USDC, WETH, amount_in, &cfg)
                .expect("v2 quote");
            black_box(q.amount_out)
        })
    });
}

fn bench_balancer(c: &mut Criterion, rt: &Rt, url: &str) {
    let adapter = BalancerV2Adapter::default();
    let mut cache = rt.block_on(fork_cache(url, FORK_BLOCK));
    let mut reg = PoolRegistration::new(PoolKey::BalancerV2(BALANCER_BAL_WETH_POOL_ID))
        .with_state_address(BALANCER_VAULT)
        .with_metadata(ProtocolMetadata::BalancerV2(BalancerV2Metadata {
            vault: Some(BALANCER_VAULT),
            ..Default::default()
        }));
    cold_start(&mut reg, &mut cache);

    let cfg = SimConfig::default();
    let amount_in = U256::from(1_000_000_000_000_000_000_u64); // 1 BAL
    c.bench_function("simulate_swap/balancer_v2", |b| {
        b.iter(|| {
            let q = adapter
                .simulate_swap(&reg, &mut cache, BAL, WETH, amount_in, &cfg)
                .expect("balancer quote");
            black_box(q.amount_out)
        })
    });
}

fn bench_curve_stable(c: &mut Criterion, rt: &Rt, url: &str) {
    let adapter = CurveAdapter::default();
    let mut cache = rt.block_on(fork_cache(url, FORK_BLOCK));
    let mut reg = PoolRegistration::new(PoolKey::Curve(CURVE_3POOL))
        .with_state_address(CURVE_3POOL)
        .with_metadata(ProtocolMetadata::Curve(CurveMetadata {
            coins: vec![DAI, USDC, USDT],
            discovered_slots: Vec::new(),
            variant: CurveVariant::StableSwap,
        }));
    cold_start(&mut reg, &mut cache);

    let cfg = SimConfig::default();
    let amount_in = U256::from(1_000_000_000_000_000_000_u128); // 1 DAI
    c.bench_function("simulate_swap/curve_stableswap", |b| {
        b.iter(|| {
            let q = adapter
                .simulate_swap(&reg, &mut cache, DAI, USDC, amount_in, &cfg)
                .expect("curve stable quote");
            black_box(q.amount_out)
        })
    });
}

fn bench_curve_crypto(c: &mut Criterion, rt: &Rt, url: &str) {
    let adapter = CurveAdapter::default();
    let mut cache = rt.block_on(fork_cache(url, FORK_BLOCK));
    let mut reg = PoolRegistration::new(PoolKey::Curve(TRICRYPTO2))
        .with_state_address(TRICRYPTO2)
        .with_metadata(ProtocolMetadata::Curve(CurveMetadata {
            coins: vec![USDT, WBTC, WETH],
            discovered_slots: Vec::new(),
            variant: CurveVariant::CryptoSwap,
        }));
    cold_start(&mut reg, &mut cache);

    let cfg = SimConfig::default();
    let amount_in = U256::from(100_000_000_u64); // 100 USDT
    c.bench_function("simulate_swap/curve_cryptoswap", |b| {
        b.iter(|| {
            let q = adapter
                .simulate_swap(&reg, &mut cache, USDT, WBTC, amount_in, &cfg)
                .expect("curve crypto quote");
            black_box(q.amount_out)
        })
    });
}

fn bench_solidly(c: &mut Criterion, rt: &Rt, base_url: &str) {
    let adapter = SolidlyV2Adapter::default();
    let mut cache = rt.block_on(fork_cache(base_url, SOLIDLY_FORK_BLOCK));
    let layout = SolidlyStorageLayout::new(
        U256::from(20_u64),
        U256::from(21_u64),
        U256::from(13_u64),
        U256::from(14_u64),
    );
    let mut reg = PoolRegistration::new(PoolKey::SolidlyV2(AERODROME_WETH_USDC))
        .with_state_address(AERODROME_WETH_USDC)
        .with_metadata(ProtocolMetadata::SolidlyV2(SolidlyV2Metadata {
            token0: None,
            token1: None,
            stable: Some(false),
            storage_layout: Some(layout),
        }));
    cold_start(&mut reg, &mut cache);

    let cfg = SimConfig::default();
    let amount_in = U256::from(1_000_000_000_000_000_u64); // 0.001 WETH
    c.bench_function("simulate_swap/solidly_v2", |b| {
        b.iter(|| {
            let q = adapter
                .simulate_swap(&reg, &mut cache, BASE_WETH, BASE_USDC, amount_in, &cfg)
                .expect("solidly quote");
            black_box(q.amount_out)
        })
    });
}

/// Reactive exact-write path: decode + route + apply one Uniswap V2 `Sync`.
fn bench_reactive_v2_sync(c: &mut Criterion, rt: &Rt, url: &str) {
    let adapter = UniswapV2Adapter::default();
    let mut cache = rt.block_on(fork_cache(url, FORK_BLOCK));
    let mut reg = PoolRegistration::new(PoolKey::UniswapV2(V2_USDC_WETH_PAIR))
        .with_state_address(V2_USDC_WETH_PAIR)
        .with_metadata(ProtocolMetadata::UniswapV2(UniswapV2Metadata {
            token0: Some(USDC),
            token1: Some(WETH),
            fee_bps: Some(30),
        }));
    cold_start(&mut reg, &mut cache);
    reg.event_sources = adapter.event_sources(&reg);

    // Drive events through the registry (route by topic + address) -> adapter.
    let mut registry = AdapterRegistry::new();
    registry
        .register_adapter(Arc::new(UniswapV2Adapter::default()))
        .expect("register adapter");
    registry.register_pool(reg).expect("register pool");
    let driver = AdapterDriver::new(registry);

    // A representative Sync(uint112,uint112) carrying absolute reserves.
    let topic = keccak256("Sync(uint112,uint112)");
    let mut data = Vec::with_capacity(64);
    data.extend_from_slice(&U256::from(40_000_000_000_000_u64).to_be_bytes::<32>()); // reserve0
    data.extend_from_slice(&U256::from(20_000_000_000_000_000_000_u128).to_be_bytes::<32>()); // reserve1
    let log = Log::new_unchecked(V2_USDC_WETH_PAIR, vec![topic], Bytes::from(data));

    // Fail fast if routing/wiring is wrong, so the bench measures a real apply.
    let report = driver
        .apply_log(&mut cache, &log)
        .expect("apply_log ok")
        .expect("Sync must route + apply (non-None report)");
    black_box(report);

    c.bench_function("reactive_apply/v2_sync", |b| {
        b.iter(|| {
            let r = driver.apply_log(&mut cache, &log).expect("apply ok");
            black_box(r)
        })
    });
}

/// Cold-start latency is network-bound (it reads storage from the archive node),
/// so report it coarsely rather than as a criterion micro-bench. Treated as a
/// one-time cost — it is amortized over every subsequent offline quote.
fn report_cold_start_latency(rt: &Rt, url: &str) {
    let adapter = UniswapV3Adapter::default();
    let mut cache = rt.block_on(fork_cache(url, FORK_BLOCK));
    let mut reg = PoolRegistration::new(PoolKey::UniswapV3(V3_USDC_WETH_005))
        .with_state_address(V3_USDC_WETH_005)
        .with_metadata(ProtocolMetadata::UniswapV3(V3Metadata {
            token0: Some(USDC),
            token1: Some(WETH),
            fee: Some(500),
            tick_spacing: Some(10),
            storage_layout: Some(V3StorageLayout::uniswap(10)),
        }));
    let _ = &adapter;

    let start = Instant::now();
    cold_start(&mut reg, &mut cache);
    let elapsed = start.elapsed();
    eprintln!(
        "\ncold_start/uniswap_v3 (one-time, NETWORK-BOUND): {:?} \
         — depends on archive-node latency; amortized over all later offline quotes\n",
        elapsed
    );
}

fn cold_start(reg: &mut PoolRegistration, cache: &mut EvmCache) {
    let mut registry = AdapterRegistry::new();
    registry
        .register_adapter(adapter_for(&reg.key))
        .expect("register adapter");
    registry
        .cold_start(reg, cache, ColdStartPolicy::Eager)
        .expect("cold-start");
}

fn adapter_for(key: &PoolKey) -> Arc<dyn AmmAdapter> {
    match key {
        PoolKey::UniswapV2(_) => Arc::new(UniswapV2Adapter::default()),
        PoolKey::UniswapV3(_) => Arc::new(UniswapV3Adapter::default()),
        PoolKey::BalancerV2(_) => Arc::new(BalancerV2Adapter::default()),
        PoolKey::SolidlyV2(_) => Arc::new(SolidlyV2Adapter::default()),
        PoolKey::Curve(_) => Arc::new(CurveAdapter::default()),
        other => panic!("no adapter wired for {other:?} in this bench"),
    }
}
