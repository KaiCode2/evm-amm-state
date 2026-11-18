use super::*;
use crate::adapters::SlipstreamRuntimeFamily;
use crate::adapters::uniswap_v3::reviewed_aero_cl_runtime;
use alloy_primitives::{B256, Bytes};
use std::collections::BTreeMap;

const CAPTURE: &str =
    include_str!("../../../../../tests/fixtures/aero_execution/base-voter-rpc.json");

#[derive(Clone, Default)]
struct Parent {
    words: BTreeMap<U256, U256>,
    code: BTreeMap<Address, B256>,
}

impl StateView for Parent {
    fn storage(&self, _: Address, slot: U256) -> Option<U256> {
        self.words.get(&slot).copied()
    }
    fn verified_code_hash(&self, _: u64, address: Address) -> Option<B256> {
        self.code.get(&address).copied()
    }
}

impl Parent {
    fn apply(&mut self, updates: Vec<StateUpdate>) {
        for update in updates {
            let StateUpdate::Slot { slot, value, .. } = update else {
                panic!("absolute known writes only")
            };
            self.words.insert(slot, value);
        }
    }
}

fn parse<T: std::str::FromStr>(value: &serde_json::Value) -> T
where
    T::Err: std::fmt::Debug,
{
    value.as_str().unwrap().parse().unwrap()
}

fn runtime() -> ReviewedAeroClRuntime {
    reviewed_aero_cl_runtime(SlipstreamRuntimeFamily::AeroBaseMooBifi).unwrap()
}

fn fixture() -> (Parent, BTreeMap<U256, U256>, Vec<Log>, AdapterEventContext) {
    let capture: serde_json::Value = serde_json::from_str(CAPTURE).unwrap();
    let r = &capture["reference"];
    let requests = capture["requests"].as_array().unwrap();
    assert_eq!(
        requests.last().unwrap()["response"]["result"]["hash"],
        r["block_hash"]
    );
    let mut parent = Parent::default();
    let mut post = BTreeMap::new();
    for request in requests {
        if request["method"] == "eth_getCode" {
            let code: Bytes = parse(&request["response"]["result"]);
            parent
                .code
                .insert(parse(&request["params"][0]), keccak256(code));
        } else if request["method"] == "eth_getStorageAt" {
            let key = parse(&request["params"][1]);
            let word = parse(&request["response"]["result"]);
            if request["params"][2]["blockHash"] == r["parent_hash"] {
                parent.words.insert(key, word);
            } else {
                post.insert(key, word);
            }
        }
    }
    let logs = capture["logs_request"]["response"]["result"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|log| {
            assert_eq!(log["blockHash"], r["block_hash"]);
            let topics: Vec<B256> = log["topics"]
                .as_array()
                .unwrap()
                .iter()
                .map(parse)
                .collect();
            [
                GaugeCapSettled::SIGNATURE_HASH,
                EmissionsMinted::SIGNATURE_HASH,
                EmissionsForfeited::SIGNATURE_HASH,
            ]
            .contains(&topics[0])
            .then(|| Log::new_unchecked(parse(&log["address"]), topics, parse(&log["data"])))
        })
        .collect();
    let context = AdapterEventContext::for_block(
        r["block_number"].as_u64().unwrap(),
        parse(&r["block_hash"]),
        r["block_timestamp"].as_u64().unwrap(),
    )
    .with_chain_id(8453)
    .with_parent_hash(parse(&r["parent_hash"]));
    (parent, post, logs, context)
}

#[test]
fn canonical_full_block_voter_events_match_all_recorded_post_cells() {
    let (mut parent, post, logs, context) = fixture();
    assert!(logs.len() > 10, "includes other-gauge global accounting");
    for log in &logs {
        parent.apply(derive(runtime(), log, &parent, &context).unwrap());
    }
    for (key, expected) in post {
        assert_eq!(parent.words.get(&key), Some(&expected), "slot {key:#x}");
    }
}

#[test]
fn missing_parent_and_unattested_library_reject_without_updates() {
    let (mut parent, _, logs, context) = fixture();
    parent.words.remove(&U256::from(2));
    assert!(matches!(
        derive(runtime(), &logs[0], &parent, &context),
        Err(AdapterEventError::MissingState { .. })
    ));
    let (mut parent, _, logs, context) = fixture();
    parent.code.remove(&LIBRARY);
    assert!(derive(runtime(), &logs[0], &parent, &context).is_err());
}

#[test]
fn target_cursor_weight_and_missing_expiry_schedule_fail_closed() {
    let (parent, _, logs, context) = fixture();
    let log = logs
        .iter()
        .find(|log| {
            canonical::<GaugeCapSettled>(log).is_ok_and(|event| event.gauge == runtime().gauge.0)
        })
        .unwrap();
    let mut wrong = parent.clone();
    let base = solidity_address_mapping_key(runtime().gauge.0, 8);
    let point = base.wrapping_add(U256::from(4));
    wrong.words.insert(point, U256::ZERO);
    assert!(derive(runtime(), log, &wrong, &context).is_err());
    let mut later = context;
    later.block_timestamp = Some((context.block_timestamp.unwrap() / WEEK + 1) * WEEK + 1);
    assert!(matches!(
        derive(runtime(), log, &parent, &later),
        Err(AdapterEventError::MissingState { .. })
    ));
    later.block_timestamp = Some(context.block_timestamp.unwrap() + 3 * WEEK);
    assert!(derive(runtime(), log, &parent, &later).is_err());
}

#[test]
fn known_expiry_boundary_reduces_slope_and_creates_exact_global_checkpoints() {
    let (mut parent, _, _, mut context) = fixture();
    let reviewed = runtime();
    let base = solidity_address_mapping_key(reviewed.gauge.0, 8);
    let start = WEEK + 1;
    let end = 2 * WEEK + 1;
    // Explicit model baseline for an expiring allocation, not a canonical
    // fixture augmentation: every input below is supplied and known.
    parent.words.extend([
        (U256::from(2), U256::from(1)),
        (U256::from(3), U256::ZERO),
        (U256::from(4), U256::ZERO),
        (U256::from(5), U256::from(start)),
        (U256::from(15), U256::ZERO),
    ]);
    for i in 0..6 {
        parent
            .words
            .insert(base.wrapping_add(U256::from(i)), U256::ZERO);
    }
    parent.words.insert(base, U256::from(10));
    parent.words.insert(
        base + U256::from(1),
        U256::from(start) | (U256::from(1) << 48_usize),
    );
    parent.words.insert(
        base + U256::from(4),
        U256::from(10 * (WEEK - 1)) | (U256::from(10) << 128_usize),
    );
    parent.words.insert(
        base + U256::from(5),
        U256::from(start) | (U256::from(500) << 48_usize),
    );
    parent.words.insert(
        mapping(
            U256::from(2 * WEEK),
            solidity_address_mapping_key(reviewed.gauge.0, 9),
        ),
        U256::from(10),
    );
    context.block_timestamp = Some(end);
    let log = Log {
        address: reviewed.voter,
        data: GaugeCapSettled {
            gauge: reviewed.gauge.0,
            effectiveShare: 0,
            surplusDelta: 0,
            cap: u128::MAX,
            gaugeWeight: U256::from(500),
        }
        .encode_log_data(),
    };
    parent.apply(derive(reviewed, &log, &parent, &context).unwrap());
    assert_eq!(parent.words[&(base + U256::from(4))], U256::ZERO);
    assert_eq!(parent.words[&U256::from(3)], U256::from(end - start));
    assert_eq!(
        parent.words[&mapping(U256::from(2 * WEEK), U256::from(6))],
        U256::from(WEEK - 1)
    );
    assert_eq!(
        parent.words[&U256::from(4)],
        U256::from(end) * U256::from(end) - U256::from(start) * U256::from(start)
    );
    assert!(
        derive(reviewed, &log, &parent, &context).is_err(),
        "duplicate target settlement is not valid"
    );
}

#[test]
fn allocation_and_admin_events_require_explicit_baseline_repair() {
    let (parent, _, _, context) = fixture();
    for signature in [
        "ChainStatusSet(uint8)",
        "ChainAllocated(uint256,uint128,uint128,uint256)",
        "GaugesAllocated(uint256,(address,uint128,bytes)[],uint256)",
        "GaugeRegistered(address,bool,uint48)",
    ] {
        let log = Log::new_unchecked(runtime().voter, vec![keccak256(signature)], Bytes::new());
        assert!(derive(runtime(), &log, &parent, &context).is_err());
    }
}
