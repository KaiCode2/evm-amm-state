//! Fail-closed acceptance tests for the canonical Uniswap V3 non-swap
//! transitions.
//!
//! The differential corpus proves the happy paths against deployed bytecode.
//! This file proves the opposite: that every way a parent or an event can be
//! untrustworthy is rejected rather than turned into plausible state. Exactness
//! is only worth anything if the boundary around it holds, so each case asserts
//! the specific conservative outcome — whole-storage invalidation for a
//! contradicted parent, a targeted resync for merely-unknown cells, and no
//! promotion at all for a family this release has not proven.

use std::collections::BTreeMap;

use alloy_primitives::{Address, B256, Bytes, Log, LogData, U256, address, b256, keccak256};
use evm_amm_state::adapters::storage::{
    V3StorageLayout, v3_tick_bitmap_storage_key_with_base, v3_tick_info_storage_keys_with_base,
};
use evm_amm_state::adapters::{
    AdapterEventContext, AdapterEventResult, AmmAdapter, ConcentratedLiquidityAdapter, PoolKey,
    PoolRegistration, ProtocolMetadata, PurgeScope, RepairAction, StateUpdate, StateView,
    UpdateQuality, V3LiquidityTransitionCapability, V3Metadata,
};

const POOL: Address = address!("00000000000000000000000000000000000000c2");
const OWNER: Address = address!("00000000000000000000000000000000000000c7");
const BLOCK_HASH: B256 = b256!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
const PARENT_HASH: B256 = b256!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
const Q96: U256 = U256::from_limbs([0, 1 << 32, 0, 0]);
const TICK_SPACING: i32 = 60;
const FEE: u32 = 3_000;
const LIQUIDITY: u128 = 1_000_000_000_000_000_000;

#[derive(Clone, Default)]
struct Parent(BTreeMap<U256, U256>);

impl StateView for Parent {
    fn storage(&self, address: Address, slot: U256) -> Option<U256> {
        if address != POOL {
            return None;
        }
        self.0.get(&slot).copied()
    }
}

fn layout() -> V3StorageLayout {
    V3StorageLayout::uniswap(TICK_SPACING)
}

/// A coherent canonical parent at tick 0: unlocked slot0, both fee
/// accumulators, protocol fees, active liquidity, an initialized observation,
/// and the two bitmap words around the current tick proven empty.
///
/// Boundary `Tick.Info` words are deliberately absent. A clear bitmap bit proves
/// the tick empty, so a mint that opens a fresh position needs no tick read —
/// exactly the case a cold start leaves unwarmed.
fn coherent_parent() -> Parent {
    let layout = layout();
    let mut slots = BTreeMap::new();
    slots.insert(
        layout.slot0_slot,
        Q96 | (U256::from(1) << 200) | (U256::from(1) << 216) | (U256::from(1) << 240),
    );
    slots.insert(U256::from(1), U256::from(101));
    slots.insert(U256::from(2), U256::from(202));
    slots.insert(U256::from(3), U256::from(303));
    slots.insert(layout.liquidity_slot, U256::from(LIQUIDITY));
    // observations[0]: timestamp 100, initialized.
    slots.insert(U256::from(8), U256::from(100) | (U256::from(1) << 248));
    for word in [-1_i16, 0] {
        slots.insert(
            v3_tick_bitmap_storage_key_with_base(word, layout.tick_bitmap_base_slot),
            U256::ZERO,
        );
    }
    Parent(slots)
}

fn registration(protocol: ProtocolMetadata) -> PoolRegistration {
    let key = match &protocol {
        ProtocolMetadata::PancakeV3(_) => PoolKey::PancakeV3(POOL),
        _ => PoolKey::UniswapV3(POOL),
    };
    PoolRegistration::new(key)
        .with_state_address(POOL)
        .with_metadata(protocol)
}

fn canonical_registration() -> PoolRegistration {
    registration(ProtocolMetadata::UniswapV3(
        V3Metadata::default()
            .with_fee(FEE)
            .with_tick_spacing(TICK_SPACING)
            .with_storage_layout(layout()),
    ))
}

fn context() -> AdapterEventContext {
    AdapterEventContext::for_block(100, BLOCK_HASH, 110)
        .with_chain_id(1)
        .with_parent_hash(PARENT_HASH)
        .with_transaction_hash(B256::repeat_byte(0xc1))
        .with_event_order(1, 2)
}

fn word(value: U256) -> [u8; 32] {
    value.to_be_bytes()
}

fn tick_topic(tick: i32) -> B256 {
    let mut bytes = [0_u8; 32];
    if tick < 0 {
        bytes = [0xff; 32];
    }
    bytes[29..].copy_from_slice(&tick.to_be_bytes()[1..]);
    B256::from(bytes)
}

fn address_topic(value: Address) -> B256 {
    let mut bytes = [0_u8; 32];
    bytes[12..].copy_from_slice(value.as_slice());
    B256::from(bytes)
}

fn log(topics: Vec<B256>, data: Vec<U256>) -> Log {
    let mut bytes = Vec::with_capacity(data.len() * 32);
    for value in data {
        bytes.extend_from_slice(&word(value));
    }
    Log {
        address: POOL,
        data: LogData::new_unchecked(topics, Bytes::from(bytes)),
    }
}

fn mint_log(tick_lower: i32, tick_upper: i32, amount: u128) -> Log {
    log(
        vec![
            keccak256("Mint(address,address,int24,int24,uint128,uint256,uint256)"),
            address_topic(OWNER),
            tick_topic(tick_lower),
            tick_topic(tick_upper),
        ],
        vec![
            U256::from_be_slice(address_topic(OWNER).as_slice()),
            U256::from(amount),
            U256::from(1_000),
            U256::from(1_000),
        ],
    )
}

fn burn_log(tick_lower: i32, tick_upper: i32, amount: u128) -> Log {
    log(
        vec![
            keccak256("Burn(address,int24,int24,uint128,uint256,uint256)"),
            address_topic(OWNER),
            tick_topic(tick_lower),
            tick_topic(tick_upper),
        ],
        vec![U256::from(amount), U256::from(1_000), U256::from(1_000)],
    )
}

fn collect_log(tick_lower: i32, tick_upper: i32) -> Log {
    log(
        vec![
            keccak256("Collect(address,address,int24,int24,uint128,uint128)"),
            address_topic(OWNER),
            tick_topic(tick_lower),
            tick_topic(tick_upper),
        ],
        vec![
            U256::from_be_slice(address_topic(OWNER).as_slice()),
            U256::from(500),
            U256::from(700),
        ],
    )
}

fn flash_log(amount0: u128, amount1: u128, paid0: u128, paid1: u128) -> Log {
    log(
        vec![
            keccak256("Flash(address,address,uint256,uint256,uint256,uint256)"),
            address_topic(OWNER),
            address_topic(OWNER),
        ],
        vec![
            U256::from(amount0),
            U256::from(amount1),
            U256::from(paid0),
            U256::from(paid1),
        ],
    )
}

fn set_fee_protocol_log(old0: u8, old1: u8, new0: u8, new1: u8) -> Log {
    log(
        vec![keccak256("SetFeeProtocol(uint8,uint8,uint8,uint8)")],
        vec![
            U256::from(old0),
            U256::from(old1),
            U256::from(new0),
            U256::from(new1),
        ],
    )
}

fn collect_protocol_log(amount0: u128, amount1: u128) -> Log {
    log(
        vec![
            keccak256("CollectProtocol(address,address,uint128,uint128)"),
            address_topic(OWNER),
            address_topic(OWNER),
        ],
        vec![U256::from(amount0), U256::from(amount1)],
    )
}

fn grow_log(old: u16, new: u16) -> Log {
    log(
        vec![keccak256(
            "IncreaseObservationCardinalityNext(uint16,uint16)",
        )],
        vec![U256::from(old), U256::from(new)],
    )
}

fn decode(registration: &PoolRegistration, log: &Log, parent: &Parent) -> AdapterEventResult {
    ConcentratedLiquidityAdapter::default().decode_event_with_context(
        registration,
        log,
        parent,
        &context(),
    )
}

/// A contradicted parent — or an event that cannot describe canonical history —
/// discards the pool's storage entirely. That is the fail-closed outcome: the
/// pool is rebuilt from the chain rather than continuing from a guess.
#[track_caller]
fn assert_fails_closed(result: &AdapterEventResult, why: &str) {
    assert!(
        result.error.is_some(),
        "{why}: expected a typed error, got none"
    );
    let event = result
        .event
        .as_ref()
        .unwrap_or_else(|| panic!("{why}: invalidation must still surface an event"));
    assert_eq!(event.quality, UpdateQuality::RequiresRepair, "{why}");
    assert!(
        matches!(event.repair, RepairAction::PurgeStorage(pool) if pool == POOL),
        "{why}: expected whole-storage invalidation, got {:?}",
        event.repair
    );
}

#[track_caller]
fn assert_exact(result: &AdapterEventResult, why: &str) {
    assert_eq!(result.error, None, "{why}");
    let event = result
        .event
        .as_ref()
        .unwrap_or_else(|| panic!("{why}: expected an event"));
    assert_eq!(event.quality, UpdateQuality::Exact, "{why}");
    assert_eq!(event.repair, RepairAction::None, "{why}");
}

/// The baseline: a fresh in-range position on a coherent parent is exact, and it
/// needs no tick read at all because the bitmap proves both boundaries empty.
#[test]
fn a_fresh_in_range_mint_is_exact_without_reading_either_tick() {
    let result = decode(
        &canonical_registration(),
        &mint_log(-120, 120, 50_000_000_000_000_000),
        &coherent_parent(),
    );
    assert_exact(&result, "fresh in-range mint");
    let event = result.event.expect("event");
    let layout = layout();
    for tick in [-120, 120] {
        for slot in v3_tick_info_storage_keys_with_base(tick, layout.ticks_base_slot) {
            assert!(
                event.updates.iter().any(|update| matches!(
                    update,
                    StateUpdate::Slot { slot: written, .. } if *written == slot
                )),
                "every Tick.Info word of a newly initialized tick must be written"
            );
        }
    }
}

/// `Collect` moves only position accounting, so it is an exact transition that
/// writes no pricing state and drops exactly the collecting position.
#[test]
fn canonical_collect_is_exact_and_touches_only_its_position() {
    let result = decode(
        &canonical_registration(),
        &collect_log(-120, 120),
        &coherent_parent(),
    );
    assert_exact(&result, "canonical Collect");
    let event = result.event.expect("event");
    assert_eq!(event.updates.len(), 1, "Collect writes nothing");
    match &event.updates[0] {
        StateUpdate::Purge {
            address,
            scope: PurgeScope::Slots(slots),
        } => {
            assert_eq!(*address, POOL);
            assert_eq!(slots.len(), 4, "a position occupies four words");
        }
        other => panic!("Collect must only invalidate its position, got {other:?}"),
    }
}

/// Mandatory cold-start cells are not optional. Without them the transition
/// cannot be computed at all, and an absent value is never read as zero.
#[test]
fn a_missing_mandatory_parent_cell_fails_closed() {
    let layout = layout();
    for (label, slot) in [
        ("slot0", layout.slot0_slot),
        ("feeGrowthGlobal0", U256::from(1)),
        ("feeGrowthGlobal1", U256::from(2)),
        ("liquidity", layout.liquidity_slot),
        ("current observation", U256::from(8)),
    ] {
        let mut parent = coherent_parent();
        parent.0.remove(&slot);
        assert_fails_closed(
            &decode(
                &canonical_registration(),
                &mint_log(-120, 120, 1_000_000),
                &parent,
            ),
            &format!("missing {label}"),
        );
    }
}

/// An unknown boundary is different in kind from a contradicted parent: it costs
/// only the cells that boundary occupies, and the rest of the pool stays exact.
#[test]
fn an_unknown_boundary_resyncs_rather_than_purging() {
    let layout = layout();
    // Tick 20040 lives in bitmap word 1, which this parent never warmed.
    let result = decode(
        &canonical_registration(),
        &mint_log(-60, 20_040, 1_000_000),
        &coherent_parent(),
    );
    assert_eq!(result.error, None, "an unknown cell is not an error");
    let event = result.event.expect("event");
    assert_eq!(event.quality, UpdateQuality::RequiresRepair);
    let expected: Vec<(Address, U256)> =
        v3_tick_info_storage_keys_with_base(20_040, layout.ticks_base_slot)
            .into_iter()
            .chain([v3_tick_bitmap_storage_key_with_base(
                1,
                layout.tick_bitmap_base_slot,
            )])
            .map(|slot| (POOL, slot))
            .collect();
    match &event.repair {
        RepairAction::VerifySlots(slots) => {
            let mut got = slots.clone();
            let mut want = expected;
            got.sort_unstable();
            want.sort_unstable();
            assert_eq!(got, want);
        }
        other => panic!("expected a targeted resync, got {other:?}"),
    }
}

/// A locked `slot0` cannot be a transaction parent: canonical entrypoints hold
/// the reentrancy lock only mid-call.
#[test]
fn a_locked_parent_slot0_fails_closed() {
    let mut parent = coherent_parent();
    let layout = layout();
    let slot0 = parent.0[&layout.slot0_slot];
    parent
        .0
        .insert(layout.slot0_slot, slot0 & !(U256::from(1) << 240_usize));
    assert_fails_closed(
        &decode(
            &canonical_registration(),
            &mint_log(-120, 120, 1_000_000),
            &parent,
        ),
        "locked slot0",
    );
}

/// The bitmap and `Tick.Info` must agree. The empty-tick inference reads a clear
/// bit as proof the struct is zero, so a parent where the two disagree would
/// make that inference unsound and must be rejected.
#[test]
fn a_bitmap_disagreeing_with_tick_info_fails_closed() {
    let layout = layout();
    let keys = v3_tick_info_storage_keys_with_base(-120, layout.ticks_base_slot);
    let bitmap_slot = v3_tick_bitmap_storage_key_with_base(-1, layout.tick_bitmap_base_slot);
    let bit = U256::from(1) << ((-120_i32).div_euclid(TICK_SPACING).rem_euclid(256) as usize);

    // Bit set, but the tick carries no gross.
    let mut parent = coherent_parent();
    parent.0.insert(bitmap_slot, bit);
    for key in keys {
        parent.0.insert(key, U256::ZERO);
    }
    assert_fails_closed(
        &decode(
            &canonical_registration(),
            &mint_log(-120, 120, 1_000_000),
            &parent,
        ),
        "bitmap bit set on an empty tick",
    );

    // Uninitialized tick carrying residue: `Tick.clear` zeroes the whole struct,
    // so leftover accumulators mean this parent did not come from canonical
    // history.
    let mut parent = coherent_parent();
    parent.0.insert(bitmap_slot, bit);
    parent.0.insert(keys[0], U256::ZERO);
    parent.0.insert(keys[1], U256::from(7));
    parent.0.insert(keys[2], U256::ZERO);
    parent.0.insert(keys[3], U256::ZERO);
    assert_fails_closed(
        &decode(
            &canonical_registration(),
            &mint_log(-120, 120, 1_000_000),
            &parent,
        ),
        "uninitialized Tick.Info carrying residue",
    );
}

/// Ranges that canonical `checkTicks` and `flipTick` would reject cannot be real
/// events for this pool.
#[test]
fn an_impossible_liquidity_range_fails_closed() {
    for (label, lower, upper) in [
        ("inverted", 120, -120),
        ("degenerate", 120, 120),
        ("below MIN_TICK", -887_340, 120),
        ("above MAX_TICK", -120, 887_340),
        ("lower not spacing-aligned", -119, 120),
        ("upper not spacing-aligned", -120, 119),
    ] {
        assert_fails_closed(
            &decode(
                &canonical_registration(),
                &mint_log(lower, upper, 1_000_000),
                &coherent_parent(),
            ),
            label,
        );
    }
}

/// `mint` requires a positive amount, and burning more than a tick holds cannot
/// have happened on chain.
#[test]
fn impossible_liquidity_amounts_fail_closed() {
    assert_fails_closed(
        &decode(
            &canonical_registration(),
            &mint_log(-120, 120, 0),
            &coherent_parent(),
        ),
        "zero-amount Mint",
    );
    assert_fails_closed(
        &decode(
            &canonical_registration(),
            &burn_log(-120, 120, 1_000_000),
            &coherent_parent(),
        ),
        "burning a tick that holds nothing",
    );
}

/// `Tick.update` bounds `liquidityGross` against `maxLiquidityPerTick`, which
/// canonical Uniswap holds as a runtime immutable derived from tick spacing.
#[test]
fn exceeding_max_liquidity_per_tick_fails_closed() {
    assert_fails_closed(
        &decode(
            &canonical_registration(),
            &mint_log(-120, 120, u128::MAX / 2),
            &coherent_parent(),
        ),
        "liquidityGross beyond maxLiquidityPerTick",
    );
}

/// A malformed log is rejected before any state is computed.
#[test]
fn a_malformed_liquidity_log_fails_closed() {
    let mut truncated = mint_log(-120, 120, 1_000_000);
    truncated.data =
        LogData::new_unchecked(truncated.topics().to_vec(), Bytes::from(vec![0_u8; 31]));
    assert_fails_closed(
        &decode(&canonical_registration(), &truncated, &coherent_parent()),
        "truncated Mint data",
    );

    let mut missing_topics = mint_log(-120, 120, 1_000_000);
    missing_topics.data = LogData::new_unchecked(
        vec![keccak256(
            "Mint(address,address,int24,int24,uint128,uint256,uint256)",
        )],
        missing_topics.data.data.clone(),
    );
    assert_fails_closed(
        &decode(
            &canonical_registration(),
            &missing_topics,
            &coherent_parent(),
        ),
        "Mint without indexed topics",
    );
}

/// Every accounting event carries a postcondition the parent must satisfy, so a
/// foreign or replayed event cannot be applied.
#[test]
fn accounting_events_validate_against_the_parent() {
    let registration = canonical_registration();
    let parent = coherent_parent();

    // Flash must have been repaid at least the fee the pool quotes.
    assert_fails_closed(
        &decode(&registration, &flash_log(1_000_000, 0, 1, 0), &parent),
        "flash repaid below the quoted fee",
    );
    // Flash reverts outright without active liquidity.
    let mut empty = coherent_parent();
    empty.0.insert(layout().liquidity_slot, U256::ZERO);
    assert_fails_closed(
        &decode(&registration, &flash_log(0, 0, 1_000, 1_000), &empty),
        "flash against zero liquidity",
    );
    // The old split the event reports must match the parent's own byte.
    assert_fails_closed(
        &decode(&registration, &set_fee_protocol_log(4, 4, 6, 6), &parent),
        "SetFeeProtocol disagreeing with the parent split",
    );
    // Only zero or 4..=10 are canonical denominators.
    assert_fails_closed(
        &decode(&registration, &set_fee_protocol_log(0, 0, 3, 6), &parent),
        "SetFeeProtocol with an out-of-range denominator",
    );
    // Protocol fees cannot be collected beyond what accrued (parent token0 = 303).
    assert_fails_closed(
        &decode(&registration, &collect_protocol_log(400, 0), &parent),
        "CollectProtocol beyond the accrued balance",
    );
    // Ring growth must start from the parent's own reservation and increase.
    assert_fails_closed(
        &decode(&registration, &grow_log(7, 9), &parent),
        "ring growth from the wrong reservation",
    );
    assert_fails_closed(
        &decode(&registration, &grow_log(1, 1), &parent),
        "ring growth that does not grow",
    );
}

/// Family capability is not inferred from a matching event ABI or a similar
/// slot layout. PancakeSwap V3's `Mint`/`Burn` are byte-identical to canonical
/// Uniswap's, and it is still not promoted.
#[test]
fn an_unproven_family_is_never_promoted_by_abi_similarity() {
    let pancake = registration(ProtocolMetadata::PancakeV3(
        V3Metadata::default()
            .with_fee(FEE)
            .with_tick_spacing(TICK_SPACING),
    ));
    assert_eq!(
        ConcentratedLiquidityAdapter::liquidity_transition_capability(&pancake),
        V3LiquidityTransitionCapability::Unsupported
    );
    assert_fails_closed(
        &decode(
            &pancake,
            &mint_log(-120, 120, 1_000_000),
            &coherent_parent(),
        ),
        "PancakeSwap V3 Mint",
    );
}

/// A registration that supplies a non-canonical layout is not canonical Uniswap,
/// whatever its protocol id says.
#[test]
fn a_non_canonical_layout_is_never_promoted() {
    let shifted = registration(ProtocolMetadata::UniswapV3(
        V3Metadata::default()
            .with_fee(FEE)
            .with_tick_spacing(TICK_SPACING)
            .with_storage_layout(V3StorageLayout::new(
                U256::from(6),
                U256::from(15),
                U256::from(17),
                U256::from(18),
                TICK_SPACING,
            )),
    ));
    assert_eq!(
        ConcentratedLiquidityAdapter::liquidity_transition_capability(&shifted),
        V3LiquidityTransitionCapability::Unsupported
    );
    assert_fails_closed(
        &decode(
            &shifted,
            &mint_log(-120, 120, 1_000_000),
            &coherent_parent(),
        ),
        "canonical protocol id with a foreign layout",
    );
}

/// A non-positive tick spacing cannot describe a real pool and would divide by
/// zero in the bitmap-word math.
#[test]
fn a_non_positive_tick_spacing_is_never_promoted() {
    for spacing in [0, -60] {
        let broken = registration(ProtocolMetadata::UniswapV3(
            V3Metadata::default()
                .with_fee(FEE)
                .with_tick_spacing(spacing),
        ));
        assert_eq!(
            ConcentratedLiquidityAdapter::liquidity_transition_capability(&broken),
            V3LiquidityTransitionCapability::Unsupported
        );
    }
}
