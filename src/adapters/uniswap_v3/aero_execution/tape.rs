//! Tape writes proved by events, never inferred from a pool `Swap`.
//!
//! `record`'s silent accumulator writes are deliberately unsupported until a
//! real input adapter can prove its successful call and exact call-time inputs.

use super::{AdapterEventError, ReviewedAeroClRuntime, StateUpdate, StateView, read, unsupported};
use crate::adapters::uniswap_v3::solidity_address_mapping_key;
use alloy_primitives::{Log, U256};
use alloy_sol_types::SolEvent;

alloy_sol_types::sol! {
    event DefaultCadenceIntervalSet(uint32 cadence);
    event PoolCadenceSet(address indexed pool, uint32 cadence);
    event VolatilityRecorded(address indexed pool, uint48 volatility, uint8 nOver);
}

const BUFFER_METADATA_OFFSET: u64 = 65_535 * 6;

pub(super) fn derive(
    reviewed: ReviewedAeroClRuntime,
    log: &Log,
    state: &dyn StateView,
) -> Result<Vec<StateUpdate>, AdapterEventError> {
    if state.verified_code_hash(reviewed.chain_id, reviewed.pool_tape.0)
        != Some(reviewed.pool_tape.1)
    {
        return Err(unsupported("unattested Aero CL tape runtime"));
    }
    match log.topics().first().copied() {
        Some(DefaultCadenceIntervalSet::SIGNATURE_HASH) => {
            let event = canonical::<DefaultCadenceIntervalSet>(log)?;
            cadence(reviewed, state, U256::from(1), event.cadence)
        }
        Some(PoolCadenceSet::SIGNATURE_HASH) => {
            let event = canonical::<PoolCadenceSet>(log)?;
            if event.pool != reviewed.pool {
                return Err(unsupported("Aero CL tape event names a different pool"));
            }
            cadence(
                reviewed,
                state,
                solidity_address_mapping_key(reviewed.pool, 2),
                event.cadence,
            )
        }
        Some(VolatilityRecorded::SIGNATURE_HASH) => {
            let event = canonical::<VolatilityRecorded>(log)?;
            if event.pool != reviewed.pool {
                return Err(unsupported("Aero CL tape event names a different pool"));
            }
            volatility(reviewed, state, event.volatility.to::<u64>(), event.nOver)
        }
        _ => Err(unsupported(
            "Aero CL tape mutation requires additional execution evidence",
        )),
    }
}

fn canonical<E: SolEvent>(log: &Log) -> Result<E, AdapterEventError> {
    let decoded = E::decode_log_data_validate(&log.data)
        .map_err(|_| AdapterEventError::MalformedLog("Aero CL tape event"))?;
    if decoded.encode_log_data() != log.data {
        return Err(AdapterEventError::MalformedLog(
            "noncanonical Aero CL tape event",
        ));
    }
    Ok(decoded)
}

fn replace(word: U256, value: U256, shift: usize, bits: usize) -> U256 {
    let mask = ((U256::from(1) << bits) - U256::from(1)) << shift;
    (word & !mask) | (value << shift)
}

fn cadence(
    reviewed: ReviewedAeroClRuntime,
    state: &dyn StateView,
    slot: U256,
    value: u32,
) -> Result<Vec<StateUpdate>, AdapterEventError> {
    // The source resolves a zero per-pool argument to the nonzero default
    // before emitting. A zero event therefore contradicts the reviewed code.
    if value == 0 {
        return Err(unsupported("Aero CL tape emitted a zero cadence"));
    }
    let word = read(state, reviewed.pool_tape.0, slot)?;
    Ok(vec![StateUpdate::slot(
        reviewed.pool_tape.0,
        slot,
        replace(word, U256::from(value), 160, 32),
    )])
}

fn volatility(
    reviewed: ReviewedAeroClRuntime,
    state: &dyn StateView,
    value: u64,
    n_over: u8,
) -> Result<Vec<StateUpdate>, AdapterEventError> {
    let address = reviewed.pool_tape.0;
    let buffer = solidity_address_mapping_key(reviewed.pool, 4);
    let metadata = read(
        state,
        address,
        buffer.wrapping_add(U256::from(BUFFER_METADATA_OFFSET)),
    )?;
    let index = (metadata & U256::from(u16::MAX)).to::<u16>();
    if index == u16::MAX {
        return Err(unsupported(
            "Aero CL tape observation index exceeds fixed array",
        ));
    }
    // No cardinality precondition is added: recordVolatility also permits an
    // uninitialized buffer's index0; exact parent cells must still be present.
    let observation = buffer.wrapping_add(U256::from(u64::from(index) * 6 + 1));
    let accumulator = solidity_address_mapping_key(reviewed.pool, 5);
    let count = accumulator.wrapping_add(U256::from(3));
    let observation_word = read(state, address, observation)?;
    let accumulator_word = read(state, address, accumulator)?;
    let count_word = read(state, address, count)?;
    Ok(vec![
        StateUpdate::slot(
            address,
            observation,
            replace(observation_word, U256::from(value), 160, 48),
        ),
        StateUpdate::slot(
            address,
            accumulator,
            replace(accumulator_word, U256::from(value), 208, 48),
        ),
        StateUpdate::slot(
            address,
            count,
            replace(count_word, U256::from(n_over), 240, 8),
        ),
    ])
}

#[cfg(test)]
mod tests;
