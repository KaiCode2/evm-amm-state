use super::*;
use crate::adapters::SlipstreamRuntimeFamily;
use crate::adapters::uniswap_v3::reviewed_aero_cl_runtime;
use alloy_primitives::{Address, B256};
use std::collections::BTreeMap;

#[derive(Default)]
struct Parent {
    words: BTreeMap<U256, U256>,
    code: Option<B256>,
}

impl StateView for Parent {
    fn storage(&self, _: Address, slot: U256) -> Option<U256> {
        self.words.get(&slot).copied()
    }

    fn verified_code_hash(&self, _: u64, _: Address) -> Option<B256> {
        self.code
    }
}

fn runtime() -> ReviewedAeroClRuntime {
    reviewed_aero_cl_runtime(SlipstreamRuntimeFamily::AeroBaseMooBifi).unwrap()
}

fn event<E: SolEvent>(event: E) -> Log {
    Log {
        address: runtime().pool_tape.0,
        data: event.encode_log_data(),
    }
}

fn parent() -> Parent {
    Parent {
        code: Some(runtime().pool_tape.1),
        ..Parent::default()
    }
}

#[test]
fn cadence_setters_preserve_caller_owner_and_padding_bits() {
    let reviewed = runtime();
    let mut state = parent();
    let config = solidity_address_mapping_key(reviewed.pool, 2);
    let original = U256::MAX;
    state.words.insert(U256::from(1), original);
    state.words.insert(config, original);
    for (slot, log) in [
        (
            U256::from(1),
            event(DefaultCadenceIntervalSet { cadence: 60 }),
        ),
        (
            config,
            event(PoolCadenceSet {
                pool: reviewed.pool,
                cadence: 60,
            }),
        ),
    ] {
        let output = derive(reviewed, &log, &state).unwrap();
        let expected =
            (original & !(U256::from(u32::MAX) << 160_usize)) | (U256::from(60) << 160_usize);
        assert_eq!(
            output,
            vec![StateUpdate::slot(reviewed.pool_tape.0, slot, expected)]
        );
    }
}

#[test]
fn volatility_writes_exact_three_packed_fields_at_current_observation() {
    let reviewed = runtime();
    let mut state = parent();
    let buffer = solidity_address_mapping_key(reviewed.pool, 4);
    let accumulator = solidity_address_mapping_key(reviewed.pool, 5);
    let index = 65_534_u64;
    let observation = buffer.wrapping_add(U256::from(index * 6 + 1));
    let count = accumulator.wrapping_add(U256::from(3));
    state.words.insert(
        buffer.wrapping_add(U256::from(BUFFER_METADATA_OFFSET)),
        U256::from(index),
    );
    for key in [observation, accumulator, count] {
        state.words.insert(key, U256::MAX);
    }
    let log = event(VolatilityRecorded {
        pool: reviewed.pool,
        volatility: 42.try_into().unwrap(),
        nOver: 7,
    });
    let output = derive(reviewed, &log, &state).unwrap();
    assert_eq!(
        output,
        vec![
            StateUpdate::slot(
                reviewed.pool_tape.0,
                observation,
                replace(U256::MAX, U256::from(42), 160, 48)
            ),
            StateUpdate::slot(
                reviewed.pool_tape.0,
                accumulator,
                replace(U256::MAX, U256::from(42), 208, 48)
            ),
            StateUpdate::slot(
                reviewed.pool_tape.0,
                count,
                replace(U256::MAX, U256::from(7), 240, 8)
            ),
        ]
    );
    for key in [observation, accumulator, count] {
        let removed = state.words.remove(&key).unwrap();
        assert_eq!(
            derive(reviewed, &log, &state),
            Err(AdapterEventError::MissingState {
                address: reviewed.pool_tape.0,
                slot: key
            })
        );
        state.words.insert(key, removed);
    }
}

#[test]
fn unseen_cadence_identity_and_wrong_pool_fail_closed() {
    let reviewed = runtime();
    let mut state = parent();
    let log = event(DefaultCadenceIntervalSet { cadence: 60 });
    assert!(matches!(
        derive(reviewed, &log, &state),
        Err(AdapterEventError::MissingState { .. })
    ));
    state.words.insert(U256::from(1), U256::ZERO);
    state.code = None;
    assert!(derive(reviewed, &log, &state).is_err());
    state.code = Some(reviewed.pool_tape.1);
    let other = event(PoolCadenceSet {
        pool: Address::ZERO,
        cadence: 60,
    });
    assert!(derive(reviewed, &other, &state).is_err());
    assert!(
        derive(
            reviewed,
            &event(DefaultCadenceIntervalSet { cadence: 0 }),
            &state
        )
        .is_err()
    );
}

#[test]
fn swap_or_observation_log_never_attests_silent_tape_record() {
    let reviewed = runtime();
    // The presence/absence of ObservationRecorded cannot distinguish the
    // silent successful accumulator branch from a reverted capped call.
    let log = Log::new_unchecked(
        reviewed.pool_tape.0,
        vec![alloy_primitives::keccak256(
            b"ObservationRecorded(address,uint16,uint48)",
        )],
        vec![0; 96].into(),
    );
    assert!(derive(reviewed, &log, &parent()).is_err());
}

#[test]
fn fixed_ring_index_overflow_and_noncanonical_payload_reject() {
    let reviewed = runtime();
    let mut state = parent();
    let buffer = solidity_address_mapping_key(reviewed.pool, 4);
    state.words.insert(
        buffer.wrapping_add(U256::from(BUFFER_METADATA_OFFSET)),
        U256::from(u16::MAX),
    );
    let log = event(VolatilityRecorded {
        pool: reviewed.pool,
        volatility: 42.try_into().unwrap(),
        nOver: 7,
    });
    assert!(derive(reviewed, &log, &state).is_err());
    let mut trailing = log.data.data.to_vec();
    trailing.push(0);
    let malformed = Log::new_unchecked(log.address, log.topics().to_vec(), trailing.into());
    assert!(derive(reviewed, &malformed, &state).is_err());
}
