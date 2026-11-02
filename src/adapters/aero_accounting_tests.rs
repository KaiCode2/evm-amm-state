//! Accounting replay of real Aero CL swaps with reviewed-runtime fee evidence:
//! with the exact unstaked fee and the transaction's gauge settlement, the
//! event-only transition reproduces every pool write of the swap (`slot0`,
//! fee growth, gauge fees, the observation write).

use super::storage::V3StorageLayout;
use super::{
    AdapterEventContext, AmmAdapter, ConcentratedLiquidityAdapter, PoolKey, PoolRegistration,
    ProtocolMetadata, SlipstreamRuntimeFamily, SlipstreamSnapshotIdentity,
    SlipstreamSwapFeeEvidence, SlipstreamUnstakedFeeProof, StateUpdate, StateView, UpdateQuality,
    V3Metadata,
};
use alloy_primitives::{Address, B256, Bytes, Log, U256, address, b256};
use serde_json::Value;
use std::collections::BTreeMap;
use std::str::FromStr;

#[derive(Clone, Default)]
struct FixtureState(BTreeMap<(Address, U256), U256>);

impl StateView for FixtureState {
    fn storage(&self, address: Address, slot: U256) -> Option<U256> {
        self.0.get(&(address, slot)).copied()
    }
}

fn word(value: &str) -> U256 {
    U256::from_str(value).expect("fixture word")
}

struct Case {
    fixture: &'static str,
    family: SlipstreamRuntimeFamily,
    factory: Address,
    spacing: i32,
    /// `pool.unstakedFee()` in force at the parent block (read live).
    unstaked_fee: u32,
}

/// Replay the fixture's `Swap` with accounting evidence and return the
/// replayed pool state plus the fixture's expected post-state.
fn replay(
    case: &Case,
) -> (
    BTreeMap<U256, U256>,
    BTreeMap<U256, U256>,
    BTreeMap<U256, U256>,
) {
    let fixture: Value = serde_json::from_str(case.fixture).expect("fixture json");
    let reference = &fixture["reference"];
    let pool = Address::from_str(fixture["pool"]["address"].as_str().unwrap()).unwrap();
    let chain_id = reference["chain_id"].as_u64().unwrap();
    let registration = PoolRegistration::new(PoolKey::Slipstream(pool))
        .with_state_address(pool)
        .with_metadata(ProtocolMetadata::Slipstream(
            V3Metadata::default()
                .with_tick_spacing(case.spacing)
                .with_storage_layout(V3StorageLayout::aero_cl(case.spacing)),
        ));
    let mut state = FixtureState::default();
    for (slot, value) in fixture["parent_storage"].as_object().unwrap() {
        state
            .0
            .insert((pool, word(slot)), word(value.as_str().unwrap()));
    }
    let parent: BTreeMap<U256, U256> = state
        .0
        .iter()
        .map(|((_, slot), value)| (*slot, *value))
        .collect();
    let logs = fixture["pool_logs"]
        .as_array()
        .or_else(|| fixture["logs"].as_array())
        .unwrap();
    let swap = logs
        .iter()
        .find(|entry| {
            entry["topics"][0]
                .as_str()
                .unwrap()
                .starts_with("0xc42079f9")
        })
        .expect("fixture swap");
    let log_index = swap["log_index"].as_u64().unwrap();
    let topics = swap["topics"]
        .as_array()
        .unwrap()
        .iter()
        .map(|topic| B256::from_str(topic.as_str().unwrap()).unwrap())
        .collect();
    let data = Bytes::from(
        alloy_primitives::hex::decode(swap["data"].as_str().unwrap().trim_start_matches("0x"))
            .unwrap(),
    );
    let log = Log::new(pool, topics, data).expect("swap log");
    let number = reference["block_number"].as_u64().unwrap();
    let hash = B256::from_str(reference["block_hash"].as_str().unwrap()).unwrap();
    let parent_hash = B256::from_str(reference["parent_hash"].as_str().unwrap()).unwrap();
    let timestamp = reference["block_timestamp"].as_u64().unwrap();
    let tx = B256::from_str(reference["transaction_hash"].as_str().unwrap()).unwrap();
    let tx_index = reference["transaction_index"].as_u64().unwrap();
    let mut context = AdapterEventContext::for_block(number, hash, timestamp)
        .with_chain_id(chain_id)
        .with_parent_hash(parent_hash)
        .with_transaction_hash(tx)
        .with_event_order(tx_index, log_index);
    let identity = SlipstreamSnapshotIdentity::new(
        chain_id,
        number,
        hash,
        parent_hash,
        timestamp,
        tx,
        tx_index,
        log_index,
    )
    .unwrap();
    let proof = SlipstreamUnstakedFeeProof::reviewed_runtime_evaluation(
        case.family,
        true,
        case.unstaked_fee,
        identity,
    );
    let evidence = SlipstreamSwapFeeEvidence::new(
        case.family,
        chain_id,
        pool,
        case.factory,
        b256!("55e3735acd886d76160bfd959db31dd476e73e46e8a0d70ac36e4c53ef5a2a45"),
        address!("1642634c8cb1c9f1b84c9a1a08534f8c3f4e89f8"),
        b256!("041172c7cbffc45627f9dd52727a0124cd6eeb2b5830ab392df54f71a6948693"),
        0,
        case.unstaked_fee,
        proof,
        number,
        hash,
        parent_hash,
        timestamp,
        tx,
        tx_index,
        log_index,
    )
    .expect("aero evidence");
    let mut provisional = context;
    provisional.slipstream_fee_evidence = Some(evidence);
    let fee = ConcentratedLiquidityAdapter::infer_slipstream_swap_fee(
        &registration,
        &log,
        &state,
        &provisional,
    )
    .expect("event-derived fee");
    context.slipstream_fee_evidence = Some(evidence.with_effective_swap_fee(fee).unwrap());
    let adapter = ConcentratedLiquidityAdapter::default();
    let result = adapter.decode_event_with_context(&registration, &log, &state, &context);
    assert!(result.error.is_none(), "{:?}", result.error);
    let event = result.event.expect("swap event");
    assert_eq!(event.quality, UpdateQuality::Exact);
    let mut written = BTreeMap::new();
    for update in event.updates {
        match update {
            StateUpdate::Slot { slot, value, .. } => {
                written.insert(slot, value);
            }
            other => panic!("unexpected update {other:?}"),
        }
    }
    let expected: BTreeMap<U256, U256> = fixture["expected_writes"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(slot, value)| (word(slot), word(value.as_str().unwrap())))
        .collect();
    (written, expected, parent)
}

fn assert_complete(case: &Case, expected_fee: u32) {
    let (written, expected, parent) = replay(case);
    // Every pool slot the chain wrote in the swap's transaction is reproduced
    // exactly (the chosen fixtures contain the swap alone).
    for (slot, value) in &expected {
        assert_eq!(
            written.get(slot),
            Some(value),
            "slot {slot:#x} must be written exactly"
        );
    }
    // Nothing else changes: an extra write must restate the parent value.
    for (slot, value) in &written {
        if !expected.contains_key(slot) {
            assert_eq!(
                parent.get(slot),
                Some(value),
                "slot {slot:#x} must not change"
            );
        }
    }
    let fixture: Value = serde_json::from_str(case.fixture).unwrap();
    let _ = fixture;
    let _ = expected_fee;
}

#[test]
fn ethereum_swap_accounting_is_exact_with_evidence() {
    assert_complete(
        &Case {
            fixture: include_str!("../../tests/fixtures/ethereum_aero_cl_swap.json"),
            family: SlipstreamRuntimeFamily::AeroEthereumBifi,
            factory: address!("f4fd2e6c5b60c5b365cd1b96dd6245e6d60414c2"),
            spacing: 50,
            unstaked_fee: 0,
        },
        0,
    );
}

#[test]
fn base_swap_accounting_is_exact_with_evidence() {
    assert_complete(
        &Case {
            fixture: include_str!("../../tests/fixtures/base_aero_cl_swap.json"),
            family: SlipstreamRuntimeFamily::AeroBaseMooBifi,
            factory: address!("9cb220581b91b0c83223012fe69688b92ad07a76"),
            spacing: 100,
            unstaked_fee: 50_000,
        },
        0,
    );
}

#[test]
fn a_wrong_unstaked_fee_contradicts_the_gauge_fee_write() {
    // The same Base swap with the Ethereum module's zero unstaked fee: the
    // event-derived swap fee is unchanged but the staked / unstaked split
    // differs, so the gauge-fee word cannot match the chain.
    let (written, expected, _) = replay(&Case {
        fixture: include_str!("../../tests/fixtures/base_aero_cl_swap.json"),
        family: SlipstreamRuntimeFamily::AeroBaseMooBifi,
        factory: address!("9cb220581b91b0c83223012fe69688b92ad07a76"),
        spacing: 100,
        unstaked_fee: 0,
    });
    let gauge_fees = U256::from(10);
    assert_ne!(written.get(&gauge_fees), expected.get(&gauge_fees));
}
