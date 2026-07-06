//! V3 sync programs: offline EVM execution tests.
//!
//! The generated full/partial sync bytecode is executed inside a
//! mocked-provider `EvmCache` (real revm, zero network) against synthetic
//! pools whose storage is seeded slot by slot. This proves, end to end and
//! offline, exactly what the live `eth_call` override path executes:
//! bitmap walking across negative/positive/edge words, in-EVM keccak slot
//! derivation, tick-record emission, cardinality-driven observation reads,
//! layout parameterization (Uniswap vs Pancake bases), and the
//! decode → materialize → inject round trip.

use std::sync::Arc;

use alloy_primitives::{Address, Bytes, U256};
use alloy_provider::RootProvider;
use alloy_provider::network::AnyNetwork;
use alloy_rpc_client::RpcClient;
use alloy_transport::mock::Asserter;
use anyhow::{Result, anyhow};
use evm_amm_state::adapters::storage::{
    V3StorageLayout, v3_tick_bitmap_storage_key_with_base, v3_tick_info_storage_keys_with_base,
    v3_word_position,
};
use evm_amm_state::adapters::v3_sync::{
    V3PoolSnapshot, V3SyncSpec, V3TickSnapshot, build_full_sync_program,
    build_partial_sync_program, decode_full_sync, decode_partial_sync, full_word_range,
    partial_storage_entries, partial_sync_calldata,
};
use evm_fork_cache::cache::EvmCache;
use revm::context::result::ExecutionResult;
use revm::state::{AccountInfo, Bytecode};

const CALLER: Address = Address::ZERO;
const POOL: Address = Address::repeat_byte(0x33);

/// Build an offline cache over a mocked provider (no network).
async fn mock_cache() -> EvmCache {
    let asserter = Asserter::new();
    let client = RpcClient::mocked(asserter);
    let provider = RootProvider::<AnyNetwork>::new(client);
    let mut cache = EvmCache::new(Arc::new(provider)).await;
    cache
        .db_mut()
        .insert_account_info(CALLER, AccountInfo::default());
    cache
}

/// Install `code` at `addr` with seeded storage; unseeded slots read zero
/// without touching the (mocked) backend — the offline equivalent of the
/// live path's code override.
fn install(cache: &mut EvmCache, addr: Address, code: Bytes, slots: &[(U256, U256)]) {
    let bytecode = Bytecode::new_raw(code);
    let code_hash = bytecode.hash_slow();
    cache.db_mut().insert_account_info(
        addr,
        AccountInfo {
            balance: U256::ZERO,
            nonce: 0,
            code: Some(bytecode),
            code_hash,
            account_id: None,
        },
    );
    cache
        .db_mut()
        .replace_account_storage(addr, Default::default())
        .expect("mark storage local");
    for (slot, value) in slots {
        cache
            .db_mut()
            .insert_account_storage(addr, *slot, *value)
            .expect("seed slot");
    }
}

fn run(cache: &mut EvmCache, to: Address, calldata: Bytes) -> Result<Bytes> {
    match cache.call_raw(CALLER, to, calldata, false)? {
        ExecutionResult::Success { output, .. } => Ok(output.into_data()),
        other => Err(anyhow!("sync program did not succeed: {other:?}")),
    }
}

/// A synthetic spacing-10 Uniswap pool: 5 initialized ticks spread across a
/// negative edge word, an interior negative word, word zero, and the positive
/// edge word, plus 3 observations and nonzero statics.
struct SyntheticPool {
    spec: V3SyncSpec,
    seeds: Vec<(U256, U256)>,
    expected: V3PoolSnapshot,
}

fn synthetic_uniswap_pool() -> SyntheticPool {
    let layout = V3StorageLayout::uniswap(10);
    let spec = V3SyncSpec::uniswap(layout);

    // slot0: junk price | tick=12 | obsIndex=1 | cardinality=3 | next=4 | unlocked.
    let slot0 = U256::from(1_234_567u64)
        | (U256::from(12u64) << 160usize)
        | (U256::from(1u64) << 184usize)
        | (U256::from(3u64) << 200usize)
        | (U256::from(4u64) << 216usize)
        | (U256::from(1u64) << 240usize);
    let statics_values = [
        slot0,
        U256::from(111u64),     // feeGrowthGlobal0X128
        U256::from(222u64),     // feeGrowthGlobal1X128
        U256::from(777u64),     // protocolFees
        U256::from(999_888u64), // liquidity
    ];

    // Ascending ticks; the program must emit them in exactly this order.
    let tick_indices = [-887_270i32, -60, 0, 10, 887_270];
    let ticks: Vec<V3TickSnapshot> = tick_indices
        .iter()
        .enumerate()
        .map(|(i, tick)| {
            V3TickSnapshot::new(
                *tick,
                std::array::from_fn(|k| U256::from(10_000 + (i as u64) * 100 + k as u64)),
            )
        })
        .collect();

    let observations = vec![
        U256::from(0xAAA1u64),
        U256::from(0xAAA2u64),
        U256::from(0xAAA3u64),
    ];

    let mut seeds: Vec<(U256, U256)> = spec
        .static_slots
        .iter()
        .copied()
        .zip(statics_values)
        .collect();
    // Bitmap words derived from the ticks (bit = compressed mod 256).
    for (word, bits) in [
        (-347i16, U256::from(1u64) << 105), // tick -887270
        (-1, U256::from(1u64) << 250),      // tick -60
        (0, U256::from(0b11u64)),           // ticks 0, 10
        (346, U256::from(1u64) << 151),     // tick 887270
    ] {
        seeds.push((
            v3_tick_bitmap_storage_key_with_base(word, layout.tick_bitmap_base_slot),
            bits,
        ));
    }
    for tick in &ticks {
        let keys = v3_tick_info_storage_keys_with_base(tick.tick, layout.ticks_base_slot);
        seeds.extend(keys.into_iter().zip(tick.info));
    }
    for (i, value) in observations.iter().enumerate() {
        seeds.push((U256::from(8 + i as u64), *value));
    }

    let expected = V3PoolSnapshot::new(
        spec.static_slots
            .iter()
            .copied()
            .zip(statics_values)
            .collect(),
        ticks,
        observations,
    );
    SyntheticPool {
        spec,
        seeds,
        expected,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn full_sync_program_returns_exact_pool_state() -> Result<()> {
    let pool = synthetic_uniswap_pool();
    let mut cache = mock_cache().await;
    install(
        &mut cache,
        POOL,
        build_full_sync_program(&pool.spec),
        &pool.seeds,
    );

    let output = run(&mut cache, POOL, Bytes::new())?;
    let snapshot = decode_full_sync(&pool.spec, &output)?;

    assert_eq!(snapshot, pool.expected);
    // Ticks must come out in ascending order (LSB-first bit walk).
    let mut sorted = snapshot.ticks.clone();
    sorted.sort_by_key(|t| t.tick);
    assert_eq!(snapshot.ticks, sorted);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn full_sync_entries_round_trip_into_a_cache() -> Result<()> {
    let pool = synthetic_uniswap_pool();
    let mut cache = mock_cache().await;
    install(
        &mut cache,
        POOL,
        build_full_sync_program(&pool.spec),
        &pool.seeds,
    );
    let output = run(&mut cache, POOL, Bytes::new())?;
    let snapshot = decode_full_sync(&pool.spec, &output)?;

    // Materialize into a SECOND, empty cache — the cold-start consumer path.
    let mut warmed = mock_cache().await;
    let injected = snapshot.inject(&mut warmed, POOL, &pool.spec);

    // Every seeded slot must read back exactly.
    for (slot, value) in &pool.seeds {
        assert_eq!(
            warmed.cached_storage_value(POOL, *slot),
            Some(*value),
            "slot {slot:#x} must round-trip"
        );
    }
    // Empty bitmap words are explicitly known-zero (no lazy fetch later).
    let empty_word_key =
        v3_tick_bitmap_storage_key_with_base(100, pool.spec.layout.tick_bitmap_base_slot);
    assert_eq!(
        warmed.cached_storage_value(POOL, empty_word_key),
        Some(U256::ZERO)
    );
    // Full spacing-10 coverage: statics + 5 ticks × 4 + 694 words + 3 obs.
    let (min_word, max_word) = full_word_range(10);
    let expected_entries = 5 + 5 * 4 + (max_word as i32 - min_word as i32 + 1) as usize + 3;
    assert_eq!(injected, expected_entries);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn partial_sync_program_scans_only_requested_words() -> Result<()> {
    let pool = synthetic_uniswap_pool();
    let mut cache = mock_cache().await;
    install(
        &mut cache,
        POOL,
        build_partial_sync_program(&pool.spec),
        &pool.seeds,
    );

    // Two populated words (negative edge + word 0) and one empty word.
    let words = [-347i16, 0, 5];
    let output = run(&mut cache, POOL, partial_sync_calldata(&words))?;
    let ticks = decode_partial_sync(&output)?;

    let expected: Vec<V3TickSnapshot> = pool
        .expected
        .ticks
        .iter()
        .filter(|t| [-347i16, 0].contains(&v3_word_position(t.tick, 10)))
        .cloned()
        .collect();
    assert_eq!(ticks, expected);

    // Materialized entries cover the ticks and mark the scanned-but-empty
    // word as known-zero.
    let entries = partial_storage_entries(&ticks, &words, &pool.spec);
    assert_eq!(entries.len(), expected.len() * 4 + words.len());
    let empty_key = v3_tick_bitmap_storage_key_with_base(5, pool.spec.layout.tick_bitmap_base_slot);
    assert!(entries.contains(&(empty_key, U256::ZERO)));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn full_sync_of_an_empty_pool_is_statics_only() -> Result<()> {
    let layout = V3StorageLayout::uniswap(10);
    let spec = V3SyncSpec::uniswap(layout);
    // Cardinality 0, no bitmap bits anywhere, statics zero except slot0.
    let slot0 = U256::from(42u64);
    let mut cache = mock_cache().await;
    install(
        &mut cache,
        POOL,
        build_full_sync_program(&spec),
        &[(layout.slot0_slot, slot0)],
    );

    let output = run(&mut cache, POOL, Bytes::new())?;
    assert_eq!(output.len(), (5 + 1) * 32, "statics + zero tick count");
    let snapshot = decode_full_sync(&spec, &output)?;
    assert!(snapshot.ticks.is_empty());
    assert!(snapshot.observations.is_empty());
    assert_eq!(snapshot.statics[0], (layout.slot0_slot, slot0));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn pancake_layout_core_spec_uses_its_own_slot_bases() -> Result<()> {
    // Pancake V3: slot0 @ 0, liquidity @ 5, ticks base @ 6, bitmap base @ 7.
    let layout = V3StorageLayout::pancake(60);
    let spec = V3SyncSpec::core(layout);

    let tick = 120i32; // compressed 2 → word 0, bit 2
    let info: [U256; 4] = std::array::from_fn(|k| U256::from(5_000 + k as u64));
    let mut seeds = vec![
        (layout.slot0_slot, U256::from(9u64)),
        (layout.liquidity_slot, U256::from(4_242u64)),
        (
            v3_tick_bitmap_storage_key_with_base(0, layout.tick_bitmap_base_slot),
            U256::from(1u64) << 2,
        ),
    ];
    let keys = v3_tick_info_storage_keys_with_base(tick, layout.ticks_base_slot);
    seeds.extend(keys.into_iter().zip(info));

    let mut cache = mock_cache().await;
    install(&mut cache, POOL, build_full_sync_program(&spec), &seeds);

    let output = run(&mut cache, POOL, Bytes::new())?;
    let snapshot = decode_full_sync(&spec, &output)?;
    assert_eq!(
        snapshot.statics,
        vec![
            (layout.slot0_slot, U256::from(9u64)),
            (layout.liquidity_slot, U256::from(4_242u64)),
        ],
    );
    assert_eq!(snapshot.ticks, vec![V3TickSnapshot::new(tick, info)]);
    assert!(snapshot.observations.is_empty(), "core spec has no ring");
    Ok(())
}
