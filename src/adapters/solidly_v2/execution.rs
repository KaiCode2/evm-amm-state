//! Provider-free execution-cell reducers for the reviewed Optimism volatile pool.
//! Event delivery, runtime attestation and whole-transaction completeness belong
//! to the caller. No reducer fetches state or defaults an absent cell to zero.
mod governance;
mod oracle;

use super::*;
use crate::adapters::AdapterEventContext;

mod abi {
    alloy_sol_types::sol! {
        event Fees(address indexed sender, uint256 amount0, uint256 amount1);
        event Transfer(address indexed from, address indexed to, uint256 amount);
        event IncreaseObservationCardinalityNext(address indexed caller, uint16 observationCardinalityNextOld, uint16 observationCardinalityNextNew);
    }
}

const POOL: Address = address!("c71b571aa9f06bfa096c24e9df5c45fc4fab4151");
const ORACLE_META: u64 = 196_616;
const RESERVE0: u64 = 196_619;
const RESERVE1: u64 = 196_620;
const LAST_TIME: u64 = 196_621;
const CUMULATIVE0: u64 = 196_622;
const CUMULATIVE1: u64 = 196_623;
const INDEX0: u64 = 196_624;
const INDEX1: u64 = 196_625;

fn invalid(message: &'static str) -> AdapterEventError {
    AdapterEventError::Custom(format!("Aero volatile execution: {message}"))
}

fn cell(view: &dyn StateView, address: Address, slot: U256) -> Result<U256, AdapterEventError> {
    view.storage(address, slot)
        .ok_or(AdapterEventError::MissingState { address, slot })
}

fn pool_cell(view: &dyn StateView, slot: u64) -> Result<U256, AdapterEventError> {
    cell(view, POOL, U256::from(slot))
}

fn attest(view: &dyn StateView, emitter: Address) -> Result<(), AdapterEventError> {
    let reviewed = reviewed_aero_volatile_runtime(AeroVolatileRuntimeFamily::AeroOptimismMooBifi);
    let identities = [
        (reviewed.pool, reviewed.proxy_runtime_code_hash),
        (
            reviewed.implementation,
            reviewed.implementation_runtime_code_hash,
        ),
        (reviewed.factory, reviewed.factory_runtime_code_hash),
        (reviewed.fee_module, reviewed.fee_module_runtime_code_hash),
        (
            reviewed.mev_tax_module,
            reviewed.mev_tax_module_runtime_code_hash,
        ),
    ];
    for (address, hash) in identities {
        if (address == POOL || address == reviewed.implementation || address == emitter)
            && view.verified_code_hash(10, address) != Some(hash)
        {
            return Err(invalid("unattested volatile execution runtime"));
        }
    }
    Ok(())
}

fn context(
    pool: &PoolRegistration,
    context: &AdapterEventContext,
) -> Result<u64, AdapterEventError> {
    if reviewed_aero_volatile_runtime_for_pool(pool).is_none()
        || solidly_layout(pool) != Some(SolidlyStorageLayout::aero_volatile())
        || context.chain_id != Some(10)
    {
        return Err(invalid("unreviewed registration, layout or chain"));
    }
    if context.block_number.is_none()
        || context.block_hash.is_none_or(|hash| hash.is_zero())
        || context.parent_hash.is_none_or(|hash| hash.is_zero())
        || context.transaction_hash.is_none_or(|hash| hash.is_zero())
        || context.transaction_index.is_none()
        || context.log_index.is_none()
        || context.block_hash == context.parent_hash
    {
        return Err(invalid("complete canonical event lineage is required"));
    }
    context
        .block_timestamp
        .ok_or_else(|| invalid("block timestamp is required"))
}

pub(super) fn event_updates(
    pool: &PoolRegistration,
    log: &Log,
    view: &dyn StateView,
    event_context: &AdapterEventContext,
) -> Result<Option<Vec<StateUpdate>>, AdapterEventError> {
    if log.address != POOL {
        let updates = governance::event_updates(log, view)?;
        if updates.is_some() {
            context(pool, event_context)?;
            attest(view, log.address)?;
        }
        return Ok(updates);
    }
    let Some(topic) = log.topics().first() else {
        return Ok(None);
    };
    if ![
        Sync::SIGNATURE_HASH,
        abi::Fees::SIGNATURE_HASH,
        abi::Transfer::SIGNATURE_HASH,
        abi::IncreaseObservationCardinalityNext::SIGNATURE_HASH,
    ]
    .contains(topic)
    {
        return Ok(None);
    }
    let timestamp = context(pool, event_context)?;
    attest(view, POOL)?;
    let updates = if *topic == Sync::SIGNATURE_HASH {
        let event =
            Sync::decode_log_data_validate(&log.data).map_err(|_| invalid("malformed Sync"))?;
        oracle::sync(view, timestamp, event.reserve0, event.reserve1)?
    } else if *topic == abi::IncreaseObservationCardinalityNext::SIGNATURE_HASH {
        let event = abi::IncreaseObservationCardinalityNext::decode_log_data_validate(&log.data)
            .map_err(|_| invalid("malformed oracle growth"))?;
        oracle::grow(
            view,
            event.observationCardinalityNextOld,
            event.observationCardinalityNextNew,
        )?
    } else if *topic == abi::Fees::SIGNATURE_HASH {
        let event = abi::Fees::decode_log_data_validate(&log.data)
            .map_err(|_| invalid("malformed Fees"))?;
        fee_indexes(view, event.amount0, event.amount1)?
    } else {
        let event = abi::Transfer::decode_log_data_validate(&log.data)
            .map_err(|_| invalid("malformed LP Transfer"))?;
        total_supply(view, event.from, event.to, event.amount)?
    };
    Ok(Some(updates))
}

fn fee_indexes(
    view: &dyn StateView,
    fee0: U256,
    fee1: U256,
) -> Result<Vec<StateUpdate>, AdapterEventError> {
    if (fee0.is_zero() && fee1.is_zero()) || (!fee0.is_zero() && !fee1.is_zero()) {
        return Err(invalid("Fees must name exactly one nonzero token amount"));
    }
    let supply = pool_cell(view, 2)?;
    if supply.is_zero() {
        return Err(invalid("zero LP supply at fee accrual"));
    }
    let (slot, amount) = if fee0.is_zero() {
        (INDEX1, fee1)
    } else {
        (INDEX0, fee0)
    };
    let ratio = amount
        .checked_mul(U256::from(10_u64.pow(18)))
        .ok_or_else(|| invalid("fee multiplication overflow"))?
        / supply;
    let index = pool_cell(view, slot)?
        .checked_add(ratio)
        .ok_or_else(|| invalid("fee index overflow"))?;
    Ok(vec![StateUpdate::slot(POOL, U256::from(slot), index)])
}

fn total_supply(
    view: &dyn StateView,
    from: Address,
    to: Address,
    amount: U256,
) -> Result<Vec<StateUpdate>, AdapterEventError> {
    if !from.is_zero() && !to.is_zero() {
        return Ok(Vec::new());
    }
    if from.is_zero() && to.is_zero() {
        return Err(invalid("LP transfer has two zero endpoints"));
    }
    let old = pool_cell(view, 2)?;
    let value = if from.is_zero() {
        old.checked_add(amount)
    } else {
        old.checked_sub(amount)
    }
    .ok_or_else(|| invalid("LP supply arithmetic overflow"))?;
    Ok(vec![StateUpdate::slot(POOL, U256::from(2), value)])
}

#[cfg(test)]
mod tests;
