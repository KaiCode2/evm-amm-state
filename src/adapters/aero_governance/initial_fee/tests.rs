use super::*;
use crate::adapters::storage::V3StorageLayout;
use crate::adapters::{ProtocolMetadata, V3Metadata};
use alloy_primitives::{address, aliases::U24};
use std::collections::BTreeMap;

#[derive(Default)]
struct State(BTreeMap<(Address, U256), U256>);
impl StateView for State {
    fn storage(&self, address: Address, slot: U256) -> Option<U256> {
        self.0.get(&(address, slot)).copied()
    }
}

fn fixture() -> (
    PoolRegistration,
    AeroClGovernanceRuntime,
    State,
    AdapterEventContext,
) {
    let pool = address!("eebe78d734dc4bac027061cee9bc12c5803fe4f8");
    let runtime = aero_cl_governance_runtime_for_pool(pool).unwrap();
    let registration = PoolRegistration::new(PoolKey::Slipstream(pool)).with_metadata(
        ProtocolMetadata::Slipstream(
            V3Metadata::default()
                .with_tick_spacing(100)
                .with_storage_layout(V3StorageLayout::aero_cl(100)),
        ),
    );
    let mut state = State::default();
    state
        .0
        .insert((runtime.hook, config_key(runtime)), U256::MAX);
    let context = AdapterEventContext::for_block(53_203_953, B256::repeat_byte(1), 1_793_197_253)
        .with_chain_id(8453)
        .with_parent_hash(B256::repeat_byte(2))
        .with_transaction_hash(B256::repeat_byte(3))
        .with_event_order(1, 1);
    (registration, runtime, state, context)
}

fn log<E: SolEvent>(runtime: AeroClGovernanceRuntime, event: E) -> Log {
    Log {
        address: runtime.hook,
        data: event.encode_log_data(),
    }
}

fn exact(
    registration: &PoolRegistration,
    runtime: AeroClGovernanceRuntime,
    state: &State,
    context: &AdapterEventContext,
    log: &Log,
) -> U256 {
    let result = decode_event(registration, log, state, context).unwrap();
    assert!(result.error.is_none(), "{:?}", result.error);
    let event = result.event.unwrap();
    assert_eq!(event.quality, UpdateQuality::Exact);
    assert_eq!(event.repair, RepairAction::None);
    let [
        StateUpdate::Slot {
            address,
            slot,
            value,
        },
    ] = event.updates.as_slice()
    else {
        panic!("one exact config write")
    };
    assert_eq!((*address, *slot), (runtime.hook, config_key(runtime)));
    *value
}

#[test]
fn initial_fee_set_preserves_every_unrelated_bit_and_uses_full_bool_byte() {
    let (registration, runtime, state, context) = fixture();
    for fee in [0_u32, 50_000, 0xff_ffff] {
        let event = log(
            runtime,
            abi::InitialFeeSet {
                pool: runtime.pool,
                fee: U24::from(fee),
            },
        );
        let result = exact(&registration, runtime, &state, &context, &event);
        assert_eq!(
            (result >> 112_usize) & MASK_32,
            U256::from(1) | (U256::from(fee) << 8_usize)
        );
        assert_eq!(
            result & !(MASK_32 << 112_usize),
            U256::MAX & !(MASK_32 << 112_usize)
        );
    }
}

#[test]
fn disable_and_reset_preserve_source_defined_fields() {
    let (registration, runtime, state, context) = fixture();
    let disabled = exact(
        &registration,
        runtime,
        &state,
        &context,
        &log(runtime, abi::InitialFeeDisabled { pool: runtime.pool }),
    );
    assert_eq!(disabled, U256::MAX & !(MASK_32 << 112_usize));
    let reset = exact(
        &registration,
        runtime,
        &state,
        &context,
        &log(runtime, abi::DynamicFeeReset { pool: runtime.pool }),
    );
    let cleared = ((U256::from(1) << 120_usize) - U256::from(1)) << 24_usize;
    assert_eq!(reset, U256::MAX & !cleared);
    assert_eq!(reset & MASK_24, MASK_24, "base fee survives reset");
}

#[test]
fn missing_parent_and_unreviewed_abi_keep_invalidation() {
    let (registration, runtime, _, context) = fixture();
    let result = decode_event(
        &registration,
        &log(runtime, abi::InitialFeeDisabled { pool: runtime.pool }),
        &State::default(),
        &context,
    )
    .unwrap();
    assert!(matches!(
        result.error,
        Some(AdapterEventError::MissingState { .. })
    ));
    assert_eq!(
        result.event.unwrap().repair,
        RepairAction::VerifySlots(vec![(runtime.hook, config_key(runtime))])
    );
    let result = decode_event(
        &registration,
        &log(
            runtime,
            abi_alt::InitialFeeSet {
                pool: runtime.pool,
                fee: U256::from(10),
            },
        ),
        &State::default(),
        &context,
    )
    .unwrap();
    assert_eq!(result.event.unwrap().quality, UpdateQuality::RequiresRepair);
}

#[test]
fn impossible_fee_wrong_pool_and_noncanonical_payload_reject() {
    let (registration, runtime, state, context) = fixture();
    for (pool, fee) in [(runtime.pool, 50_001), (Address::ZERO, 10)] {
        let result = decode_event(
            &registration,
            &log(
                runtime,
                abi::InitialFeeSet {
                    pool,
                    fee: U24::from(fee),
                },
            ),
            &state,
            &context,
        )
        .unwrap();
        assert!(result.error.is_some());
        assert_eq!(result.event.unwrap().quality, UpdateQuality::RequiresRepair);
    }
    let mut event = log(runtime, abi::InitialFeeDisabled { pool: runtime.pool });
    event.data.data = vec![0; 32].into();
    assert!(
        decode_event(&registration, &event, &state, &context)
            .unwrap()
            .error
            .is_some()
    );
}
