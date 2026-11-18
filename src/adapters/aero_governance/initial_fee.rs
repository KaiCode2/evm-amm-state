//! Source-verified initial-fee and dynamic-reset packed writes.
//!
//! Matched Base hook runtime198b45ce…ecbd compiler storage layout places
//! initialFeeEnabled at byte14 and uint24 initialFee at byte15 of config slot3.
//! The equivalent reviewed Ethereum hook shares this layout. Existing event
//! interests already include these setters; no new subscription or read occurs.

use super::*;

pub(super) fn derive(
    runtime: AeroClGovernanceRuntime,
    log: &Log,
    state: &dyn StateView,
) -> Option<Result<Transition, AdapterEventError>> {
    let topic = *log.topics().first()?;
    if ![
        abi::InitialFeeSet::SIGNATURE_HASH,
        abi::InitialFeeDisabled::SIGNATURE_HASH,
        abi::DynamicFeeReset::SIGNATURE_HASH,
    ]
    .contains(&topic)
    {
        return None;
    }
    Some(apply(runtime, log, state, topic))
}

fn apply(
    runtime: AeroClGovernanceRuntime,
    log: &Log,
    state: &dyn StateView,
    topic: B256,
) -> Result<Transition, AdapterEventError> {
    let (pool, clear, value) = if topic == abi::InitialFeeSet::SIGNATURE_HASH {
        let event = decode::<abi::InitialFeeSet>(log)?;
        let fee = event.fee.to::<u32>();
        if fee > 50_000 && fee != 0xff_ffff {
            return Err(failure("initial fee contradicts reviewed hook bounds"));
        }
        (
            event.pool,
            MASK_32 << 112_usize,
            (U256::from(1) << 112_usize) | (U256::from(fee) << 120_usize),
        )
    } else if topic == abi::InitialFeeDisabled::SIGNATURE_HASH {
        (
            decode::<abi::InitialFeeDisabled>(log)?.pool,
            MASK_32 << 112_usize,
            U256::ZERO,
        )
    } else {
        // resetDynamicFee preserves baseFee but deletes cap, scaling, bool and
        // initialFee: bytes3..18, i.e.120bits beginning at bit24.
        let mask = ((U256::from(1) << 120_usize) - U256::from(1)) << 24_usize;
        (decode::<abi::DynamicFeeReset>(log)?.pool, mask, U256::ZERO)
    };
    if pool != runtime.pool {
        return Err(failure("initial-fee event names a different pool"));
    }
    let slot = config_key(runtime);
    let parent = state
        .storage(runtime.hook, slot)
        .ok_or(AdapterEventError::MissingState {
            address: runtime.hook,
            slot,
        })?;
    Ok(Transition::Exact(vec![StateUpdate::slot(
        runtime.hook,
        slot,
        (parent & !clear) | value,
    )]))
}

#[cfg(test)]
mod tests;
