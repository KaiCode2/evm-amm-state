use std::{
    collections::BTreeMap,
    str::FromStr,
    time::{Duration, Instant},
};

use alloy_primitives::{Address, B256, Bytes, Log, U256, address, b256, hex, keccak256};
use evm_amm_state::adapters::storage::v3_tick_bitmap_storage_key;
use evm_amm_state::adapters::{
    AdapterEvent, AdapterEventContext, AdapterEventError, AmmAdapter, ConcentratedLiquidityAdapter,
    PoolKey, PoolRegistration, ProtocolMetadata, PurgeScope, StateUpdate, StateView, UpdateQuality,
    V3Metadata, V3StorageLayout, V3TransitionError,
};

const INCIDENT_BLOCK: u64 = 25_723_647;
const INCIDENT_TIMESTAMP: u64 = 1_786_353_491;
const POOL: Address = address!("fBa26C3F9C8eCeF989def3C5c8aD037487462d83");
const BLOCK_HASH: B256 = b256!("5e5c89c910eaa065a7b0b8ebdf8bdb13edf5697ed7b593b723a26402d7e27fc2");
const PARENT_HASH: B256 = b256!("abacb8c50e9e2af3c89a257c909f6bc66dae5f9b8c64f61428b8135fd047972e");
// These two transactions were independently checked against the deployed pool
// bytecode using `debug_traceTransaction` with geth's `prestateTracer` in
// `diffMode=true`. The assertions below cover every storage word written by
// either transaction, including the intermediate poststate after the first.
const FIRST_TRANSACTION_HASH: B256 =
    b256!("8ce97be13effe21a24b234df1db74ea3569ce9953c54b4373154c5e3dc849b92");
const SECOND_TRANSACTION_HASH: B256 =
    b256!("75b5edcaf9e5da77246fddc2cdb5aeb43fc2baa0c6d5a7f94db6730ecd3615bf");

#[derive(Default)]
struct FixtureState(BTreeMap<(Address, U256), U256>);

impl FixtureState {
    fn seed(&mut self, slot: u64, value: &str) {
        self.0.insert((POOL, U256::from(slot)), word(value));
    }

    fn seed_key(&mut self, slot: U256, value: &str) {
        self.0.insert((POOL, slot), word(value));
    }

    fn apply(&mut self, event: &AdapterEvent) {
        for update in &event.updates {
            match update {
                StateUpdate::Slot {
                    address,
                    slot,
                    value,
                } => {
                    self.0.insert((*address, *slot), *value);
                }
                StateUpdate::SlotMasked {
                    address,
                    slot,
                    mask,
                    value,
                } => {
                    let current = self
                        .storage(*address, *slot)
                        .expect("acceptance fixture contains every masked parent slot");
                    self.0
                        .insert((*address, *slot), (current & !*mask) | (*value & *mask));
                }
                other => panic!("incident transition emitted unexpected update: {other:?}"),
            }
        }
    }

    fn assert_slot(&self, slot: u64, expected: &str) {
        assert_eq!(
            self.storage(POOL, U256::from(slot)),
            Some(word(expected)),
            "storage slot {slot} differs from the canonical block result"
        );
    }
}

impl StateView for FixtureState {
    fn storage(&self, address: Address, slot: U256) -> Option<U256> {
        self.0.get(&(address, slot)).copied()
    }
}

fn word(value: &str) -> U256 {
    U256::from_str(value).expect("valid checked-in storage word")
}

fn registration() -> PoolRegistration {
    PoolRegistration::new(PoolKey::UniswapV3(POOL))
        .with_state_address(POOL)
        .with_metadata(ProtocolMetadata::UniswapV3(
            V3Metadata::default()
                .with_fee(3_000)
                .with_tick_spacing(60)
                .with_storage_layout(V3StorageLayout::uniswap(60)),
        ))
}

fn topic_address(value: Address) -> B256 {
    let mut topic = [0_u8; 32];
    topic[12..].copy_from_slice(value.as_slice());
    B256::from(topic)
}

fn swap_log(sender: Address, recipient: Address, data: Bytes) -> Log {
    Log::new(
        POOL,
        vec![
            keccak256("Swap(address,address,int256,int256,uint160,uint128,int24)"),
            topic_address(sender),
            topic_address(recipient),
        ],
        data,
    )
    .expect("valid checked-in swap log")
}

fn first_swap() -> Log {
    swap_log(
        address!("4c82d1FBfE28C977cbB58D8c7FF8FcF9f70a2CcA"),
        address!("BDd6d43eA5259d4E61D516e8E19Dc588f9C795A6"),
        Bytes::from(hex!(
            "fffffffffffffffffffffffffffffffffffffffffffffff5287143a539e00000"
            "0000000000000000000000000000000000000000000000003755eb492ab7dc34"
            "000000000000000000000000000000000000000024cb297cc4d038751a3edd34"
            "000000000000000000000000000000000000000000000027b6a1d4916dfea3c8"
            "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff6870"
        )),
    )
}

fn second_swap() -> Log {
    swap_log(
        address!("0000000000000000009E50A7ddB7a7b0e2eE6604"),
        address!("0000000000000000009E50A7ddB7a7b0e2eE6604"),
        Bytes::from(hex!(
            "00000000000000000000000000000000000000000000000010d8021af632f4cc"
            "ffffffffffffffffffffffffffffffffffffffffffffffffffa737361d000000"
            "000000000000000000000000000000000000000024c8ed2ac61e16a5419e83e4"
            "000000000000000000000000000000000000000000000027b6a1d4916dfea3c8"
            "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff686c"
        )),
    )
}

fn incident_parent() -> FixtureState {
    let mut state = FixtureState::default();
    state.seed(
        0,
        "0x000166009000900016ff656f00000000000000002367874a018409636fd72026",
    );
    state.seed(
        1,
        "0x00000000000000000000000000000000deb80f7569773c70ec5eaa8a3a09ad3b",
    );
    state.seed(
        2,
        "0x000000000000000000000000000000000e17aba50ecbfd7743f84be2d2e546da",
    );
    state.seed(
        3,
        "0x000000000000000000000ccf333cc4f4000000000000000017279d00179c286b",
    );
    state.seed(
        4,
        "0x000000000000000000000000000000000000000000000027b6a1d4916dfea3c8",
    );
    // observations[22] is the parent oracle head; observations[23] is the
    // ring position written by the first tick-changing swap in this block.
    state.seed(
        30,
        "0x01002bb7a4000000000053214ab73f43e592ed0bbdfffde1e1a1e4646a7993cf",
    );
    state.seed(
        31,
        "0x01002bb7a40000000000530a83d6ac86cc2a2c9327fffde404b125646a760b43",
    );
    // The complete tick path remains inside compressed bitmap word -3. Its
    // canonical zero value proves that none of the spacing boundaries crossed
    // by these swaps is initialized; absence would be unknown, not zero.
    state.seed_key(v3_tick_bitmap_storage_key(-3), "0x0");
    state
}

fn context(transaction_hash: B256, transaction_index: u64, log_index: u64) -> AdapterEventContext {
    AdapterEventContext::for_block(INCIDENT_BLOCK, BLOCK_HASH, INCIDENT_TIMESTAMP)
        .with_chain_id(1)
        .with_parent_hash(PARENT_HASH)
        .with_transaction_hash(transaction_hash)
        .with_event_order(transaction_index, log_index)
}

#[test]
fn ethereum_incident_ordered_swaps_match_every_swap_touched_storage_word() {
    let adapter = ConcentratedLiquidityAdapter::default();
    let registration = registration();
    let mut state = incident_parent();

    for (index, (transaction_hash, log, event_context)) in [
        (
            FIRST_TRANSACTION_HASH,
            first_swap(),
            context(FIRST_TRANSACTION_HASH, 1, 6),
        ),
        (
            SECOND_TRANSACTION_HASH,
            second_swap(),
            context(SECOND_TRANSACTION_HASH, 2, 12),
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let decoded =
            adapter.decode_event_with_context(&registration, &log, &state, &event_context);
        assert_eq!(
            decoded.error, None,
            "incident swap {transaction_hash} must decode exactly"
        );
        let event = decoded.event.expect("incident swap event");
        assert_eq!(
            event.quality,
            UpdateQuality::Exact,
            "complete parent state and context must produce an exact transition"
        );
        state.apply(&event);

        if index == 0 {
            state.assert_slot(
                0,
                "0x000166009000900017ff6870000000000000000024cb297cc4d038751a3edd34",
            );
            state.assert_slot(
                1,
                "0x00000000000000000000000000000000deb80f7569773c70ec5eaa8a3a09ad3b",
            );
            state.assert_slot(
                2,
                "0x000000000000000000000000000000000e188fef64470bb4d5d6afc0c031cbe0",
            );
            state.assert_slot(
                3,
                "0x00000000000000000007220c096719ed000000000000000017279d00179c286b",
            );
            state.assert_slot(
                4,
                "0x000000000000000000000000000000000000000000000027b6a1d4916dfea3c8",
            );
            state.assert_slot(
                31,
                "0x01002bb7a4000000000053216160d07a666c176240fffde1df827ea06a799753",
            );
        }
    }

    state.assert_slot(
        0,
        "0x000166009000900017ff686c000000000000000024c8ed2ac61e16a5419e83e4",
    );
    state.assert_slot(
        1,
        "0x00000000000000000000000000000000deb854f2db21fba9ebbbff2f60cfc2cf",
    );
    state.assert_slot(
        2,
        "0x000000000000000000000000000000000e188fef64470bb4d5d6afc0c031cbe0",
    );
    state.assert_slot(
        3,
        "0x00000000000000000007220c096719ed00000000000000001729c4effa4b0807",
    );
    state.assert_slot(
        4,
        "0x000000000000000000000000000000000000000000000027b6a1d4916dfea3c8",
    );
    state.assert_slot(
        31,
        "0x01002bb7a4000000000053216160d07a666c176240fffde1df827ea06a799753",
    );
}

#[test]
fn contextless_tick_change_must_not_claim_an_exact_transition() {
    let adapter = ConcentratedLiquidityAdapter::default();
    let decoded = adapter.decode_event(&registration(), &first_swap(), &incident_parent());

    assert!(
        decoded.error.is_some()
            || decoded.event.is_some_and(|event| {
                !matches!(
                    event.quality,
                    UpdateQuality::Exact | UpdateQuality::ExactIfApplied
                )
            }),
        "a tick-changing swap without its timestamp/order context cannot update the oracle ring exactly"
    );
}

#[test]
fn exact_transition_requires_chain_and_transaction_identity() {
    let adapter = ConcentratedLiquidityAdapter::default();
    let complete = context(FIRST_TRANSACTION_HASH, 1, 6);
    let mut missing_chain = complete;
    missing_chain.chain_id = None;
    let mut missing_transaction = complete;
    missing_transaction.transaction_hash = None;
    for (missing, event_context) in [
        ("chain_id", missing_chain),
        ("transaction_hash", missing_transaction),
    ] {
        let decoded = adapter.decode_event_with_context(
            &registration(),
            &first_swap(),
            &incident_parent(),
            &event_context,
        );
        assert_eq!(
            decoded.error,
            Some(AdapterEventError::V3Transition(
                V3TransitionError::MissingContext(missing),
            )),
        );
        let event = decoded.event.expect("recognized swap must invalidate");
        assert_eq!(event.quality, UpdateQuality::ConservativeInvalidation);
        assert_eq!(
            event.updates,
            vec![StateUpdate::purge(POOL, PurgeScope::AllStorage)],
        );
    }
}

#[test]
fn public_exact_path_rejects_adversarial_far_price_within_the_work_budget() {
    let mut state = FixtureState::default();
    let start_sqrt = U256::from_str("1461446703485210103287273052203988822378723970341")
        .expect("canonical max sqrt minus one");
    let start_tick = 887_271_i32;
    let liquidity = U256::from(1_000_000_000_000_000_000_u128);
    let slot0 = start_sqrt
        | (U256::from((start_tick as u32) & 0x00ff_ffff) << 160_usize)
        | (U256::from(1) << 200_usize)
        | (U256::from(1) << 216_usize)
        | (U256::from(1) << 240_usize);
    state.seed_key(U256::ZERO, &format!("{slot0}"));
    state.seed(1, "0x0");
    state.seed(2, "0x0");
    state.seed(3, "0x0");
    state.seed_key(U256::from(4), &format!("{liquidity}"));
    state.seed(
        8,
        &format!("{}", U256::from(100) | (U256::from(1) << 248_usize)),
    );
    // Starting near MAX_TICK and targeting MIN_TICK spans more than 4,096
    // canonical bitmap-word steps at spacing=1. Every required word is
    // explicitly proven zero so the rejection exercises the work ceiling,
    // rather than stopping early on missing state.
    for word_position in -632_i16..=3_465_i16 {
        state.seed_key(v3_tick_bitmap_storage_key(word_position), "0x0");
    }

    let mut data = Vec::with_capacity(32 * 5);
    data.extend_from_slice(&U256::from(1).to_be_bytes::<32>());
    data.extend_from_slice(&U256::MAX.to_be_bytes::<32>());
    data.extend_from_slice(&U256::from(4_295_128_740_u64).to_be_bytes::<32>());
    data.extend_from_slice(&liquidity.to_be_bytes::<32>());
    let final_tick = (!U256::from(887_272_u32)).wrapping_add(U256::from(1));
    data.extend_from_slice(&final_tick.to_be_bytes::<32>());
    let log = swap_log(Address::ZERO, Address::ZERO, Bytes::from(data));
    let started = Instant::now();
    let decoded = ConcentratedLiquidityAdapter::default().decode_event_with_context(
        &PoolRegistration::new(PoolKey::UniswapV3(POOL))
            .with_state_address(POOL)
            .with_metadata(ProtocolMetadata::UniswapV3(
                V3Metadata::default()
                    .with_fee(3_000)
                    .with_tick_spacing(1)
                    .with_storage_layout(V3StorageLayout::uniswap(1)),
            )),
        &log,
        &state,
        &context(B256::repeat_byte(0xdd), 1, 1),
    );
    let elapsed = started.elapsed();

    assert_eq!(
        decoded.error,
        Some(AdapterEventError::V3Transition(
            V3TransitionError::WorkLimitExceeded { limit: 4_096 }
        ))
    );
    let event = decoded.event.expect("recognized swap must invalidate");
    assert_eq!(event.quality, UpdateQuality::ConservativeInvalidation);
    assert_eq!(
        event.updates,
        vec![StateUpdate::purge(POOL, PurgeScope::AllStorage)],
        "failure must expose only whole-storage invalidation, never partial exact writes"
    );
    assert!(
        elapsed < Duration::from_secs(1),
        "bounded rejection took {elapsed:?}"
    );
}
