//! Provider-free differential corpus for canonical Uniswap V3 swaps.
//!
//! Each case executes the embedded deployed Uniswap V3 pool runtime in revm,
//! feeds that bytecode-emitted `Swap` log plus the exact same parent storage to
//! the public context-aware adapter path, and compares the complete declared
//! canonical swap surface. No RPC or formula-only reference implementation is
//! involved in the expected poststate.

use std::{
    collections::{BTreeMap, BTreeSet},
    str::FromStr,
    sync::Arc,
};

use alloy_primitives::{Address, B256, Bytes, Log, U256, address, b256, hex, keccak256};
use alloy_provider::{RootProvider, network::AnyNetwork};
use alloy_rpc_client::RpcClient;
use alloy_transport::mock::Asserter;
use anyhow::{Result, anyhow};
use evm_amm_state::adapters::storage::{
    V3StorageLayout, v3_tick_bitmap_storage_key_with_base, v3_tick_info_storage_keys_with_base,
};
use evm_amm_state::adapters::{
    AdapterEventContext, AmmAdapter, ConcentratedLiquidityAdapter, PoolKey, PoolRegistration,
    ProtocolMetadata, PurgeScope, StateUpdate, StateView, UpdateQuality, V3ImmutablePatchValues,
    V3Metadata, uniswap_v3_code_seed, uniswap_v3_max_liquidity_per_tick,
};
use evm_fork_cache::cache::EvmCache;
use revm::{
    context::result::ExecutionResult,
    state::{AccountInfo, Bytecode},
};

const CALLER: Address = address!("00000000000000000000000000000000000000c1");
const POOL: Address = address!("00000000000000000000000000000000000000c2");
const FACTORY: Address = address!("00000000000000000000000000000000000000c3");
const TOKEN0: Address = address!("00000000000000000000000000000000000000c4");
const TOKEN1: Address = address!("00000000000000000000000000000000000000c5");
const HARNESS: Address = address!("00000000000000000000000000000000000000c6");
const BLOCK_HASH: B256 = b256!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
const PARENT_HASH: B256 = b256!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
const Q96: U256 = U256::from_limbs([0, 1 << 32, 0, 0]);
const DEFAULT_LIQUIDITY: U256 = U256::from_limbs([1_000_000_000_000_000_000, 0, 0, 0]);
const MIN_SQRT_LIMIT: U256 = U256::from_limbs([4_295_128_740, 0, 0, 0]);

#[derive(Clone)]
struct ParentState(BTreeMap<(Address, U256), U256>);

impl ParentState {
    fn apply(&mut self, updates: &[StateUpdate]) {
        for update in updates {
            match update {
                StateUpdate::Slot {
                    address,
                    slot,
                    value,
                } => {
                    self.0.insert((*address, *slot), *value);
                }
                other => panic!("exact transition emitted non-atomic update {other:?}"),
            }
        }
    }
}

impl StateView for ParentState {
    fn storage(&self, address: Address, slot: U256) -> Option<U256> {
        self.0.get(&(address, slot)).copied()
    }
}

#[derive(Clone, Copy, Debug)]
struct TickSpec {
    tick: i32,
    liquidity_gross: U256,
    liquidity_net: i128,
}

#[derive(Clone, Debug)]
struct Case {
    name: &'static str,
    parent_sqrt: U256,
    parent_tick: i32,
    zero_for_one: bool,
    amount_specified: i128,
    sqrt_price_limit: U256,
    fee: u32,
    tick_spacing: i32,
    liquidity: U256,
    fee_protocol: u8,
    initialized_ticks: Vec<TickSpec>,
    oracle: OracleCase,
}

#[derive(Clone, Copy, Debug)]
struct OracleCase {
    index: u16,
    cardinality: u16,
    cardinality_next: u16,
    parent_timestamp: u32,
    block_timestamp: u64,
}

const DEFAULT_ORACLE: OracleCase = OracleCase {
    index: 0,
    cardinality: 1,
    cardinality_next: 1,
    parent_timestamp: 100,
    block_timestamp: 110,
};

async fn cache() -> EvmCache {
    let asserter = Asserter::new();
    let client = RpcClient::mocked(asserter);
    let provider = RootProvider::<AnyNetwork>::new(client);
    EvmCache::new(Arc::new(provider)).await
}

fn install(cache: &mut EvmCache, address: Address, code: Bytes, slots: &[(U256, U256)]) {
    let code = Bytecode::new_raw(code);
    let code_hash = code.hash_slow();
    cache.db_mut().insert_account_info(
        address,
        AccountInfo {
            balance: U256::ZERO,
            nonce: 1,
            code: Some(code),
            code_hash,
            account_id: None,
        },
    );
    cache
        .db_mut()
        .replace_account_storage(address, Default::default())
        .expect("mark reference account storage local");
    for (slot, value) in slots {
        cache
            .db_mut()
            .insert_account_storage(address, *slot, *value)
            .expect("seed reference account slot");
    }
}

fn runtime(hex_runtime: &str) -> Bytes {
    Bytes::from(hex::decode(hex_runtime.trim()).expect("checked-in runtime hex"))
}

fn address_word(address: Address) -> U256 {
    U256::from_be_slice(address.as_slice())
}

fn mapping_slot(address: Address, slot: U256) -> U256 {
    let mut encoded = [0_u8; 64];
    encoded[12..32].copy_from_slice(address.as_slice());
    encoded[32..].copy_from_slice(&slot.to_be_bytes::<32>());
    U256::from_be_slice(keccak256(encoded).as_slice())
}

fn signed_word(value: i128) -> U256 {
    if value >= 0 {
        U256::from(value as u128)
    } else {
        (!U256::from(value.unsigned_abs())).wrapping_add(U256::from(1))
    }
}

fn execute_calldata(case: &Case) -> Bytes {
    let mut data = Vec::with_capacity(4 + 32 * 4);
    data.extend_from_slice(&keccak256("execute(address,bool,int256,uint160)")[..4]);
    data.extend_from_slice(&address_word(POOL).to_be_bytes::<32>());
    data.extend_from_slice(&U256::from(case.zero_for_one as u8).to_be_bytes::<32>());
    data.extend_from_slice(&signed_word(case.amount_specified).to_be_bytes::<32>());
    data.extend_from_slice(&case.sqrt_price_limit.to_be_bytes::<32>());
    Bytes::from(data)
}

fn slot0(case: &Case) -> U256 {
    case.parent_sqrt
        | ((signed_word(i128::from(case.parent_tick)) & U256::from(0xffffff)) << 160_usize)
        | (U256::from(case.oracle.index) << 184_usize)
        | (U256::from(case.oracle.cardinality) << 200_usize)
        | (U256::from(case.oracle.cardinality_next) << 216_usize)
        | (U256::from(case.fee_protocol) << 232_usize)
        | (U256::from(1) << 240_usize)
}

fn observation(timestamp: u32) -> U256 {
    U256::from(timestamp) | (U256::from(1) << 248_usize)
}

fn initialized_tick(tick: TickSpec) -> [U256; 4] {
    [
        tick.liquidity_gross | (signed_word(tick.liquidity_net) << 128_usize),
        U256::from(17),
        U256::from(19),
        U256::from(1) << 248_usize,
    ]
}

fn positive_tick(tick: i32, liquidity: U256) -> TickSpec {
    let net = (liquidity / U256::from(5)).to::<u128>();
    TickSpec {
        tick,
        liquidity_gross: U256::from(net),
        liquidity_net: i128::try_from(net).expect("test liquidityNet fits int128"),
    }
}

fn parent_slots(case: &Case, layout: V3StorageLayout) -> (Vec<(U256, U256)>, Vec<U256>) {
    let mut slots = vec![
        (layout.slot0_slot, slot0(case)),
        (U256::from(1), U256::from(101)),
        (U256::from(2), U256::from(202)),
        (U256::from(3), U256::from(303)),
        (layout.liquidity_slot, case.liquidity),
    ];
    let parent_word = case
        .parent_tick
        .div_euclid(case.tick_spacing)
        .div_euclid(256) as i16;
    for word in (parent_word - 4)..=(parent_word + 4) {
        slots.push((
            v3_tick_bitmap_storage_key_with_base(word, layout.tick_bitmap_base_slot),
            U256::ZERO,
        ));
    }

    let mut declared = vec![
        layout.slot0_slot,
        U256::from(1),
        U256::from(2),
        U256::from(3),
        layout.liquidity_slot,
    ];
    for index in 0..case.oracle.cardinality_next {
        let value = if index < case.oracle.cardinality {
            let timestamp = if index == case.oracle.index {
                case.oracle.parent_timestamp
            } else {
                case.oracle
                    .parent_timestamp
                    .saturating_sub(u32::from(case.oracle.cardinality - index) * 10)
            };
            observation(timestamp)
        } else {
            U256::ZERO
        };
        let slot = U256::from(8) + U256::from(index);
        slots.push((slot, value));
        declared.push(slot);
    }

    for tick in &case.initialized_ticks {
        let compressed = tick.tick.div_euclid(case.tick_spacing);
        let word_position = compressed.div_euclid(256) as i16;
        let bit = compressed.rem_euclid(256) as usize;
        let word_slot =
            v3_tick_bitmap_storage_key_with_base(word_position, layout.tick_bitmap_base_slot);
        let bitmap = slots
            .iter_mut()
            .find(|(slot, _)| *slot == word_slot)
            .expect("bitmap seed");
        bitmap.1 |= U256::from(1) << bit;
        let keys = v3_tick_info_storage_keys_with_base(tick.tick, layout.ticks_base_slot);
        slots.extend(keys.into_iter().zip(initialized_tick(*tick)));
        declared.extend(keys);
    }
    (slots, declared)
}

async fn run_case(case: &Case) -> Result<()> {
    run_checked_case(case, false).await
}

async fn run_checked_case(case: &Case, check_invalid: bool) -> Result<()> {
    let layout = V3StorageLayout::uniswap(case.tick_spacing);
    let (pool_slots, declared_slots) = parent_slots(case, layout);
    let parent = ParentState(
        pool_slots
            .iter()
            .map(|(slot, value)| ((POOL, *slot), *value))
            .collect(),
    );
    let mut reference = cache().await;
    reference.set_timestamp(Some(case.oracle.block_timestamp));
    reference.set_block_context(Some(100), Some(0));
    reference
        .db_mut()
        .insert_account_info(CALLER, AccountInfo::default());
    reference
        .db_mut()
        .insert_account_info(Address::ZERO, AccountInfo::default());

    let mut immutables = V3ImmutablePatchValues::default()
        .with_pool_address(POOL)
        .with_factory(FACTORY)
        .with_token0(TOKEN0)
        .with_token1(TOKEN1)
        .with_fee(case.fee)
        .with_tick_spacing(case.tick_spacing);
    immutables.max_liquidity_per_tick = uniswap_v3_max_liquidity_per_tick(case.tick_spacing);
    let pool_code = uniswap_v3_code_seed(POOL, &immutables)?.runtime_bytecode;
    install(&mut reference, POOL, pool_code, &pool_slots);

    let token_code = runtime(include_str!("fixtures/reference_swap_token_runtime.hex"));
    let pool_balance = U256::from(1) << 200_usize;
    let token_slots = [(mapping_slot(POOL, U256::ZERO), pool_balance)];
    install(&mut reference, TOKEN0, token_code.clone(), &token_slots);
    install(&mut reference, TOKEN1, token_code, &token_slots);
    install(
        &mut reference,
        HARNESS,
        runtime(include_str!(
            "fixtures/v3_reference_swap_harness_runtime.hex"
        )),
        &[
            (U256::ZERO, address_word(TOKEN0)),
            (U256::from(1), address_word(TOKEN1)),
        ],
    );

    let calldata = execute_calldata(case);
    let (_, access) = reference.call_raw_with_access_list(CALLER, HARNESS, calldata.clone())?;
    let result = reference.call_raw(CALLER, HARNESS, calldata, true)?;
    let logs = match result {
        ExecutionResult::Success { logs, .. } => logs,
        other => return Err(anyhow!("reference case {} failed: {other:?}", case.name)),
    };
    let swap_topic = keccak256("Swap(address,address,int256,int256,uint160,uint128,int24)");
    let reference_log: Log = logs
        .into_iter()
        .find(|log| log.address == POOL && log.topics().first() == Some(&swap_topic))
        .ok_or_else(|| anyhow!("reference case {} emitted no Swap log", case.name))?;
    let registration = PoolRegistration::new(PoolKey::UniswapV3(POOL))
        .with_state_address(POOL)
        .with_metadata(ProtocolMetadata::UniswapV3(
            V3Metadata::default()
                .with_fee(case.fee)
                .with_tick_spacing(case.tick_spacing)
                .with_storage_layout(layout),
        ));
    let context = AdapterEventContext::for_block(100, BLOCK_HASH, case.oracle.block_timestamp)
        .with_chain_id(1)
        .with_parent_hash(PARENT_HASH)
        .with_transaction_hash(B256::repeat_byte(0xc1))
        .with_event_order(1, 2);
    let decoded = ConcentratedLiquidityAdapter::default().decode_event_with_context(
        &registration,
        &reference_log,
        &parent,
        &context,
    );
    assert_eq!(decoded.error, None, "reference case {}", case.name);
    let event = decoded.event.expect("reference Swap must decode");
    assert_eq!(event.quality, UpdateQuality::Exact, "case {}", case.name);
    if check_invalid {
        assert_invalid_rounding_mutations(case, &reference_log, &registration, &parent, &context);
    }
    let initial = parent.clone();
    let mut derived = parent;
    derived.apply(&event.updates);

    for tick in &case.initialized_ticks {
        let keys = v3_tick_info_storage_keys_with_base(tick.tick, layout.ticks_base_slot);
        assert!(
            keys[1..].iter().any(|slot| {
                reference.cached_storage_value(POOL, *slot) != initial.storage(POOL, *slot)
            }),
            "reference case {} did not cross configured tick {}",
            case.name,
            tick.tick
        );
    }

    let all_reference_pool_slots: BTreeSet<U256> = declared_slots
        .into_iter()
        .chain(
            access
                .slots
                .into_iter()
                .filter_map(|(address, slot)| (address == POOL).then_some(slot)),
        )
        .collect();
    let all_reference_pool_slots = all_reference_pool_slots
        .into_iter()
        .chain(event.updates.iter().filter_map(|update| match update {
            StateUpdate::Slot { address, slot, .. } if *address == POOL => Some(*slot),
            _ => None,
        }))
        .collect::<BTreeSet<_>>();
    for slot in all_reference_pool_slots {
        let expected = reference
            .cached_storage_value(POOL, slot)
            .unwrap_or_default();
        assert_eq!(
            derived.storage(POOL, slot).unwrap_or_default(),
            expected,
            "deployed-bytecode differential mismatch in case {} at slot {slot}",
            case.name
        );
        if initial.storage(POOL, slot).unwrap_or_default() != expected {
            assert!(
                event.updates.iter().any(|update| {
                    matches!(
                        update,
                        StateUpdate::Slot {
                            address,
                            slot: updated,
                            value,
                        } if *address == POOL && *updated == slot && *value == expected
                    )
                }),
                "case {} omitted reference-changed slot {slot}",
                case.name
            );
        }
    }

    if case.name == "exact-output zero-for-one initialized crossing" {
        let tick_keys = v3_tick_info_storage_keys_with_base(
            case.initialized_ticks[0].tick,
            layout.ticks_base_slot,
        );
        let tick_outside_slot = tick_keys[1..]
            .iter()
            .copied()
            .find(|slot| {
                initial.storage(POOL, *slot).unwrap_or_default()
                    != reference
                        .cached_storage_value(POOL, *slot)
                        .unwrap_or_default()
            })
            .expect("reference crossing changes a tick-outside accumulator");
        for (surface, omitted_slot) in [
            ("fee growth", U256::from(1)),
            ("protocol fees", U256::from(3)),
            ("observation", U256::from(8)),
            ("tick outside", tick_outside_slot),
        ] {
            let mutated_updates: Vec<_> = event
                .updates
                .iter()
                .filter(|update| {
                    !matches!(
                        update,
                        StateUpdate::Slot { address, slot, .. }
                            if *address == POOL && *slot == omitted_slot
                    )
                })
                .cloned()
                .collect();
            assert_eq!(
                mutated_updates.len() + 1,
                event.updates.len(),
                "fixture must contain exactly one {surface} update to mutate",
            );
            let mut mutated = initial.clone();
            mutated.apply(&mutated_updates);
            let expected = reference
                .cached_storage_value(POOL, omitted_slot)
                .unwrap_or_default();
            assert_ne!(
                mutated.storage(POOL, omitted_slot).unwrap_or_default(),
                expected,
                "the full-slot completeness oracle must reject omission of {surface}",
            );
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug)]
struct SwapAction {
    zero_for_one: bool,
    exact_input: bool,
    amount: i128,
    block_timestamp: u64,
}

#[derive(Clone, Copy, Debug)]
struct DeterministicRng(u64);

impl DeterministicRng {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0
    }
}

fn generated_tick_distribution(
    tick_spacing: i32,
    liquidity: U256,
    rng: &mut DeterministicRng,
) -> Vec<TickSpec> {
    let unit = (liquidity / U256::from(20)).to::<u128>();
    let unit = i128::try_from(unit).expect("generated liquidityNet fits int128");
    let outer_negative = -(2 + (rng.next() % 2) as i32) * tick_spacing;
    let outer_positive = (2 + (rng.next() % 2) as i32) * tick_spacing;
    [
        (outer_negative, unit),
        (-tick_spacing, -(unit / 2)),
        (tick_spacing, unit / 2),
        (outer_positive, -unit),
    ]
    .into_iter()
    .map(|(tick, liquidity_net)| {
        let extra = U256::from(1 + rng.next() % 3);
        TickSpec {
            tick,
            liquidity_gross: U256::from(liquidity_net.unsigned_abs())
                + (liquidity / U256::from(100)) * extra,
            liquidity_net,
        }
    })
    .collect()
}

async fn run_ordered_sequence(initial: &Case, actions: &[SwapAction]) -> Result<()> {
    let layout = V3StorageLayout::uniswap(initial.tick_spacing);
    let (pool_slots, declared_slots) = parent_slots(initial, layout);
    let mut parent = ParentState(
        pool_slots
            .iter()
            .map(|(slot, value)| ((POOL, *slot), *value))
            .collect(),
    );
    let mut reference = cache().await;
    reference
        .db_mut()
        .insert_account_info(CALLER, AccountInfo::default());
    reference
        .db_mut()
        .insert_account_info(Address::ZERO, AccountInfo::default());

    let mut immutables = V3ImmutablePatchValues::default()
        .with_pool_address(POOL)
        .with_factory(FACTORY)
        .with_token0(TOKEN0)
        .with_token1(TOKEN1)
        .with_fee(initial.fee)
        .with_tick_spacing(initial.tick_spacing);
    immutables.max_liquidity_per_tick = uniswap_v3_max_liquidity_per_tick(initial.tick_spacing);
    let pool_code = uniswap_v3_code_seed(POOL, &immutables)?.runtime_bytecode;
    install(&mut reference, POOL, pool_code, &pool_slots);
    let token_code = runtime(include_str!("fixtures/reference_swap_token_runtime.hex"));
    let token_slots = [(mapping_slot(POOL, U256::ZERO), U256::from(1) << 200_usize)];
    install(&mut reference, TOKEN0, token_code.clone(), &token_slots);
    install(&mut reference, TOKEN1, token_code, &token_slots);
    install(
        &mut reference,
        HARNESS,
        runtime(include_str!(
            "fixtures/v3_reference_swap_harness_runtime.hex"
        )),
        &[
            (U256::ZERO, address_word(TOKEN0)),
            (U256::from(1), address_word(TOKEN1)),
        ],
    );

    let registration = PoolRegistration::new(PoolKey::UniswapV3(POOL))
        .with_state_address(POOL)
        .with_metadata(ProtocolMetadata::UniswapV3(
            V3Metadata::default()
                .with_fee(initial.fee)
                .with_tick_spacing(initial.tick_spacing)
                .with_storage_layout(layout),
        ));
    let swap_topic = keccak256("Swap(address,address,int256,int256,uint160,uint128,int24)");
    let mut crossed = vec![false; initial.initialized_ticks.len()];

    for (step, action) in actions.iter().enumerate() {
        reference.set_timestamp(Some(action.block_timestamp));
        reference.set_block_context(Some(100 + step as u64), Some(0));
        let mut request = initial.clone();
        request.zero_for_one = action.zero_for_one;
        request.amount_specified = if action.exact_input {
            action.amount
        } else {
            -action.amount
        };
        request.sqrt_price_limit = if action.zero_for_one {
            MIN_SQRT_LIMIT
        } else {
            U256::from_str("1461446703485210103287273052203988822378723970341")?
        };

        let before = parent.clone();
        let calldata = execute_calldata(&request);
        let (_, access) = reference.call_raw_with_access_list(CALLER, HARNESS, calldata.clone())?;
        let result = reference.call_raw(CALLER, HARNESS, calldata, true)?;
        let logs = match result {
            ExecutionResult::Success { logs, .. } => logs,
            other => {
                return Err(anyhow!(
                    "reference sequence {} step {step} failed: {other:?}",
                    initial.name
                ));
            }
        };
        let reference_log: Log = logs
            .into_iter()
            .find(|log| log.address == POOL && log.topics().first() == Some(&swap_topic))
            .ok_or_else(|| {
                anyhow!(
                    "reference sequence {} step {step} emitted no Swap log",
                    initial.name
                )
            })?;
        let context =
            AdapterEventContext::for_block(100 + step as u64, BLOCK_HASH, action.block_timestamp)
                .with_chain_id(1)
                .with_parent_hash(PARENT_HASH)
                .with_transaction_hash(B256::repeat_byte(0xd0 + step as u8))
                .with_event_order(step as u64 + 1, step as u64 + 2);
        let decoded = ConcentratedLiquidityAdapter::default().decode_event_with_context(
            &registration,
            &reference_log,
            &parent,
            &context,
        );
        assert_eq!(
            decoded.error, None,
            "reference sequence {} step {step}",
            initial.name
        );
        let event = decoded.event.expect("reference sequence Swap must decode");
        assert_eq!(event.quality, UpdateQuality::Exact);
        parent.apply(&event.updates);

        for (tick_index, tick) in initial.initialized_ticks.iter().enumerate() {
            let keys = v3_tick_info_storage_keys_with_base(tick.tick, layout.ticks_base_slot);
            crossed[tick_index] |= keys[1..].iter().any(|slot| {
                reference.cached_storage_value(POOL, *slot) != before.storage(POOL, *slot)
            });
        }
        let all_reference_pool_slots: BTreeSet<U256> = declared_slots
            .iter()
            .copied()
            .chain(
                access
                    .slots
                    .into_iter()
                    .filter_map(|(address, slot)| (address == POOL).then_some(slot)),
            )
            .collect();
        let all_reference_pool_slots = all_reference_pool_slots
            .into_iter()
            .chain(event.updates.iter().filter_map(|update| match update {
                StateUpdate::Slot { address, slot, .. } if *address == POOL => Some(*slot),
                _ => None,
            }))
            .collect::<BTreeSet<_>>();
        for slot in all_reference_pool_slots {
            let expected = reference
                .cached_storage_value(POOL, slot)
                .unwrap_or_default();
            assert_eq!(
                parent.storage(POOL, slot).unwrap_or_default(),
                expected,
                "sequence {} step {step} mismatch at slot {slot}",
                initial.name
            );
            if before.storage(POOL, slot).unwrap_or_default() != expected {
                assert!(
                    event.updates.iter().any(|update| {
                        matches!(
                            update,
                            StateUpdate::Slot {
                                address,
                                slot: updated,
                                value,
                            } if *address == POOL && *updated == slot && *value == expected
                        )
                    }),
                    "sequence {} step {step} omitted reference-changed slot {slot}",
                    initial.name
                );
            }
        }
    }
    for (was_crossed, tick) in crossed.into_iter().zip(&initial.initialized_ticks) {
        assert!(
            was_crossed,
            "reference sequence {} never crossed generated tick {}",
            initial.name, tick.tick
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn canonical_uniswap_v3_generated_swap_corpus_matches_deployed_bytecode() -> Result<()> {
    let sqrt_at_minus_10 = U256::from_str("79188560314459151373725315960")?;
    let sqrt_at_10 = U256::from_str("79267784519130042428790663799")?;
    let max_sqrt_limit = U256::from_str("1461446703485210103287273052203988822378723970341")?;
    const GROW_ORACLE: OracleCase = OracleCase {
        index: 0,
        cardinality: 1,
        cardinality_next: 3,
        parent_timestamp: 100,
        block_timestamp: 110,
    };
    const WRAP_ORACLE: OracleCase = OracleCase {
        index: 2,
        cardinality: 3,
        cardinality_next: 3,
        parent_timestamp: 100,
        block_timestamp: 110,
    };
    const SAME_TIMESTAMP_ORACLE: OracleCase = OracleCase {
        parent_timestamp: 110,
        block_timestamp: 110,
        ..DEFAULT_ORACLE
    };
    let cases = [
        Case {
            parent_sqrt: Q96,
            parent_tick: 0,
            name: "exact-input zero-for-one",
            zero_for_one: true,
            amount_specified: 1_000_000_000_000_000,
            sqrt_price_limit: MIN_SQRT_LIMIT,
            fee: 3_000,
            tick_spacing: 1,
            liquidity: DEFAULT_LIQUIDITY,
            fee_protocol: 0,
            initialized_ticks: vec![],
            oracle: GROW_ORACLE,
        },
        Case {
            parent_sqrt: Q96,
            parent_tick: 0,
            name: "exact-input one-for-zero with protocol fee",
            zero_for_one: false,
            amount_specified: 1_000_000_000_000_000,
            sqrt_price_limit: max_sqrt_limit,
            fee: 3_000,
            tick_spacing: 1,
            liquidity: DEFAULT_LIQUIDITY,
            fee_protocol: 0x40,
            initialized_ticks: vec![],
            oracle: WRAP_ORACLE,
        },
        Case {
            parent_sqrt: Q96,
            parent_tick: 0,
            name: "exact-output zero-for-one initialized crossing",
            zero_for_one: true,
            amount_specified: -2_000_000_000_000_000,
            sqrt_price_limit: MIN_SQRT_LIMIT,
            fee: 3_000,
            tick_spacing: 1,
            liquidity: DEFAULT_LIQUIDITY,
            fee_protocol: 0x04,
            initialized_ticks: vec![
                positive_tick(-10, DEFAULT_LIQUIDITY),
                positive_tick(-20, DEFAULT_LIQUIDITY),
            ],
            oracle: DEFAULT_ORACLE,
        },
        Case {
            parent_sqrt: Q96,
            parent_tick: 0,
            name: "exact-output one-for-zero initialized crossing",
            zero_for_one: false,
            amount_specified: -2_000_000_000_000_000,
            sqrt_price_limit: max_sqrt_limit,
            fee: 3_000,
            tick_spacing: 1,
            liquidity: DEFAULT_LIQUIDITY,
            fee_protocol: 0x40,
            initialized_ticks: vec![
                positive_tick(10, DEFAULT_LIQUIDITY),
                positive_tick(20, DEFAULT_LIQUIDITY),
            ],
            oracle: DEFAULT_ORACLE,
        },
        Case {
            parent_sqrt: Q96,
            parent_tick: 0,
            name: "zero-for-one explicit price limit",
            zero_for_one: true,
            amount_specified: 100_000_000_000_000_000,
            sqrt_price_limit: sqrt_at_minus_10,
            fee: 3_000,
            tick_spacing: 1,
            liquidity: DEFAULT_LIQUIDITY,
            fee_protocol: 0,
            initialized_ticks: vec![],
            oracle: DEFAULT_ORACLE,
        },
        Case {
            parent_sqrt: Q96,
            parent_tick: 0,
            name: "one-for-zero explicit price limit",
            zero_for_one: false,
            amount_specified: 100_000_000_000_000_000,
            sqrt_price_limit: sqrt_at_10,
            fee: 3_000,
            tick_spacing: 1,
            liquidity: DEFAULT_LIQUIDITY,
            fee_protocol: 0,
            initialized_ticks: vec![],
            oracle: DEFAULT_ORACLE,
        },
        Case {
            parent_sqrt: Q96,
            parent_tick: 0,
            name: "tiny exact-input all fee",
            zero_for_one: true,
            amount_specified: 1,
            sqrt_price_limit: MIN_SQRT_LIMIT,
            fee: 3_000,
            tick_spacing: 1,
            liquidity: DEFAULT_LIQUIDITY,
            fee_protocol: 0,
            initialized_ticks: vec![],
            oracle: DEFAULT_ORACLE,
        },
        Case {
            parent_sqrt: Q96,
            parent_tick: 0,
            name: "tiny exact-output zero-for-one rounding",
            zero_for_one: true,
            amount_specified: -1,
            sqrt_price_limit: MIN_SQRT_LIMIT,
            fee: 3_000,
            tick_spacing: 1,
            liquidity: DEFAULT_LIQUIDITY,
            fee_protocol: 0x04,
            initialized_ticks: vec![],
            oracle: DEFAULT_ORACLE,
        },
        Case {
            parent_sqrt: Q96,
            parent_tick: 0,
            name: "tiny exact-output one-for-zero rounding at same timestamp",
            zero_for_one: false,
            amount_specified: -1,
            sqrt_price_limit: max_sqrt_limit,
            fee: 3_000,
            tick_spacing: 1,
            liquidity: DEFAULT_LIQUIDITY,
            fee_protocol: 0x40,
            initialized_ticks: vec![],
            oracle: SAME_TIMESTAMP_ORACLE,
        },
    ];
    for case in cases {
        run_case(&case).await?;
    }

    const FEES: [u32; 4] = [100, 500, 3_000, 10_000];
    const LIQUIDITIES: [U256; 3] = [
        U256::from_limbs([1_000_000_000_000, 0, 0, 0]),
        DEFAULT_LIQUIDITY,
        U256::from_limbs([4_007_528_410_413_793_280, 108_420, 0, 0]),
    ];
    for fee in FEES {
        for liquidity in LIQUIDITIES {
            let base_amount = (liquidity / U256::from(1_000_u16)).to::<u128>().max(1_000);
            let base_amount = i128::try_from(base_amount)?;
            for zero_for_one in [true, false] {
                let sqrt_price_limit = if zero_for_one {
                    MIN_SQRT_LIMIT
                } else {
                    max_sqrt_limit
                };
                for amount_specified in [base_amount, -(base_amount / 2)] {
                    run_case(&Case {
                        parent_sqrt: Q96,
                        parent_tick: 0,
                        name: "generated fee/liquidity/direction/exactness matrix",
                        zero_for_one,
                        amount_specified,
                        sqrt_price_limit,
                        fee,
                        tick_spacing: 1,
                        liquidity,
                        fee_protocol: if zero_for_one { 0x04 } else { 0x40 },
                        initialized_ticks: vec![],
                        oracle: DEFAULT_ORACLE,
                    })
                    .await?;
                }
            }
        }
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn generated_initialized_tick_sequences_match_deployed_bytecode_after_every_swap()
-> Result<()> {
    const SEED: u64 = 0x5eed_cafe_d15c_a11e;
    let mut rng = DeterministicRng(SEED);
    let oracle_cases = [
        OracleCase {
            index: 0,
            cardinality: 1,
            cardinality_next: 1,
            parent_timestamp: 100,
            block_timestamp: 110,
        },
        OracleCase {
            index: 0,
            cardinality: 1,
            cardinality_next: 4,
            parent_timestamp: 200,
            block_timestamp: 210,
        },
        OracleCase {
            index: 2,
            cardinality: 3,
            cardinality_next: 3,
            parent_timestamp: 300,
            block_timestamp: 310,
        },
        OracleCase {
            index: 1,
            cardinality: 2,
            cardinality_next: 4,
            parent_timestamp: 400,
            block_timestamp: 410,
        },
    ];
    for (scenario, (fee, tick_spacing, oracle)) in [
        (100_u32, 1_i32, oracle_cases[0]),
        (500, 10, oracle_cases[1]),
        (3_000, 60, oracle_cases[2]),
        (10_000, 200, oracle_cases[3]),
    ]
    .into_iter()
    .enumerate()
    {
        let liquidity = DEFAULT_LIQUIDITY + U256::from((scenario as u64 + 1) * 1_000_003);
        let initialized_ticks = generated_tick_distribution(tick_spacing, liquidity, &mut rng);
        let base_amount = ((liquidity * U256::from(tick_spacing as u32)) / U256::from(5_000))
            .to::<u128>()
            .max(10_000);
        let mut action = |zero_for_one: bool, exact_input: bool, timestamp: u64| {
            let scale = 10 + rng.next() % 4;
            let amount = base_amount * u128::from(scale) / 10;
            SwapAction {
                zero_for_one,
                exact_input,
                amount: i128::try_from(amount).expect("generated swap amount fits int128"),
                block_timestamp: timestamp,
            }
        };
        let first_timestamp = u64::from(oracle.parent_timestamp) + 10;
        let actions = [
            action(false, true, first_timestamp),
            action(true, false, first_timestamp),
            action(true, true, first_timestamp + 10),
            action(false, false, first_timestamp + 10),
        ];
        let initial = Case {
            parent_sqrt: Q96,
            parent_tick: 0,
            name: "fixed-seed ordered initialized-tick property scenario",
            zero_for_one: false,
            amount_specified: actions[0].amount,
            sqrt_price_limit: U256::MAX,
            fee,
            tick_spacing,
            liquidity,
            fee_protocol: if scenario % 2 == 0 { 0x44 } else { 0x65 },
            initialized_ticks,
            oracle,
        };
        run_ordered_sequence(&initial, &actions).await?;
    }
    Ok(())
}

// Geometry and amounts from Plasma block 32451176, tx 0x5548f995bd836926932331ab0403abb835e20119a3f7563b23679280b5239001.
// This executes the canonical runtime with that geometry; the expected storage comes from revm.
#[tokio::test]
async fn plasma_exact_input_rounding_matches_deployed_bytecode() -> Result<()> {
    run_checked_case(
        &Case {
            name: "Plasma exact-input 1037-wei budget slack",
            parent_sqrt: U256::from_str("22663668894434745395613")?,
            parent_tick: -301357,
            zero_for_one: true,
            amount_specified: 86272922443841536,
            sqrt_price_limit: MIN_SQRT_LIMIT,
            fee: 500,
            tick_spacing: 10,
            liquidity: U256::from(12941633377475003401_u64),
            fee_protocol: 0,
            initialized_ticks: vec![],
            oracle: DEFAULT_ORACLE,
        },
        true,
    )
    .await
}

async fn capped_output_case(zero_for_one: bool, initialized: bool) -> Result<()> {
    let liquidity = Q96 * U256::from(2);
    run_checked_case(
        &Case {
            name: "output capped by one wei at rounded price",
            parent_sqrt: if zero_for_one {
                Q96 + U256::from(100)
            } else {
                Q96 - U256::from(100)
            },
            parent_tick: if zero_for_one { 0 } else { -1 },
            zero_for_one,
            amount_specified: if initialized { -199 } else { -197 },
            sqrt_price_limit: if zero_for_one {
                MIN_SQRT_LIMIT
            } else {
                U256::from_str("1461446703485210103287273052203988822378723970341")?
            },
            fee: 500,
            tick_spacing: 1,
            liquidity,
            fee_protocol: 0x44,
            initialized_ticks: if initialized {
                vec![positive_tick(0, liquidity)]
            } else {
                vec![]
            },
            oracle: DEFAULT_ORACLE,
        },
        true,
    )
    .await
}

#[tokio::test]
async fn capped_output_zero_for_one_partial() -> Result<()> {
    capped_output_case(true, false).await
}
#[tokio::test]
async fn capped_output_one_for_zero_partial() -> Result<()> {
    capped_output_case(false, false).await
}
#[tokio::test]
async fn capped_output_zero_for_one_initialized_boundary() -> Result<()> {
    capped_output_case(true, true).await
}
#[tokio::test]
async fn capped_output_one_for_zero_initialized_boundary() -> Result<()> {
    capped_output_case(false, true).await
}

fn assert_invalid_rounding_mutations(
    case: &Case,
    log: &Log,
    registration: &PoolRegistration,
    parent: &ParentState,
    context: &AdapterEventContext,
) {
    let input_index = usize::from(!case.zero_for_one);
    let output_index = 1 - input_index;
    let word = |index| U256::from_be_slice(&log.data.data[index * 32..(index + 1) * 32]);
    let input = word(input_index);
    let output = (!word(output_index)).wrapping_add(U256::from(1));
    // Double the input while retaining the same price/output, or claim output
    // beyond the path, or contradict the endpoint's tick/liquidity/ABI widths.
    // These are constructed contradictions, not arbitrary one-wei mutations.
    let mut mutations = vec![
        (input_index, input * U256::from(2)),
        (
            output_index,
            (!(output * U256::from(2))).wrapping_add(U256::from(1)),
        ),
        (3, word(3) + U256::from(1)),
        (4, word(4).wrapping_add(U256::from(2))),
        (2, U256::from(1) << 160_usize),
    ];
    if case.amount_specified < 0 {
        // The uncapped event can consume this extra wei as exact-input fee;
        // combining that fee with a capped exact-output event is invalid.
        mutations.push((input_index, input + U256::from(1)));
    }
    for (index, replacement) in mutations {
        let mut data = log.data.data.to_vec();
        data[index * 32..(index + 1) * 32].copy_from_slice(&replacement.to_be_bytes::<32>());
        let bad = Log::new_unchecked(log.address, log.topics().to_vec(), Bytes::from(data));
        let decoded = ConcentratedLiquidityAdapter::default().decode_event_with_context(
            registration,
            &bad,
            parent,
            context,
        );
        assert!(
            decoded.error.is_some(),
            "{} accepted invalid word {index}",
            case.name
        );
        let invalidation = decoded
            .event
            .expect("recognized malformed swap invalidates");
        assert!(matches!(
            invalidation.quality,
            UpdateQuality::RequiresRepair | UpdateQuality::ConservativeInvalidation
        ));
        assert_eq!(
            invalidation.updates,
            vec![StateUpdate::purge(POOL, PurgeScope::AllStorage)],
            "rejection must not leak partial exact writes"
        );
    }
}

#[test]
fn historical_plasma_exact_input_matches_trace_and_runtime_identity() -> Result<()> {
    historical_plasma_trace(include_str!("fixtures/plasma_exact_input_rounding.json"))
}

#[test]
fn historical_plasma_exact_output_matches_trace_and_runtime_identity() -> Result<()> {
    historical_plasma_trace(include_str!("fixtures/plasma_exact_output_rounding.json"))
}

fn historical_plasma_trace(fixture: &str) -> Result<()> {
    let fixture: serde_json::Value = serde_json::from_str(fixture)?;
    let pool: Address = fixture["pool"]["address"].as_str().unwrap().parse()?;
    let parse = |value: &serde_json::Value| U256::from_str(value.as_str().unwrap()).unwrap();
    let storage = |value: &serde_json::Value| {
        value
            .as_object()
            .unwrap()
            .iter()
            .map(|(slot, value)| ((pool, U256::from_str(slot).unwrap()), parse(value)))
            .collect::<BTreeMap<_, _>>()
    };
    let parent = ParentState(storage(&fixture["pool_pre"]["storage"]));
    let mut expected = parent.clone();
    for key in fixture["pool_diff"]["pre"]["storage"]
        .as_object()
        .unwrap()
        .keys()
    {
        expected.0.insert((pool, key.parse()?), U256::ZERO);
    }
    expected
        .0
        .extend(storage(&fixture["pool_diff"]["post"]["storage"]));
    let log: alloy_rpc_types_eth::Log = serde_json::from_value(fixture["log"].clone())?;
    let identity = &fixture["reference"];
    let context = AdapterEventContext::for_block(
        identity["block_number"].as_u64().unwrap(),
        identity["block_hash"].as_str().unwrap().parse()?,
        identity["block_timestamp"].as_u64().unwrap(),
    )
    .with_chain_id(9745)
    .with_parent_hash(identity["parent_hash"].as_str().unwrap().parse()?)
    .with_transaction_hash(identity["transaction_hash"].as_str().unwrap().parse()?)
    .with_event_order(
        identity["transaction_index"].as_u64().unwrap(),
        identity["log_index"].as_u64().unwrap(),
    );
    let metadata = V3Metadata::default()
        .with_fee(500)
        .with_tick_spacing(10)
        .with_storage_layout(V3StorageLayout::uniswap(10));
    let registration = PoolRegistration::new(PoolKey::UniswapV3(pool))
        .with_state_address(pool)
        .with_metadata(ProtocolMetadata::UniswapV3(metadata));
    let decoded = ConcentratedLiquidityAdapter::default().decode_event_with_context(
        &registration,
        &log.inner,
        &parent,
        &context,
    );
    assert_eq!(decoded.error, None);
    let event = decoded.event.unwrap();
    assert_eq!(event.quality, UpdateQuality::Exact);
    let mut actual = parent;
    actual.apply(&event.updates);
    assert_eq!(actual.0, expected.0, "complete trace/write-union poststate");
    let mut immutables = V3ImmutablePatchValues::default()
        .with_pool_address(pool)
        .with_factory(fixture["pool"]["factory"].as_str().unwrap().parse()?)
        .with_token0(fixture["pool"]["token0"].as_str().unwrap().parse()?)
        .with_token1(fixture["pool"]["token1"].as_str().unwrap().parse()?)
        .with_fee(500)
        .with_tick_spacing(10);
    immutables.max_liquidity_per_tick = uniswap_v3_max_liquidity_per_tick(10);
    assert_eq!(
        keccak256(uniswap_v3_code_seed(pool, &immutables)?.runtime_bytecode),
        keccak256(Bytes::from_str(
            fixture["pool_pre"]["code"].as_str().unwrap()
        )?),
        "Plasma runtime must match the differential reference after immutable patching"
    );
    Ok(())
}

#[tokio::test]
async fn asymmetric_price_rounding_matrix_and_sequences_match_bytecode() -> Result<()> {
    let low = U256::from_str("22663668894434745395613")?;
    for (parent_sqrt, parent_tick) in [(low, -301357), (Q96 * Q96 / low, 301356)] {
        for liquidity in [
            U256::from(1_000_000_000_000_u64),
            DEFAULT_LIQUIDITY,
            Q96 * U256::from(2),
        ] {
            let token0_amount = (liquidity * Q96 / parent_sqrt / U256::from(100_000))
                .max(U256::from(1))
                .to::<i128>();
            let token1_amount = (liquidity * parent_sqrt / Q96 / U256::from(100_000))
                .max(U256::from(1))
                .to::<i128>();
            for fee in [0, 100, 500, 3000, 10000] {
                let mut actions = Vec::new();
                for zero_for_one in [true, false] {
                    for exact_input in [true, false] {
                        let amount = if zero_for_one == exact_input {
                            token0_amount
                        } else {
                            token1_amount
                        };
                        let case = Case {
                            name: "asymmetric price/fee/liquidity/exactness matrix",
                            parent_sqrt,
                            parent_tick,
                            liquidity,
                            fee,
                            zero_for_one,
                            amount_specified: if exact_input { amount } else { -amount },
                            sqrt_price_limit: if zero_for_one {
                                MIN_SQRT_LIMIT
                            } else {
                                U256::from_str("1461446703485210103287273052203988822378723970341")?
                            },
                            tick_spacing: 10,
                            fee_protocol: 0x44,
                            initialized_ticks: vec![],
                            oracle: DEFAULT_ORACLE,
                        };
                        run_case(&case).await?;
                        actions.push(SwapAction {
                            zero_for_one,
                            exact_input,
                            amount,
                            block_timestamp: 110 + actions.len() as u64,
                        });
                        if actions.len() == 4 {
                            run_ordered_sequence(&case, &actions).await?;
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

#[tokio::test]
async fn rounding_after_initialized_crossing_preserves_prior_step_fees() -> Result<()> {
    let liquidity = Q96 * U256::from(2);
    for zero_for_one in [true, false] {
        for exact_input in [true, false] {
            run_case(&Case {
                name: "rounding after preceding initialized step",
                parent_sqrt: if zero_for_one {
                    Q96 + U256::from(100)
                } else {
                    Q96 - U256::from(100)
                },
                parent_tick: if zero_for_one { 0 } else { -1 },
                zero_for_one,
                amount_specified: match (zero_for_one, exact_input) {
                    (true, true) => 208,
                    (false, true) => 209,
                    (true, false) => -202,
                    (false, false) => -203,
                },
                sqrt_price_limit: if zero_for_one {
                    MIN_SQRT_LIMIT
                } else {
                    U256::from_str("1461446703485210103287273052203988822378723970341")?
                },
                fee: 500,
                tick_spacing: 1,
                liquidity,
                fee_protocol: 0x44,
                initialized_ticks: vec![positive_tick(0, liquidity)],
                oracle: DEFAULT_ORACLE,
            })
            .await?;
        }
    }
    Ok(())
}
