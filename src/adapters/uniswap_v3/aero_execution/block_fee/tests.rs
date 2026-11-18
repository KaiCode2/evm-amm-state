use super::*;
use crate::adapters::storage::V3StorageLayout;
use crate::adapters::uniswap_v3::reviewed_aero_cl_runtime;
use crate::adapters::{
    ConcentratedLiquidityAdapter, PoolKey, PoolRegistration, ProtocolMetadata,
    SlipstreamRuntimeFamily, V3Metadata,
};
use alloy_primitives::{Address, B256, Bytes, LogData, keccak256};
use serde_json::Value;
use std::collections::BTreeMap;

const CAPTURES: [&str; 2] = [
    include_str!("../../../../../tests/fixtures/aero_execution/ethereum-first-swap-fee-rpc.json"),
    include_str!("../../../../../tests/fixtures/aero_execution/base-first-swap-fee-rpc.json"),
];

#[derive(Clone, Default)]
struct Parent {
    slots: BTreeMap<(Address, U256), U256>,
    identities: BTreeMap<(u64, Address), B256>,
}
impl StateView for Parent {
    fn storage(&self, address: Address, slot: U256) -> Option<U256> {
        self.slots.get(&(address, slot)).copied()
    }
    fn verified_code_hash(&self, chain: u64, address: Address) -> Option<B256> {
        self.identities.get(&(chain, address)).copied()
    }
}
fn value<T: std::str::FromStr>(v: &Value) -> T
where
    T::Err: std::fmt::Debug,
{
    v.as_str().unwrap().parse().unwrap()
}

struct Fixture {
    pool: PoolRegistration,
    reviewed: ReviewedAeroClRuntime,
    log: Log,
    state: Parent,
    context: AdapterEventContext,
    key: U256,
    expected: U256,
}
impl Fixture {
    fn derive(&self) -> Result<Vec<StateUpdate>, AdapterEventError> {
        ConcentratedLiquidityAdapter::derive_aero_execution_event(
            &self.pool,
            &self.log,
            &self.state,
            &self.context,
        )
    }
}
fn fixture(capture: &str) -> Fixture {
    let capture: Value = serde_json::from_str(capture).unwrap();
    let r = &capture["reference"];
    let family = if r["chain_id"] == 1 {
        SlipstreamRuntimeFamily::AeroEthereumBifi
    } else {
        SlipstreamRuntimeFamily::AeroBaseMooBifi
    };
    let reviewed = reviewed_aero_cl_runtime(family).unwrap();
    assert_eq!(value::<Address>(&capture["pool"]), reviewed.pool);
    let raw = &capture["first_swap"];
    let pool = PoolRegistration::new(PoolKey::Slipstream(reviewed.pool)).with_metadata(
        ProtocolMetadata::Slipstream(
            V3Metadata::default()
                .with_tick_spacing(reviewed.tick_spacing)
                .with_storage_layout(V3StorageLayout::aero_cl(reviewed.tick_spacing)),
        ),
    );
    let context = AdapterEventContext::for_block(
        r["block_number"].as_u64().unwrap(),
        value(&r["block_hash"]),
        r["block_timestamp"].as_u64().unwrap(),
    )
    .with_chain_id(reviewed.chain_id)
    .with_parent_hash(value(&r["parent_hash"]))
    .with_transaction_hash(value(&raw["transactionHash"]))
    .with_event_order(
        value::<U256>(&raw["transactionIndex"]).to(),
        value::<U256>(&raw["logIndex"]).to(),
    );
    let log = Log {
        address: value(&raw["address"]),
        data: LogData::new(
            raw["topics"]
                .as_array()
                .unwrap()
                .iter()
                .map(value)
                .collect(),
            value(&raw["data"]),
        )
        .unwrap(),
    };
    let mut state = Parent::default();
    for request in capture["requests"].as_array().unwrap() {
        let p = &request["params"];
        match request["method"].as_str().unwrap() {
            "eth_getStorageAt" if p[2]["blockHash"] == r["parent_hash"] => {
                assert_eq!(p[2]["requireCanonical"], true);
                state.slots.insert(
                    (value(&p[0]), value(&p[1])),
                    value(&request["response"]["result"]),
                );
            }
            "eth_getCode" => {
                assert_eq!(p[1]["blockHash"], r["parent_hash"]);
                let code: Bytes = value(&request["response"]["result"]);
                state
                    .identities
                    .insert((reviewed.chain_id, value(&p[0])), keccak256(code));
            }
            _ => {}
        }
    }
    let requests = capture["requests"].as_array().unwrap();
    assert_eq!(
        requests.last().unwrap()["response"]["result"]["hash"],
        r["block_hash"]
    );
    assert_eq!(raw["blockHash"], r["block_hash"]);
    let logs = requests
        .iter()
        .find(|x| x["method"] == "eth_getLogs")
        .unwrap()["response"]["result"]
        .as_array()
        .unwrap();
    assert!(
        logs.iter()
            .all(|x| value::<U256>(&x["logIndex"]) >= value::<U256>(&raw["logIndex"])),
        "no prior relevant pool/governance mutation"
    );
    Fixture {
        pool,
        reviewed,
        log,
        state,
        context,
        key: value(&capture["block_fee_slot"]),
        expected: value(&capture["expected_stored_fee"]),
    }
}

#[test]
fn canonical_ethereum_and_base_first_swap_match_hash_pinned_stored_raw_fee() {
    for capture in CAPTURES {
        let f = fixture(capture);
        assert_eq!(
            f.derive().unwrap(),
            vec![StateUpdate::slot(f.reviewed.hook, f.key, f.expected)]
        );
    }
}

#[test]
fn missing_height_or_oracle_word_is_not_zero() {
    for capture in CAPTURES {
        let f = fixture(capture);
        let oracle_slots: Vec<_> = f
            .state
            .slots
            .keys()
            .filter(|(a, s)| *a == f.reviewed.pool && *s >= U256::from(17))
            .copied()
            .collect();
        assert!(!oracle_slots.is_empty());
        for key in std::iter::once((f.reviewed.hook, f.key)).chain(oracle_slots) {
            let mut missing = fixture(capture);
            missing.state.slots.remove(&key);
            assert!(
                missing.derive().is_err(),
                "unknown {key:?} must not be filled"
            );
        }
    }
}

#[test]
fn subsequent_swap_keeps_stored_fee_without_recomputing_from_post_swap_oracle() {
    let mut f = fixture(CAPTURES[0]);
    f.state.slots.insert((f.reviewed.hook, f.key), f.expected);
    f.state
        .slots
        .retain(|(a, s), _| *a != f.reviewed.pool || *s == U256::from(3));
    assert!(f.derive().unwrap().is_empty());
}

#[test]
fn initial_transaction_fee_never_replaces_raw_block_fee() {
    for capture in CAPTURES {
        let mut f = fixture(capture);
        let key = (
            f.reviewed.hook,
            solidity_address_mapping_key(f.reviewed.pool, 3),
        );
        let original = f.state.slots[&key];
        for initial in [0_u32, 1, 50_000, 0xff_ffff] {
            let mask = U256::from(u32::MAX) << 112_usize;
            f.state.slots.insert(
                key,
                (original & !mask)
                    | (U256::from(1) << 112_usize)
                    | (U256::from(initial) << 120_usize),
            );
            assert_eq!(
                f.derive().unwrap(),
                vec![StateUpdate::slot(f.reviewed.hook, f.key, f.expected)]
            );
        }
    }
}

#[test]
fn hook_identity_binding_and_canonical_context_fail_closed() {
    for capture in CAPTURES {
        let f = fixture(capture);
        for identity in f.state.identities.keys() {
            let mut missing = fixture(capture);
            missing.state.identities.remove(identity);
            assert!(missing.derive().is_err());
        }
        let mut wrong = fixture(capture);
        wrong.state.slots.insert(
            (
                f.reviewed.factory,
                solidity_address_mapping_key(f.reviewed.pool, 10),
            ),
            U256::from(1),
        );
        assert!(wrong.derive().is_err());
        wrong = fixture(capture);
        wrong.context.parent_hash = None;
        assert!(wrong.derive().is_err());
    }
}

fn signed_word(value: i64, bits: u32) -> U256 {
    U256::from(value as u64) & ((U256::from(1) << bits) - U256::from(1))
}
fn ring(state: &mut Parent, pool: Address, index: u16, card: u16, tick: i64) {
    state.slots.insert(
        (pool, U256::from(6)),
        (signed_word(tick, 24) << 160_usize)
            | (U256::from(index) << 184_usize)
            | (U256::from(card) << 200_usize),
    );
}
fn observation(state: &mut Parent, pool: Address, index: u16, timestamp: u32, cumulative: i64) {
    state.slots.insert(
        (pool, U256::from(17 + u32::from(index))),
        U256::from(timestamp)
            | (signed_word(cumulative, 56) << 32_usize)
            | (U256::from(1) << 248_usize),
    );
}
#[test]
fn negative_twap_rounds_down_and_interpolation_divides_before_multiplication() {
    let pool = Address::ZERO;
    let mut state = Parent::default();
    ring(&mut state, pool, 1, 2, 0);
    observation(&mut state, pool, 0, 100, 0);
    observation(&mut state, pool, 1, 700, -601);
    assert_eq!(oracle::tick_distance(pool, &state, 700, 600).unwrap(), 2);
    observation(&mut state, pool, 1, 700, 601);
    // target=101; source interpolates 601/600*1=1, then600/599 rounds1.
    assert_eq!(oracle::tick_distance(pool, &state, 700, 599).unwrap(), 1);
}
#[test]
fn maximum_uint16_ring_search_is_bounded_and_missing_cells_reject() {
    let pool = Address::ZERO;
    let mut state = Parent::default();
    ring(&mut state, pool, 65_534, 65_535, 7);
    // This is a complete deterministic model ring, not canonical unknown cells.
    for i in 0..65_535_u16 {
        observation(&mut state, pool, i, u32::from(i) + 100, i64::from(i) * 7);
    }
    for ago in [1, 2, 600, 32_767, 65_534] {
        assert_eq!(oracle::tick_distance(pool, &state, 65_634, ago).unwrap(), 0);
    }
    state.slots.remove(&(pool, U256::from(17 + 32_767)));
    assert!(oracle::tick_distance(pool, &state, 65_634, 600).is_err());
}
#[test]
fn oracle_geometry_oldest_underflow_and_missing_history_reject() {
    let pool = Address::ZERO;
    let mut state = Parent::default();
    ring(&mut state, pool, 0, 1, 0);
    observation(&mut state, pool, 0, 101, 0);
    assert!(oracle::tick_distance(pool, &state, 100, 1).is_err());
    ring(&mut state, pool, 1, 1, 0);
    assert!(oracle::tick_distance(pool, &state, 200, 1).is_err());
    ring(&mut state, pool, 0, 1, 0);
    state.slots.remove(&(pool, U256::from(17)));
    assert!(oracle::tick_distance(pool, &state, 200, 1).is_err());
}

#[test]
fn rotated_small_rings_match_independent_chronological_twap() {
    let pool = Address::ZERO;
    for cardinality in 2_u16..32 {
        for index in 0..cardinality {
            let mut state = Parent::default();
            ring(&mut state, pool, index, cardinality, 27);
            let cumulative = |i: i64| i * i - 10 * i;
            for i in 0..cardinality {
                let position = (index + 1 + i) % cardinality;
                observation(
                    &mut state,
                    pool,
                    position,
                    100 + u32::from(i) * 10,
                    cumulative(i64::from(i)),
                );
            }
            let newest_time = 100 + u32::from(cardinality - 1) * 10;
            let newest_tick = cumulative(i64::from(cardinality - 1));
            let now = newest_time + 3;
            let current = newest_tick + 27 * 3;
            for target in 100..now {
                let historical = if target >= newest_time {
                    newest_tick + 27 * i64::from(target - newest_time)
                } else {
                    let i = i64::from((target - 100) / 10);
                    let remainder = i64::from((target - 100) % 10);
                    cumulative(i) + (cumulative(i + 1) - cumulative(i)) / 10 * remainder
                };
                let lookback = now - target;
                let average = (current - historical).div_euclid(i64::from(lookback));
                assert_eq!(
                    oracle::tick_distance(pool, &state, now, lookback).unwrap(),
                    (27 - average).unsigned_abs() as u32
                );
            }
        }
    }
}

#[test]
fn growing_ring_skips_known_uninitialized_entries_without_inventing_values() {
    let pool = Address::ZERO;
    let mut state = Parent::default();
    ring(&mut state, pool, 2, 5, 4);
    for i in 0..3 {
        observation(
            &mut state,
            pool,
            i,
            100 + u32::from(i) * 100,
            i64::from(i) * 400,
        );
    }
    for i in 3..5 {
        state
            .slots
            .insert((pool, U256::from(17 + i)), U256::from(1));
    }
    assert_eq!(oracle::tick_distance(pool, &state, 310, 160).unwrap(), 0);
    state.slots.remove(&(pool, U256::from(20)));
    assert!(oracle::tick_distance(pool, &state, 310, 160).is_err());
}
