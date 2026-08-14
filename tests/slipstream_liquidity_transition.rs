use std::collections::BTreeMap;

use alloy_primitives::{Address, B256, Bytes, Log, U256, address, b256, keccak256};
use evm_amm_state::adapters::storage::{
    V3StorageLayout, slipstream_tick_info_storage_keys_with_base,
    v3_tick_bitmap_storage_key_with_base, v3_word_position,
};
use evm_amm_state::adapters::{
    AdapterEventContext, AmmAdapter, ConcentratedLiquidityAdapter, PoolKey, PoolRegistration,
    ProtocolMetadata, RepairAction, StateUpdate, StateView, UpdateQuality, V3Metadata,
};

const POOL: Address = address!("b378137c90444bbcecd44a1f766851fbf53d2a9e");
const BLOCK_HASH: B256 = b256!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
const PARENT_HASH: B256 = b256!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
const TX_HASH: B256 = b256!("cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc");
const Q96: U256 = U256::from_limbs([0, 1 << 32, 0, 0]);
const WORD_128_MASK: U256 = U256::from_limbs([u64::MAX, u64::MAX, 0, 0]);

#[derive(Clone, Default)]
struct FixtureState(BTreeMap<(Address, U256), U256>);

impl StateView for FixtureState {
    fn storage(&self, address: Address, slot: U256) -> Option<U256> {
        self.0.get(&(address, slot)).copied()
    }
}

fn topic_address(value: Address) -> B256 {
    let mut topic = [0_u8; 32];
    topic[12..].copy_from_slice(value.as_slice());
    B256::from(topic)
}

fn topic_i24(value: i32) -> B256 {
    let mut topic = if value < 0 { [0xff; 32] } else { [0_u8; 32] };
    let value = (value as u32) & 0x00ff_ffff;
    topic[29] = (value >> 16) as u8;
    topic[30] = (value >> 8) as u8;
    topic[31] = value as u8;
    B256::from(topic)
}

fn mint_log(tick_lower: i32, tick_upper: i32, amount: u128) -> Log {
    liquidity_log(POOL, true, tick_lower, tick_upper, amount)
}

fn burn_log(pool: Address, tick_lower: i32, tick_upper: i32, amount: u128) -> Log {
    liquidity_log(pool, false, tick_lower, tick_upper, amount)
}

fn liquidity_log(
    pool: Address,
    is_mint: bool,
    tick_lower: i32,
    tick_upper: i32,
    amount: u128,
) -> Log {
    let mut data = Vec::with_capacity(128);
    if is_mint {
        data.extend_from_slice(
            &U256::from_be_slice(Address::repeat_byte(0x11).as_slice()).to_be_bytes::<32>(),
        );
    }
    data.extend_from_slice(&U256::from(amount).to_be_bytes::<32>());
    data.extend_from_slice(&U256::from(1).to_be_bytes::<32>());
    data.extend_from_slice(&U256::from(1).to_be_bytes::<32>());
    Log::new(
        pool,
        vec![
            if is_mint {
                keccak256("Mint(address,address,int24,int24,uint128,uint256,uint256)")
            } else {
                keccak256("Burn(address,int24,int24,uint128,uint256,uint256)")
            },
            topic_address(Address::repeat_byte(0x12)),
            topic_i24(tick_lower),
            topic_i24(tick_upper),
        ],
        Bytes::from(data),
    )
    .expect("valid Mint log")
}

fn collect_log(pool: Address, tick_lower: i32, tick_upper: i32) -> Log {
    let mut data = Vec::with_capacity(96);
    data.extend_from_slice(
        &U256::from_be_slice(Address::repeat_byte(0x13).as_slice()).to_be_bytes::<32>(),
    );
    data.extend_from_slice(&U256::from(1).to_be_bytes::<32>());
    data.extend_from_slice(&U256::from(1).to_be_bytes::<32>());
    Log::new(
        pool,
        vec![
            keccak256("Collect(address,address,int24,int24,uint128,uint128)"),
            topic_address(Address::repeat_byte(0x12)),
            topic_i24(tick_lower),
            topic_i24(tick_upper),
        ],
        Bytes::from(data),
    )
    .expect("valid Collect log")
}

struct TransitionFixture {
    pool: Address,
    layout: V3StorageLayout,
    lower_keys: [U256; 6],
    upper_keys: [U256; 6],
    lower_bitmap: U256,
    upper_bitmap: U256,
    observation_slot: U256,
    state: FixtureState,
    registration: PoolRegistration,
    context: AdapterEventContext,
}

impl TransitionFixture {
    fn new(pool: Address, chain_id: u64, tick_lower: i32, tick_upper: i32) -> Self {
        let layout = V3StorageLayout::slipstream(200);
        let slot0 = Q96
            | (U256::from(1) << 200_usize)
            | (U256::from(1) << 216_usize)
            | (U256::from(1) << 232_usize);
        let liquidity_word = U256::from(1_000_u64) | (U256::from(1_000_000_u64) << 128_usize);
        let observation_slot = U256::from(20);
        let observation = U256::from(1_000_u32) | (U256::from(1) << 248_usize);
        let lower_keys =
            slipstream_tick_info_storage_keys_with_base(tick_lower, layout.ticks_base_slot);
        let upper_keys =
            slipstream_tick_info_storage_keys_with_base(tick_upper, layout.ticks_base_slot);
        let lower_bitmap = v3_tick_bitmap_storage_key_with_base(
            v3_word_position(tick_lower, layout.tick_spacing),
            layout.tick_bitmap_base_slot,
        );
        let upper_bitmap = v3_tick_bitmap_storage_key_with_base(
            v3_word_position(tick_upper, layout.tick_spacing),
            layout.tick_bitmap_base_slot,
        );
        let mut state = FixtureState::default();
        for (slot, value) in [
            (layout.slot0_slot, slot0),
            (layout.liquidity_slot, liquidity_word),
            (U256::from(7), U256::from(11)),
            (U256::from(8), U256::from(13)),
            (U256::from(9), U256::from(17)),
            (observation_slot, observation),
            (lower_bitmap, U256::ZERO),
            (upper_bitmap, U256::ZERO),
        ] {
            state.0.insert((pool, slot), value);
        }
        for slot in lower_keys.iter().chain(upper_keys.iter()) {
            state.0.insert((pool, *slot), U256::ZERO);
        }
        let registration = PoolRegistration::new(PoolKey::Slipstream(pool))
            .with_state_address(pool)
            .with_metadata(ProtocolMetadata::Slipstream(
                V3Metadata::default()
                    .with_tick_spacing(layout.tick_spacing)
                    .with_storage_layout(layout),
            ));
        let context = AdapterEventContext::for_block(100, BLOCK_HASH, 1_001)
            .with_chain_id(chain_id)
            .with_parent_hash(PARENT_HASH)
            .with_transaction_hash(TX_HASH)
            .with_event_order(2, 3);
        Self {
            pool,
            layout,
            lower_keys,
            upper_keys,
            lower_bitmap,
            upper_bitmap,
            observation_slot,
            state,
            registration,
            context,
        }
    }

    fn seed_initialized_ticks(&mut self, amount: u128) {
        self.state.0.insert(
            (self.pool, self.lower_keys[0]),
            U256::from(amount) | (U256::from(amount) << 128_usize),
        );
        self.state.0.insert(
            (self.pool, self.upper_keys[0]),
            U256::from(amount) | (U256::from((-(amount as i128)) as u128) << 128_usize),
        );
        self.state
            .0
            .insert((self.pool, self.lower_keys[5]), U256::from(1) << 248_usize);
        self.state
            .0
            .insert((self.pool, self.upper_keys[5]), U256::from(1) << 248_usize);
        for (bitmap, tick) in [(self.lower_bitmap, -200_i32), (self.upper_bitmap, 200_i32)] {
            let bit = tick.div_euclid(self.layout.tick_spacing).rem_euclid(256) as usize;
            let current = self.state.storage(self.pool, bitmap).unwrap_or_default();
            self.state
                .0
                .insert((self.pool, bitmap), current | (U256::from(1) << bit));
        }
    }

    fn decode(&self, log: &Log) -> evm_amm_state::adapters::AdapterEventResult {
        ConcentratedLiquidityAdapter::default().decode_event_with_context(
            &self.registration,
            log,
            &self.state,
            &self.context,
        )
    }
}

fn apply(state: &FixtureState, updates: &[StateUpdate]) -> FixtureState {
    let mut next = state.clone();
    for update in updates {
        match update {
            StateUpdate::Slot {
                address,
                slot,
                value,
            } => {
                next.0.insert((*address, *slot), *value);
            }
            other => panic!("exact liquidity transition emitted {other:?}"),
        }
    }
    next
}

#[test]
fn reviewed_base_in_range_mint_advances_search_state_without_repair() {
    let layout = V3StorageLayout::slipstream(200);
    let tick_lower = -200;
    let tick_upper = 200;
    let amount = 7_u128;
    let timestamp_before = 1_000_u32;
    let timestamp_after = 1_001_u32;
    let slot0 = Q96
        | (U256::from(1) << 200_usize)
        | (U256::from(1) << 216_usize)
        | (U256::from(1) << 232_usize);
    let liquidity_word = U256::from(1_000_u64) | (U256::from(1_000_000_u64) << 128_usize);
    let observation_slot = U256::from(20);
    let observation = U256::from(timestamp_before) | (U256::from(1) << 248_usize);
    let lower_keys =
        slipstream_tick_info_storage_keys_with_base(tick_lower, layout.ticks_base_slot);
    let upper_keys =
        slipstream_tick_info_storage_keys_with_base(tick_upper, layout.ticks_base_slot);
    let lower_bitmap = v3_tick_bitmap_storage_key_with_base(
        v3_word_position(tick_lower, layout.tick_spacing),
        layout.tick_bitmap_base_slot,
    );
    let upper_bitmap = v3_tick_bitmap_storage_key_with_base(
        v3_word_position(tick_upper, layout.tick_spacing),
        layout.tick_bitmap_base_slot,
    );
    let mut state = FixtureState::default();
    for (slot, value) in [
        (layout.slot0_slot, slot0),
        (layout.liquidity_slot, liquidity_word),
        (U256::from(7), U256::from(11)),
        (U256::from(8), U256::from(13)),
        (observation_slot, observation),
        (lower_bitmap, U256::ZERO),
        (upper_bitmap, U256::ZERO),
    ] {
        state.0.insert((POOL, slot), value);
    }
    for slot in lower_keys.iter().chain(upper_keys.iter()) {
        state.0.insert((POOL, *slot), U256::ZERO);
    }

    let registration = PoolRegistration::new(PoolKey::Slipstream(POOL))
        .with_state_address(POOL)
        .with_metadata(ProtocolMetadata::Slipstream(
            V3Metadata::default()
                .with_tick_spacing(layout.tick_spacing)
                .with_storage_layout(layout),
        ));
    let context = AdapterEventContext::for_block(100, BLOCK_HASH, u64::from(timestamp_after))
        .with_chain_id(8_453)
        .with_parent_hash(PARENT_HASH)
        .with_transaction_hash(TX_HASH)
        .with_event_order(2, 3);
    let decoded = ConcentratedLiquidityAdapter::default().decode_event_with_context(
        &registration,
        &mint_log(tick_lower, tick_upper, amount),
        &state,
        &context,
    );
    assert_eq!(decoded.error, None);
    let event = decoded.event.expect("recognized Mint");
    assert_eq!(event.quality, UpdateQuality::Exact);
    assert_eq!(event.repair, RepairAction::None);
    let next = apply(&state, &event.updates);

    assert_eq!(
        next.storage(POOL, layout.liquidity_slot).unwrap() & WORD_128_MASK,
        U256::from(1_007_u64),
    );
    assert_eq!(
        next.storage(POOL, lower_keys[0]),
        Some(U256::from(amount) | (U256::from(amount) << 128_usize)),
    );
    assert_eq!(
        next.storage(POOL, upper_keys[0]),
        Some(U256::from(amount) | (U256::from((-(amount as i128)) as u128) << 128_usize)),
    );
    assert_eq!(next.storage(POOL, lower_keys[2]), Some(U256::from(11)));
    assert_eq!(next.storage(POOL, lower_keys[3]), Some(U256::from(13)));
    assert_eq!(next.storage(POOL, upper_keys[2]), Some(U256::ZERO));
    assert_eq!(next.storage(POOL, upper_keys[3]), Some(U256::ZERO));
    assert_eq!(
        next.storage(POOL, lower_bitmap),
        Some(U256::from(1) << 255_usize),
    );
    assert_eq!(
        next.storage(POOL, upper_bitmap),
        Some(U256::from(1) << 1_usize)
    );

    let seconds_per_liquidity = (U256::from(1) << 128_usize) / U256::from(1_000_u64);
    let lower_word5 = (seconds_per_liquidity << 56_usize)
        | (U256::from(timestamp_after) << 216_usize)
        | (U256::from(1) << 248_usize);
    assert_eq!(next.storage(POOL, lower_keys[5]), Some(lower_word5));
    assert_eq!(
        next.storage(POOL, upper_keys[5]),
        Some(U256::from(1) << 248_usize),
    );
    let next_observation = U256::from(timestamp_after)
        | (seconds_per_liquidity << 88_usize)
        | (U256::from(1) << 248_usize);
    assert_eq!(next.storage(POOL, observation_slot), Some(next_observation));
}

#[test]
fn reviewed_base_burn_to_zero_clears_ticks_bitmaps_and_active_liquidity() {
    let mut fixture = TransitionFixture::new(POOL, 8_453, -200, 200);
    fixture.seed_initialized_ticks(7);
    let decoded = fixture.decode(&burn_log(POOL, -200, 200, 7));
    assert_eq!(decoded.error, None);
    let event = decoded.event.expect("recognized Burn");
    assert_eq!(event.quality, UpdateQuality::Exact);
    assert_eq!(event.repair, RepairAction::None);
    let next = apply(&fixture.state, &event.updates);

    for slot in fixture.lower_keys.iter().chain(fixture.upper_keys.iter()) {
        assert_eq!(next.storage(POOL, *slot), Some(U256::ZERO));
    }
    assert_eq!(next.storage(POOL, fixture.lower_bitmap), Some(U256::ZERO));
    assert_eq!(next.storage(POOL, fixture.upper_bitmap), Some(U256::ZERO));
    assert_eq!(
        next.storage(POOL, fixture.layout.liquidity_slot).unwrap() & WORD_128_MASK,
        U256::from(993_u64),
    );
    assert_ne!(
        next.storage(POOL, fixture.observation_slot),
        fixture.state.storage(POOL, fixture.observation_slot),
    );
}

#[test]
fn reviewed_base_existing_ticks_preserve_non_liquidity_words_and_bitmap() {
    let mut fixture = TransitionFixture::new(POOL, 8_453, -200, 200);
    fixture.seed_initialized_ticks(10);
    for (keys, marker) in [(&fixture.lower_keys, 20_u64), (&fixture.upper_keys, 40_u64)] {
        for (index, key) in keys.iter().enumerate().skip(1).take(4) {
            fixture
                .state
                .0
                .insert((POOL, *key), U256::from(marker + index as u64));
        }
    }
    let lower_tail = fixture.state.storage(POOL, fixture.lower_keys[5]).unwrap();
    let upper_tail = fixture.state.storage(POOL, fixture.upper_keys[5]).unwrap();
    let lower_bitmap = fixture.state.storage(POOL, fixture.lower_bitmap).unwrap();
    let upper_bitmap = fixture.state.storage(POOL, fixture.upper_bitmap).unwrap();
    let decoded = fixture.decode(&mint_log(-200, 200, 7));
    assert_eq!(decoded.error, None);
    let event = decoded.event.expect("recognized Mint");
    let next = apply(&fixture.state, &event.updates);

    assert_eq!(
        next.storage(POOL, fixture.lower_keys[0]),
        Some(U256::from(17_u64) | (U256::from(17_u64) << 128_usize)),
    );
    assert_eq!(
        next.storage(POOL, fixture.upper_keys[0]),
        Some(U256::from(17_u64) | (U256::from((-17_i128) as u128) << 128_usize),),
    );
    for keys in [&fixture.lower_keys, &fixture.upper_keys] {
        for key in keys.iter().skip(1).take(4) {
            assert_eq!(next.storage(POOL, *key), fixture.state.storage(POOL, *key));
        }
    }
    assert_eq!(next.storage(POOL, fixture.lower_keys[5]), Some(lower_tail));
    assert_eq!(next.storage(POOL, fixture.upper_keys[5]), Some(upper_tail));
    assert_eq!(next.storage(POOL, fixture.lower_bitmap), Some(lower_bitmap));
    assert_eq!(next.storage(POOL, fixture.upper_bitmap), Some(upper_bitmap));
}

#[test]
fn reviewed_base_out_of_range_mint_only_changes_ticks_and_shared_bitmap_word() {
    let fixture = TransitionFixture::new(POOL, 8_453, 200, 400);
    assert_eq!(fixture.lower_bitmap, fixture.upper_bitmap);
    let decoded = fixture.decode(&liquidity_log(POOL, true, 200, 400, 7));
    assert_eq!(decoded.error, None);
    let event = decoded.event.expect("recognized Mint");
    let next = apply(&fixture.state, &event.updates);

    assert_eq!(
        next.storage(POOL, fixture.layout.liquidity_slot),
        fixture.state.storage(POOL, fixture.layout.liquidity_slot),
    );
    assert_eq!(
        next.storage(POOL, fixture.layout.slot0_slot),
        fixture.state.storage(POOL, fixture.layout.slot0_slot),
    );
    assert_eq!(
        next.storage(POOL, fixture.observation_slot),
        fixture.state.storage(POOL, fixture.observation_slot),
    );
    assert_eq!(
        next.storage(POOL, fixture.lower_bitmap),
        Some((U256::from(1) << 1_usize) | (U256::from(1) << 2_usize)),
    );
    assert_eq!(
        next.storage(POOL, fixture.lower_keys[5]),
        Some(U256::from(1) << 248_usize),
    );
    assert_eq!(
        next.storage(POOL, fixture.upper_keys[5]),
        Some(U256::from(1) << 248_usize),
    );
}

#[test]
fn reviewed_optimism_pool_uses_the_same_exact_liquidity_transition() {
    let optimism_pool = address!("173cdc71e29d5cffa6d090ad99f555a24b8831f9");
    let fixture = TransitionFixture::new(optimism_pool, 10, -200, 200);
    let decoded = fixture.decode(&liquidity_log(optimism_pool, true, -200, 200, 7));
    assert_eq!(decoded.error, None);
    let event = decoded.event.expect("recognized Optimism Mint");
    assert_eq!(event.quality, UpdateQuality::Exact);
    assert_eq!(event.repair, RepairAction::None);
    let next = apply(&fixture.state, &event.updates);
    assert_eq!(
        next.storage(optimism_pool, fixture.lower_keys[4]),
        Some(U256::from(17_u64)),
    );
}

#[test]
fn missing_tick_parent_cell_fails_closed_to_full_repair() {
    let mut fixture = TransitionFixture::new(POOL, 8_453, -200, 200);
    fixture.state.0.remove(&(POOL, fixture.upper_keys[4]));
    let decoded = fixture.decode(&mint_log(-200, 200, 7));
    assert!(matches!(
        decoded.error,
        Some(evm_amm_state::adapters::AdapterEventError::MissingState { .. })
    ));
    let event = decoded.event.expect("recognized but unprovable Mint");
    assert_eq!(event.quality, UpdateQuality::RequiresRepair);
    assert_eq!(event.repair, RepairAction::PurgeStorage(POOL));
    assert!(matches!(
        event.updates.as_slice(),
        [StateUpdate::Purge { .. }]
    ));
}

#[test]
fn reviewed_pool_on_wrong_chain_fails_closed() {
    let fixture = TransitionFixture::new(POOL, 10, -200, 200);
    let decoded = fixture.decode(&mint_log(-200, 200, 7));
    assert!(decoded.error.is_some());
    let event = decoded.event.expect("recognized but wrong-chain Mint");
    assert_eq!(event.quality, UpdateQuality::RequiresRepair);
    assert_eq!(event.repair, RepairAction::PurgeStorage(POOL));
}

#[test]
fn zero_amount_burn_is_exact_search_state_noop() {
    let fixture = TransitionFixture::new(POOL, 8_453, -200, 200);
    let decoded = fixture.decode(&burn_log(POOL, -200, 200, 0));
    assert_eq!(decoded.error, None);
    let event = decoded.event.expect("recognized zero Burn");
    assert_eq!(event.quality, UpdateQuality::Exact);
    assert_eq!(event.repair, RepairAction::None);
    assert!(event.updates.is_empty());
}

#[test]
fn collect_after_liquidity_change_is_exact_search_state_noop() {
    let fixture = TransitionFixture::new(POOL, 8_453, -200, 200);
    let decoded = fixture.decode(&collect_log(POOL, -200, 200));
    assert_eq!(decoded.error, None);
    let event = decoded.event.expect("recognized Collect");
    assert_eq!(event.quality, UpdateQuality::Exact);
    assert_eq!(event.repair, RepairAction::None);
    assert!(event.updates.is_empty());
}
