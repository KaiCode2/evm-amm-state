//! Fully-offline reactive-apply micro-benchmarks: decode + route + apply one
//! event through [`AdapterDriver`] for each **event-sourced** (exact-write,
//! no-RPC) hot path — Uniswap V2 `Sync`, Uniswap V3 `Mint`/`Burn` onto warm
//! ticks, and Balancer V2 vault `Swap`s onto probed cash fields.
//!
//! Unlike `swap_sim.rs` (which cold-starts real pools over RPC before its
//! micro-benches), this harness needs **no network and no env vars**: the
//! cache is a mock-transport [`EvmCache`] and the slots each apply path reads
//! are pre-warmed with the exact packed words the adapters expect (the same
//! fixtures as `tests/adapter_reactive.rs`). Run it anywhere:
//!
//! ```text
//! cargo bench --bench reactive_apply
//! ```
//!
//! Each measured iteration is one `AdapterDriver::apply_log`: topic routing,
//! ABI decode, packed-word arithmetic, and the cache write(s). Warm-path
//! invariants (no resync scheduled) are asserted once before measuring, and
//! initial values leave enough headroom that millions of repeated applies stay
//! on the warm path (a V3 burn never empties its tick, Balancer cash never
//! under/overflows its 112-bit field).

use std::sync::Arc;

use alloy_network::AnyNetwork;
use alloy_primitives::{Address, B256, Bytes, Log, U256, keccak256};
use alloy_provider::RootProvider;
use alloy_rpc_client::RpcClient;
use alloy_transport::mock::Asserter;
use criterion::{Criterion, black_box, criterion_group, criterion_main};
use evm_amm_state::adapters::storage::{
    V2_RESERVES_SLOT, V3StorageLayout, v3_tick_info_storage_keys_with_base,
};
use evm_amm_state::adapters::{
    AdapterDriver, AdapterRegistry, AmmAdapter, BalancerTokenBalance, BalancerV2Adapter,
    BalancerV2Metadata, ConcentratedLiquidityAdapter, PoolKey, PoolRegistration, ProtocolMetadata,
    UniswapV2Adapter, UniswapV2Metadata, V3Metadata,
};
use evm_fork_cache::StateUpdate;
use evm_fork_cache::cache::EvmCache;
use tokio::runtime::Runtime as Rt;

// --- offline cache -----------------------------------------------------------

fn mock_cache(rt: &Rt) -> EvmCache {
    rt.block_on(async {
        let asserter = Asserter::new();
        let client = RpcClient::mocked(asserter);
        let provider = RootProvider::<AnyNetwork>::new(client);
        EvmCache::new(Arc::new(provider)).await
    })
}

// --- log fixtures (mirroring tests/adapter_reactive.rs) -----------------------

fn word(value: U256) -> Vec<u8> {
    value.to_be_bytes::<32>().to_vec()
}

fn abi_words(values: impl IntoIterator<Item = U256>) -> Vec<u8> {
    values.into_iter().flat_map(word).collect()
}

fn address_word(address: Address) -> Vec<u8> {
    let mut bytes = [0u8; 32];
    bytes[12..].copy_from_slice(address.as_slice());
    bytes.to_vec()
}

fn topic_address(address: Address) -> B256 {
    let mut bytes = [0u8; 32];
    bytes[12..].copy_from_slice(address.as_slice());
    B256::from(bytes)
}

fn topic_i24(value: i32) -> B256 {
    let mut bytes = if value < 0 { [0xff; 32] } else { [0u8; 32] };
    let raw = value.to_be_bytes();
    bytes[29..32].copy_from_slice(&raw[1..4]);
    B256::from(bytes)
}

/// Pack a `Tick.Info` word 0: `liquidityGross` (low 128) + `liquidityNet`
/// (high 128, two's complement).
fn packed_tick_word0(gross: u128, net: i128) -> U256 {
    U256::from(gross) | (U256::from(net as u128) << 128)
}

/// Pack a V3 `slot0`: `sqrtPriceX96` (160 bits) + current tick (24 bits).
fn v3_slot0_word(sqrt_price: U256, tick: i32) -> U256 {
    sqrt_price | (U256::from((tick as u32) & 0x00FF_FFFF) << 160)
}

/// A Balancer poolId: 20 pool-address bytes, then the 2-byte specialization.
fn balancer_pool_id(specialization: u16, seed: u8) -> B256 {
    let mut bytes = [seed; 32];
    bytes[20..22].copy_from_slice(&specialization.to_be_bytes());
    B256::from(bytes)
}

// --- benches ------------------------------------------------------------------

/// Uniswap V2 `Sync`: one exact masked write of the packed reserves word.
fn bench_v2_sync(c: &mut Criterion, rt: &Rt) {
    let pair = Address::repeat_byte(0x21);
    let adapter = UniswapV2Adapter::default();
    let mut cache = mock_cache(rt);
    // The masked write needs the slot warm; any packed word will do.
    cache.apply_updates(&[StateUpdate::slot(
        pair,
        V2_RESERVES_SLOT,
        (U256::from(1_u64) << 224) | (U256::from(1_u64) << 112) | U256::from(1_u64),
    )]);

    let mut reg = PoolRegistration::new(PoolKey::UniswapV2(pair))
        .with_state_address(pair)
        .with_metadata(ProtocolMetadata::UniswapV2(
            UniswapV2Metadata::default()
                .with_token0(Address::repeat_byte(0x01))
                .with_token1(Address::repeat_byte(0x02))
                .with_fee_bps(30),
        ));
    reg.event_sources = adapter.event_sources(&reg);
    let mut registry = AdapterRegistry::new();
    registry
        .register_adapter(Arc::new(adapter))
        .expect("adapter");
    registry.register_pool(reg).expect("pool");
    let driver = AdapterDriver::new(registry);

    let reserve0 = U256::from(40_000_000_000_000_u64);
    let reserve1 = U256::from(20_000_000_000_000_000_000_u128);
    let log = Log::new_unchecked(
        pair,
        vec![keccak256("Sync(uint112,uint112)")],
        Bytes::from(abi_words([reserve0, reserve1])),
    );

    // Pre-flight: the write must land exactly before we measure anything.
    driver
        .apply_log(&mut cache, &log)
        .expect("apply ok")
        .expect("Sync must route + apply");
    let packed = cache
        .cached_storage_value(pair, V2_RESERVES_SLOT)
        .expect("warm");
    assert_eq!(
        packed & ((U256::from(1_u64) << 112) - U256::from(1_u64)),
        reserve0
    );

    c.bench_function("reactive_apply/v2_sync", |b| {
        b.iter(|| {
            let r = driver.apply_log(&mut cache, &log).expect("apply ok");
            black_box(r)
        })
    });
}

/// Uniswap V3 `Mint`/`Burn` onto warm, already-initialized boundary ticks: the
/// event-sourced path writes the packed gross/net words and the in-range
/// global liquidity directly — no resync.
fn bench_v3_liquidity(c: &mut Criterion, rt: &Rt) {
    let pool = Address::repeat_byte(0x42);
    let layout = V3StorageLayout::uniswap(60);
    let (tick_lower, tick_upper) = (60, 180); // current tick 120 in [60, 180)
    let lower_key = v3_tick_info_storage_keys_with_base(tick_lower, layout.ticks_base_slot)[0];
    let upper_key = v3_tick_info_storage_keys_with_base(tick_upper, layout.ticks_base_slot)[0];

    let adapter = ConcentratedLiquidityAdapter::default();
    let mut cache = mock_cache(rt);
    // Headroom so millions of repeated applies stay on the warm path: a burn
    // never empties its tick, a mint never overflows gross or global liquidity.
    let headroom = u128::MAX / 2;
    cache.apply_updates(&[
        StateUpdate::slot(
            pool,
            layout.slot0_slot,
            v3_slot0_word(U256::from(1_u64), 120),
        ),
        StateUpdate::slot(pool, layout.liquidity_slot, U256::from(headroom)),
        StateUpdate::slot(pool, lower_key, packed_tick_word0(headroom, 40)),
        StateUpdate::slot(pool, upper_key, packed_tick_word0(headroom, -40)),
    ]);

    let mut reg = PoolRegistration::new(PoolKey::UniswapV3(pool))
        .with_state_address(pool)
        .with_metadata(ProtocolMetadata::UniswapV3(
            V3Metadata::default().with_storage_layout(layout),
        ));
    reg.event_sources = adapter.event_sources(&reg);
    let mut registry = AdapterRegistry::new();
    registry
        .register_adapter(Arc::new(adapter))
        .expect("adapter");
    registry.register_pool(reg).expect("pool");
    let driver = AdapterDriver::new(registry);

    let mint_log = Log::new_unchecked(
        pool,
        vec![
            keccak256("Mint(address,address,int24,int24,uint128,uint256,uint256)"),
            topic_address(Address::repeat_byte(0x04)),
            topic_i24(tick_lower),
            topic_i24(tick_upper),
        ],
        Bytes::from({
            let mut data = address_word(Address::repeat_byte(0x03));
            data.extend(abi_words([
                U256::from(7_u64), // amount
                U256::from(8_u64),
                U256::from(9_u64),
            ]));
            data
        }),
    );
    let burn_log = Log::new_unchecked(
        pool,
        vec![
            keccak256("Burn(address,int24,int24,uint128,uint256,uint256)"),
            topic_address(Address::repeat_byte(0x04)),
            topic_i24(tick_lower),
            topic_i24(tick_upper),
        ],
        Bytes::from(abi_words([
            U256::from(7_u64), // amount
            U256::from(8_u64),
            U256::from(9_u64),
        ])),
    );

    // Pre-flight both directions: gross must move by ±7 (warm event-sourcing),
    // never a resync-only report.
    driver
        .apply_log(&mut cache, &mint_log)
        .expect("apply ok")
        .expect("Mint must route + apply");
    let after_mint = cache.cached_storage_value(pool, lower_key).expect("warm");
    assert_eq!(
        after_mint & ((U256::from(1_u64) << 128) - U256::from(1_u64)),
        U256::from(headroom + 7)
    );
    driver
        .apply_log(&mut cache, &burn_log)
        .expect("apply ok")
        .expect("Burn must route + apply");
    let after_burn = cache.cached_storage_value(pool, lower_key).expect("warm");
    assert_eq!(
        after_burn & ((U256::from(1_u64) << 128) - U256::from(1_u64)),
        U256::from(headroom)
    );

    c.bench_function("reactive_apply/v3_mint_warm", |b| {
        b.iter(|| {
            let r = driver.apply_log(&mut cache, &mint_log).expect("apply ok");
            black_box(r)
        })
    });
    c.bench_function("reactive_apply/v3_burn_warm", |b| {
        b.iter(|| {
            let r = driver.apply_log(&mut cache, &burn_log).expect("apply ok");
            black_box(r)
        })
    });
}

/// Balancer V2 vault `Swap` with probed cash fields: exact 112-bit field
/// writes — the TWO_TOKEN shared slot gets one combined write, a GENERAL pool
/// two per-token writes.
fn bench_balancer_swap(c: &mut Criterion, rt: &Rt) {
    let vault = Address::repeat_byte(0x52);
    let token_in = Address::repeat_byte(0x01);
    let token_out = Address::repeat_byte(0x02);

    // TWO_TOKEN: both cash fields share one slot (in = low, out = high).
    let two_token_id = balancer_pool_id(2, 0xc3);
    let shared_slot = U256::from(0x77_u64);
    // GENERAL: one slot per token (both low fields).
    let general_id = balancer_pool_id(0, 0xd4);
    let (slot_in, slot_out) = (U256::from(0x11_u64), U256::from(0x22_u64));

    let adapter = Arc::new(BalancerV2Adapter::default());
    let mut cache = mock_cache(rt);
    // cash_out starts near the top of the 112-bit field, cash_in near the
    // bottom: millions of (+30 in / -20 out) applies stay in range.
    let cash_in0 = U256::from(1_u64) << 80;
    let cash_out0 = U256::from(1_u64) << 111;
    cache.apply_updates(&[
        StateUpdate::slot(
            vault,
            shared_slot,
            (U256::from(0xABCD_u64) << 224) | (cash_out0 << 112) | cash_in0,
        ),
        StateUpdate::slot(vault, slot_in, cash_in0),
        StateUpdate::slot(vault, slot_out, cash_out0),
    ]);

    let mut registry = AdapterRegistry::new();
    registry.register_adapter(adapter.clone()).expect("adapter");
    for (pool_id, token_cash) in [
        (
            two_token_id,
            vec![
                BalancerTokenBalance::new(token_in, shared_slot, false),
                BalancerTokenBalance::new(token_out, shared_slot, true),
            ],
        ),
        (
            general_id,
            vec![
                BalancerTokenBalance::new(token_in, slot_in, false),
                BalancerTokenBalance::new(token_out, slot_out, false),
            ],
        ),
    ] {
        let mut reg = PoolRegistration::new(PoolKey::BalancerV2(pool_id))
            .with_state_address(vault)
            .with_metadata(ProtocolMetadata::BalancerV2(
                BalancerV2Metadata::default()
                    .with_vault(vault)
                    .with_tokens([token_in, token_out])
                    .with_token_cash(token_cash),
            ));
        reg.event_sources = adapter.event_sources(&reg);
        registry.register_pool(reg).expect("pool");
    }
    let driver = AdapterDriver::new(registry);

    let swap_log = |pool_id: B256| {
        Log::new_unchecked(
            vault,
            vec![
                keccak256("Swap(bytes32,address,address,uint256,uint256)"),
                pool_id,
                topic_address(token_in),
                topic_address(token_out),
            ],
            Bytes::from(abi_words([U256::from(30_u64), U256::from(20_u64)])),
        )
    };
    let two_token_log = swap_log(two_token_id);
    let general_log = swap_log(general_id);

    // Pre-flight: cash fields must move exactly (event-sourced, not resync).
    driver
        .apply_log(&mut cache, &two_token_log)
        .expect("apply ok")
        .expect("Swap must route + apply");
    let shared = cache
        .cached_storage_value(vault, shared_slot)
        .expect("warm");
    let mask112 = (U256::from(1_u64) << 112) - U256::from(1_u64);
    assert_eq!(shared & mask112, cash_in0 + U256::from(30_u64));
    assert_eq!((shared >> 112) & mask112, cash_out0 - U256::from(20_u64));
    driver
        .apply_log(&mut cache, &general_log)
        .expect("apply ok")
        .expect("Swap must route + apply");
    assert_eq!(
        cache.cached_storage_value(vault, slot_in).expect("warm") & mask112,
        cash_in0 + U256::from(30_u64)
    );

    c.bench_function("reactive_apply/balancer_swap_two_token", |b| {
        b.iter(|| {
            let r = driver
                .apply_log(&mut cache, &two_token_log)
                .expect("apply ok");
            black_box(r)
        })
    });
    c.bench_function("reactive_apply/balancer_swap_general", |b| {
        b.iter(|| {
            let r = driver
                .apply_log(&mut cache, &general_log)
                .expect("apply ok");
            black_box(r)
        })
    });
}

fn benches(c: &mut Criterion) {
    let rt = Rt::new().expect("tokio runtime");
    bench_v2_sync(c, &rt);
    bench_v3_liquidity(c, &rt);
    bench_balancer_swap(c, &rt);
}

criterion_group!(reactive_apply, benches);
criterion_main!(reactive_apply);
