//! Additional execution-accounting transitions for reviewed Aero CL deployments.
//!
//! Quote-state reducers remain separate. Success here proves only the named
//! event's returned cells, not completeness of a full executor footprint.

use super::{
    AdapterEventContext, AdapterEventError, ConcentratedLiquidityAdapter, PoolRegistration,
    ProtocolId, ReviewedAeroClRuntime, StateUpdate, StateView, UnsupportedReason,
    reviewed_aero_cl_runtime, validate_reviewed_slipstream_event,
};
use alloy_primitives::{Address, Log, U256};

mod block_fee;
mod gauge;
mod tape;
mod voter;

impl ConcentratedLiquidityAdapter {
    /// Derive the additional execution-accounting writes proven by an Aero CL
    /// event, using the exact state immediately before that ordered event.
    ///
    /// Supports raw first-swap hook fees, `GaugeSettled` and its gauge reward
    /// cursor, ordinary Base voter settlement, tape cadence setters and
    /// `VolatilityRecorded` with their exact packed-field updates.
    /// The existing event decoder separately owns the pool's reward accounting.
    /// Unsupported events return an error; an empty update list never means
    /// that tape, voter or token state is complete. The caller must establish
    /// canonical ordering, complete receipt delivery, runtime attestations and
    /// atomic application together with the corresponding pool transition.
    /// This method performs no provider reads.
    pub fn derive_aero_execution_event(
        pool: &PoolRegistration,
        log: &Log,
        state: &dyn StateView,
        context: &AdapterEventContext,
    ) -> Result<Vec<StateUpdate>, AdapterEventError> {
        let reviewed = reviewed(pool, state, context)?;
        if log.address == reviewed.pool {
            block_fee::derive(reviewed, log, state, context)
        } else if log.address == reviewed.voter {
            voter::derive(reviewed, log, state, context)
        } else if log.address == reviewed.pool_tape.0 {
            tape::derive(reviewed, log, state)
        } else {
            gauge::derive(reviewed, log, state)
        }
    }
}

fn reviewed(
    pool: &PoolRegistration,
    state: &dyn StateView,
    context: &AdapterEventContext,
) -> Result<ReviewedAeroClRuntime, AdapterEventError> {
    if pool.protocol() != ProtocolId::Slipstream {
        return Err(unsupported(
            "execution transition requires a reviewed Aero CL pool",
        ));
    }
    let address = pool
        .key
        .address()
        .ok_or_else(|| unsupported("missing pool address"))?;
    let layout = super::layout_for(pool).ok_or_else(|| unsupported("missing Aero CL layout"))?;
    let family = validate_reviewed_slipstream_event(address, layout, context)?;
    let reviewed = reviewed_aero_cl_runtime(family)
        .ok_or_else(|| unsupported("execution transition requires an Aero CL runtime"))?;
    for (account, hash) in [
        (reviewed.pool, reviewed.proxy_runtime_code_hash),
        (
            reviewed.implementation,
            reviewed.implementation_runtime_code_hash,
        ),
        reviewed.gauge,
        reviewed.gauge_implementation,
    ] {
        if state.verified_code_hash(reviewed.chain_id, account) != Some(hash) {
            return Err(unsupported("unattested Aero CL execution runtime"));
        }
    }
    let gauge = read(state, address, U256::from(3))?;
    if gauge != U256::from_be_slice(reviewed.gauge.0.as_slice()) {
        return Err(unsupported("Aero CL gauge binding changed"));
    }
    Ok(reviewed)
}

fn read(state: &dyn StateView, address: Address, slot: U256) -> Result<U256, AdapterEventError> {
    state
        .storage(address, slot)
        .ok_or(AdapterEventError::MissingState { address, slot })
}

fn unsupported(reason: &str) -> AdapterEventError {
    AdapterEventError::Unsupported(UnsupportedReason::Custom(reason.to_owned()))
}
