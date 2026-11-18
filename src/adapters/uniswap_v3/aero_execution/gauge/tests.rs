use super::*;
use crate::adapters::storage::V3StorageLayout;
use crate::adapters::uniswap_v3::reviewed_aero_cl_runtime;
use crate::adapters::{
    AdapterEventContext, ConcentratedLiquidityAdapter, PoolKey, PoolRegistration, ProtocolMetadata,
    SlipstreamRuntimeFamily, V3Metadata,
};
use alloy_primitives::{Address, B256, Bytes, LogData, keccak256};
use std::collections::BTreeMap;

const CAPTURE: &str =
    include_str!("../../../../../tests/fixtures/aero_execution/base-gauge-cursor-rpc.json");

#[derive(Clone, Default)]
struct Parent {
    slots: BTreeMap<(Address, U256), U256>,
    identities: BTreeMap<(u64, Address), B256>,
}

impl StateView for Parent {
    fn storage(&self, address: Address, slot: U256) -> Option<U256> {
        self.slots.get(&(address, slot)).copied()
    }

    fn verified_code_hash(&self, chain_id: u64, address: Address) -> Option<B256> {
        self.identities.get(&(chain_id, address)).copied()
    }
}

fn value<T: std::str::FromStr>(v: &serde_json::Value) -> T
where
    T::Err: std::fmt::Debug,
{
    v.as_str().unwrap().parse().unwrap()
}

fn fixture() -> (PoolRegistration, Log, Parent, AdapterEventContext, U256) {
    let capture: serde_json::Value = serde_json::from_str(CAPTURE).unwrap();
    let reviewed = reviewed_aero_cl_runtime(SlipstreamRuntimeFamily::AeroBaseMooBifi).unwrap();
    let registration = PoolRegistration::new(PoolKey::Slipstream(reviewed.pool)).with_metadata(
        ProtocolMetadata::Slipstream(
            V3Metadata::default()
                .with_tick_spacing(100)
                .with_storage_layout(V3StorageLayout::aero_cl(100)),
        ),
    );
    let requests = &capture["requests"];
    let reference = &capture["reference"];
    let logs = requests[6]["response"]["result"].as_array().unwrap();
    assert_eq!(
        logs.len(),
        1,
        "capture covers the complete block's gauge settlements"
    );
    let raw = &logs[0];
    assert_eq!(raw["blockHash"], reference["block_hash"]);
    assert_eq!(raw["transactionHash"], reference["transaction_hash"]);
    assert_eq!(
        requests[7]["response"]["result"]["hash"],
        reference["block_hash"]
    );
    let context = AdapterEventContext::for_block(
        reference["block_number"].as_u64().unwrap(),
        value(&reference["block_hash"]),
        reference["block_timestamp"].as_u64().unwrap(),
    )
    .with_chain_id(8453)
    .with_parent_hash(value(&reference["parent_hash"]))
    .with_transaction_hash(value(&reference["transaction_hash"]))
    .with_event_order(reference["transaction_index"].as_u64().unwrap(), 641);
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
    for (address, hash) in [
        (reviewed.pool, reviewed.proxy_runtime_code_hash),
        (
            reviewed.implementation,
            reviewed.implementation_runtime_code_hash,
        ),
        reviewed.gauge,
        reviewed.gauge_implementation,
    ] {
        state.identities.insert((8453, address), hash);
    }
    for (index, expected) in [(4, reviewed.gauge.1), (5, reviewed.gauge_implementation.1)] {
        let code: Bytes = value(&requests[index]["response"]["result"]);
        assert_eq!(keccak256(code), expected);
    }
    state.slots.insert(
        (reviewed.pool, U256::from(3)),
        U256::from_be_slice(reviewed.gauge.0.as_slice()),
    );
    state.slots.insert(
        (reviewed.gauge.0, CURSOR_SLOT),
        value(&requests[2]["response"]["result"]),
    );
    (
        registration,
        log,
        state,
        context,
        value(&requests[3]["response"]["result"]),
    )
}

#[test]
fn canonical_base_gauge_cursor_matches_captured_post_state() {
    let (pool, log, state, context, post) = fixture();
    let updates =
        ConcentratedLiquidityAdapter::derive_aero_execution_event(&pool, &log, &state, &context)
            .unwrap();
    assert_eq!(
        updates,
        vec![StateUpdate::slot(log.address, CURSOR_SLOT, post)]
    );
}

#[test]
fn missing_parent_cursor_is_not_assumed_zero() {
    let (pool, log, mut state, context, _) = fixture();
    state.slots.remove(&(log.address, CURSOR_SLOT));
    assert_eq!(
        ConcentratedLiquidityAdapter::derive_aero_execution_event(&pool, &log, &state, &context),
        Err(AdapterEventError::MissingState {
            address: log.address,
            slot: CURSOR_SLOT
        })
    );
}

#[test]
fn duplicate_stale_and_overflowing_cursors_reject() {
    let (pool, log, state, context, post) = fixture();
    for cursor in [post, post - U256::from(1), U256::MAX] {
        let mut wrong = state.clone();
        wrong.slots.insert((log.address, CURSOR_SLOT), cursor);
        assert!(
            ConcentratedLiquidityAdapter::derive_aero_execution_event(
                &pool, &log, &wrong, &context
            )
            .is_err()
        );
    }
}

#[test]
fn identities_binding_and_canonical_context_are_required() {
    let (pool, log, state, context, _) = fixture();
    for key in state.identities.keys() {
        let mut wrong = state.clone();
        wrong.identities.remove(key);
        assert!(
            ConcentratedLiquidityAdapter::derive_aero_execution_event(
                &pool, &log, &wrong, &context
            )
            .is_err()
        );
    }
    let mut wrong = state.clone();
    wrong
        .slots
        .insert((pool.key.address().unwrap(), U256::from(3)), U256::ZERO);
    assert!(
        ConcentratedLiquidityAdapter::derive_aero_execution_event(&pool, &log, &wrong, &context)
            .is_err()
    );
    let mut missing_parent = context;
    missing_parent.parent_hash = None;
    for wrong in [missing_parent, context.with_chain_id(1)] {
        assert!(
            ConcentratedLiquidityAdapter::derive_aero_execution_event(&pool, &log, &state, &wrong)
                .is_err()
        );
    }
}

#[test]
fn zero_delta_wrong_emitter_and_trailing_data_reject() {
    let (pool, log, state, context, post) = fixture();
    let mut extra = log.data.data.to_vec();
    extra.extend_from_slice(&[0; 32]);
    let mut wrong_emitter = log.clone();
    wrong_emitter.address = pool.key.address().unwrap();
    for wrong in [
        wrong_emitter,
        Log {
            address: log.address,
            data: GaugeSettled {
                cumulativeRewardShare: post,
                delta: U256::ZERO,
            }
            .encode_log_data(),
        },
        Log {
            address: log.address,
            data: LogData::new(log.topics().to_vec(), extra.into()).unwrap(),
        },
    ] {
        assert!(
            ConcentratedLiquidityAdapter::derive_aero_execution_event(
                &pool, &wrong, &state, &context
            )
            .is_err()
        );
    }
}
