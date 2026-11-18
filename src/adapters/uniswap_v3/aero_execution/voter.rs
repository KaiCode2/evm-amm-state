//! Base LeafVoter ordinary settlement from exact parent state and ordered logs.
//!
//! Allocation/registration/status mutations are deliberately not handled: they
//! revoke this capability until an explicitly repaired baseline is installed.
//! All gauges' settlement/forfeiture events are needed for shared accumulators.

use super::{
    AdapterEventContext, AdapterEventError, ReviewedAeroClRuntime, StateUpdate, StateView, read,
    unsupported,
};
use crate::adapters::uniswap_v3::solidity_address_mapping_key;
use alloy_primitives::{Address, Log, U256, address, b256, keccak256};
use alloy_sol_types::SolEvent;

mod accounting;
mod settlement;

alloy_sol_types::sol! {
    event GaugeCapSettled(address indexed gauge, uint128 effectiveShare, uint128 surplusDelta, uint128 cap, uint256 gaugeWeight);
    event EmissionsMinted(address indexed gauge, address[] recipients, uint128[] amounts);
    event EmissionsForfeited(address indexed gauge, uint128 amount);
}

const MASK_128: U256 = U256::from_limbs([u64::MAX, u64::MAX, 0, 0]);
const MASK_48: U256 = U256::from_limbs([(1_u64 << 48) - 1, 0, 0, 0]);
const WEEK: u64 = 604_800;
// Same retained-schedule horizon as the reviewed Base executor footprint.
const MAX_WEEKS: u64 = 2;
const LIBRARY: Address = address!("c7e262e848fca78c3c66ee3c9835e4e65e3e0398");

pub(super) fn derive(
    reviewed: ReviewedAeroClRuntime,
    log: &Log,
    state: &dyn StateView,
    context: &AdapterEventContext,
) -> Result<Vec<StateUpdate>, AdapterEventError> {
    if reviewed.chain_id != 8453
        || state.verified_code_hash(8453, reviewed.voter)
            != Some(b256!(
                "363145d181429cc6f7b7c55a7b332fedb5a06b44da5371acee8a94d34d140f35"
            ))
        || state.verified_code_hash(8453, LIBRARY)
            != Some(b256!(
                "3759e4a45455ad769a000e151c3423658493e9be84015da913ea992c4c0e2d8e"
            ))
    {
        return Err(unsupported("unattested Base LeafVoter execution runtime"));
    }
    // Bound dynamic ABI allocation before decoding the only array-bearing log.
    if log.data.data.len() > 4_256 {
        return Err(unsupported("LeafVoter event exceeds bounded payload"));
    }
    match log.topics().first().copied() {
        Some(GaugeCapSettled::SIGNATURE_HASH) => {
            settlement::derive(reviewed, canonical(log)?, state, context)
        }
        Some(EmissionsMinted::SIGNATURE_HASH) => accounting::mint(reviewed, canonical(log)?, state),
        Some(EmissionsForfeited::SIGNATURE_HASH) => {
            accounting::forfeit(reviewed, canonical(log)?, state)
        }
        _ => Err(unsupported(
            "LeafVoter mutation requires a new authoritative baseline",
        )),
    }
}

fn canonical<E: SolEvent>(log: &Log) -> Result<E, AdapterEventError> {
    let event = E::decode_log_data_validate(&log.data)
        .map_err(|_| AdapterEventError::MalformedLog("LeafVoter event"))?;
    if event.encode_log_data() != log.data {
        return Err(AdapterEventError::MalformedLog(
            "noncanonical LeafVoter event",
        ));
    }
    Ok(event)
}

fn arithmetic() -> AdapterEventError {
    unsupported("LeafVoter checked arithmetic failed")
}

fn mapping(key: U256, root: U256) -> U256 {
    let mut preimage = [0; 64];
    preimage[..32].copy_from_slice(&key.to_be_bytes::<32>());
    preimage[32..].copy_from_slice(&root.to_be_bytes::<32>());
    U256::from_be_slice(keccak256(preimage).as_slice())
}

fn gauge_words(
    reviewed: ReviewedAeroClRuntime,
    state: &dyn StateView,
) -> Result<(U256, [U256; 6]), AdapterEventError> {
    let base = solidity_address_mapping_key(reviewed.gauge.0, 8);
    let mut words = [U256::ZERO; 6];
    for (index, word) in words.iter_mut().enumerate() {
        *word = read(state, reviewed.voter, base.wrapping_add(U256::from(index)))?;
    }
    if ((words[1] >> 48_usize) & U256::from(255)) != U256::from(1) {
        return Err(unsupported("LeafVoter target gauge is not registered"));
    }
    Ok((base, words))
}

#[cfg(test)]
mod tests;
