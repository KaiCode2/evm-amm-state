//! Raw block-wide hook fee from exact pre-swap controls and pool oracle.
//! Initial-transaction fees, caller discounts and MEV taxes never determine
//! this stored value. The canonical block owner supplies a proven new-height
//! zero before its first event, and applies this write before the pool update.

use super::{
    AdapterEventContext, AdapterEventError, ReviewedAeroClRuntime, StateUpdate, StateView, read,
    unsupported,
};
use crate::adapters::uniswap_v3::{
    Swap, solidity_address_mapping_key, solidity_i24_mapping_key, solidity_nested_mapping_key,
};
use alloy_primitives::{Log, U256};
use alloy_sol_types::SolEvent;

mod oracle;

pub(super) fn derive(
    reviewed: ReviewedAeroClRuntime,
    log: &Log,
    state: &dyn StateView,
    context: &AdapterEventContext,
) -> Result<Vec<StateUpdate>, AdapterEventError> {
    let event = Swap::decode_log_validate(log)
        .map_err(|_| AdapterEventError::MalformedLog("Aero first-swap fee event"))?;
    if event.data.encode_log_data() != log.data {
        return Err(AdapterEventError::MalformedLog(
            "noncanonical Aero first-swap fee event",
        ));
    }
    for (address, hash) in [
        (reviewed.hook, reviewed.hook_runtime_code_hash),
        (reviewed.factory, reviewed.factory_runtime_code_hash),
    ] {
        if state.verified_code_hash(reviewed.chain_id, address) != Some(hash) {
            return Err(unsupported("unattested Aero block-fee runtime"));
        }
    }
    let key = solidity_nested_mapping_key(
        reviewed.pool,
        U256::from(
            context
                .block_number
                .ok_or_else(|| unsupported("missing block-fee height"))?,
        ),
        4,
    );
    let current = read(state, reviewed.hook, key)?;
    if !current.is_zero() {
        return Ok(Vec::new());
    }
    if read(state, reviewed.pool, U256::ZERO)? != U256::from_be_slice(reviewed.factory.as_slice()) {
        return Err(unsupported("Aero block-fee pool factory changed"));
    }
    let custom_hook = read(
        state,
        reviewed.factory,
        solidity_address_mapping_key(reviewed.pool, 10),
    )?;
    let selected_hook = if custom_hook.is_zero() {
        read(state, reviewed.factory, U256::from(9))?
    } else {
        custom_hook
    };
    if selected_hook != U256::from_be_slice(reviewed.hook.as_slice()) {
        return Err(unsupported("Aero block-fee hook binding changed"));
    }
    let config = read(
        state,
        reviewed.hook,
        solidity_address_mapping_key(reviewed.pool, 3),
    )?;
    let base = config & U256::from(0xff_ffff);
    let tick_fee = || {
        read(
            state,
            reviewed.factory,
            solidity_i24_mapping_key(reviewed.tick_spacing, 15),
        )
    };
    let value = if base == U256::from(0xff_ffff) {
        tick_fee()?
    } else {
        let mut scaling = (config >> 48_usize) & U256::from(u64::MAX);
        let mut cap = (config >> 24_usize) & U256::from(0xff_ffff);
        if scaling.is_zero() {
            scaling = read(state, reviewed.hook, U256::ZERO)?;
            cap = read(state, reviewed.hook, U256::from(1))?;
        }
        if scaling > U256::from(1_000_000_000_000_000_000_u64)
            || cap.is_zero()
            || cap > U256::from(50_000)
        {
            return Err(unsupported("invalid Aero dynamic fee controls"));
        }
        let base = if base.is_zero() { tick_fee()? } else { base };
        let now = u32::try_from(
            context
                .block_timestamp
                .ok_or_else(|| unsupported("missing block-fee timestamp"))?,
        )
        .map_err(|_| unsupported("Aero block-fee timestamp exceeds uint32"))?;
        let lookback =
            (read(state, reviewed.hook, U256::from(2))? & U256::from(u32::MAX)).to::<u32>();
        if !(1..65_535).contains(&lookback) {
            return Err(unsupported("invalid Aero dynamic fee lookback"));
        }
        let delta = oracle::tick_distance(reviewed.pool, state, now, lookback)?;
        let dynamic = U256::from(delta)
            .checked_mul(scaling)
            .ok_or_else(|| unsupported("Aero dynamic fee multiplication overflow"))?
            / U256::from(1_000_000);
        base.checked_add(dynamic)
            .ok_or_else(|| unsupported("Aero block fee addition overflow"))?
            .min(cap)
    };
    Ok(vec![StateUpdate::slot(reviewed.hook, key, value)])
}

#[cfg(test)]
mod tests;
