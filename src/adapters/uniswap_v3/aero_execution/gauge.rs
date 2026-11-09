//! `CLGauge.settleGauge`: exact cursor transition, separately from pool rewards.

use super::{AdapterEventError, ReviewedAeroClRuntime, StateUpdate, StateView, read, unsupported};
use alloy_primitives::{Log, U256};
use alloy_sol_types::SolEvent;

alloy_sol_types::sol! {
    event GaugeSettled(uint256 cumulativeRewardShare, uint256 delta);
}

// Gauge inherits ReentrancyGuard (slot0), then votingRewardsManager/isPool
// (slot1). Sourcify's matched CLGauge compiler layout places this cursor at2.
const CURSOR_SLOT: U256 = U256::from_limbs([2, 0, 0, 0]);

pub(super) fn derive(
    reviewed: ReviewedAeroClRuntime,
    log: &Log,
    state: &dyn StateView,
) -> Result<Vec<StateUpdate>, AdapterEventError> {
    if log.address != reviewed.gauge.0
        || log.topics().first() != Some(&GaugeSettled::SIGNATURE_HASH)
    {
        return Err(unsupported(
            "Aero CL execution event is not a reviewed GaugeSettled",
        ));
    }
    let event = GaugeSettled::decode_log_validate(log)
        .map_err(|_| AdapterEventError::MalformedLog("Aero CL GaugeSettled"))?;
    if event.data.encode_log_data() != log.data {
        return Err(AdapterEventError::MalformedLog(
            "noncanonical Aero CL GaugeSettled",
        ));
    }
    let parent = read(state, reviewed.gauge.0, CURSOR_SLOT)?;
    if event.delta.is_zero() || parent.checked_add(event.delta) != Some(event.cumulativeRewardShare)
    {
        return Err(unsupported(
            "Aero CL gauge cumulative/delta contradicts parent cursor",
        ));
    }
    Ok(vec![StateUpdate::slot(
        reviewed.gauge.0,
        CURSOR_SLOT,
        event.cumulativeRewardShare,
    )])
}

#[cfg(test)]
mod tests;
