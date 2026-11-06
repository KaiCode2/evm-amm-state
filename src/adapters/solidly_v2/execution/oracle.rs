//! Exact three-word reserve oracle, including its distinct uint32 time wrap.
use super::*;

fn meta(view: &dyn StateView) -> Result<(U256, u16, u16, u16), AdapterEventError> {
    let word = pool_cell(view, ORACLE_META)?;
    let index = (word & U256::from(u16::MAX)).to::<u16>();
    let cardinality = ((word >> 16_usize) & U256::from(u16::MAX)).to::<u16>();
    let next = ((word >> 32_usize) & U256::from(u16::MAX)).to::<u16>();
    if cardinality == 0 || index >= cardinality || next < cardinality {
        return Err(invalid("corrupt reserve oracle cardinality"));
    }
    Ok((word, index, cardinality, next))
}

fn observation(index: u16) -> u64 {
    11 + 3 * u64::from(index)
}

// The immutable clone's reviewed implementation initializes only observation 0,
// grow writes the contiguous [oldNext,newNext) range, and write consumes the
// initialized prefix one entry at a time. There is no shrink, reset or arbitrary
// storage writer. Therefore cells beyond next are zero and prewarmed entries
// [cardinality,next) are exactly (1,1,1). This is a protocol proof, not a cache
// fallback; existing contradictory cells or absent runtime evidence reject.
fn allocated_triple(
    view: &dyn StateView,
    index: u16,
    lower: u16,
    upper: u16,
    expected: U256,
) -> Result<U256, AdapterEventError> {
    attest(view, POOL)?;
    if index < lower || index >= upper {
        return Err(invalid(
            "oracle allocation is reused or outside its contiguous range",
        ));
    }
    for slot in observation(index)..observation(index) + 3 {
        if view
            .storage(POOL, U256::from(slot))
            .is_some_and(|word| word != expected)
        {
            return Err(invalid(
                "oracle allocation contradicts known parent storage",
            ));
        }
    }
    Ok(expected)
}

pub(super) fn sync(
    view: &dyn StateView,
    timestamp: u64,
    reserve0: U256,
    reserve1: U256,
) -> Result<Vec<StateUpdate>, AdapterEventError> {
    let prior_time = pool_cell(view, LAST_TIME)?;
    let elapsed = U256::from(timestamp)
        .checked_sub(prior_time)
        .ok_or_else(|| invalid("block timestamp regressed"))?;
    let old0 = pool_cell(view, RESERVE0)?;
    let old1 = pool_cell(view, RESERVE1)?;
    let mut cumulatives = [pool_cell(view, CUMULATIVE0)?, pool_cell(view, CUMULATIVE1)?];
    if !elapsed.is_zero() && !old0.is_zero() && !old1.is_zero() {
        for (value, old) in cumulatives.iter_mut().zip([old0, old1]) {
            *value = old
                .checked_mul(elapsed)
                .and_then(|increment| value.checked_add(increment))
                .ok_or_else(|| invalid("reserve cumulative arithmetic overflow"))?;
        }
    }
    let (metadata, index, cardinality, next) = meta(view)?;
    let last_word = pool_cell(view, observation(index))?;
    let last = (last_word & U256::from(u32::MAX)).to::<u32>();
    // Solidity narrows both operands to uint32 before unchecked subtraction.
    let now = (timestamp & u64::from(u32::MAX)) as u32;
    let mut updates = vec![
        StateUpdate::slot(POOL, U256::from(RESERVE0), reserve0),
        StateUpdate::slot(POOL, U256::from(RESERVE1), reserve1),
        StateUpdate::slot(POOL, U256::from(LAST_TIME), U256::from(timestamp)),
        StateUpdate::slot(POOL, U256::from(CUMULATIVE0), cumulatives[0]),
        StateUpdate::slot(POOL, U256::from(CUMULATIVE1), cumulatives[1]),
    ];
    if now.wrapping_sub(last) > 60 {
        let prior_cardinality = cardinality;
        let cardinality = if next > cardinality && index == cardinality - 1 {
            cardinality
                .checked_add(1)
                .ok_or_else(|| invalid("oracle cardinality overflow"))?
        } else {
            cardinality
        };
        let index = index
            .checked_add(1)
            .ok_or_else(|| invalid("oracle index overflow"))?
            % cardinality;
        let slot = observation(index);
        let target = if cardinality > prior_cardinality {
            allocated_triple(view, index, prior_cardinality, next, U256::ONE)?
        } else {
            pool_cell(view, slot)?
        };
        updates.push(StateUpdate::slot(
            POOL,
            U256::from(slot),
            (target & !U256::from(u32::MAX)) | U256::from(now),
        ));
        updates.push(StateUpdate::slot(
            POOL,
            U256::from(slot + 1),
            cumulatives[0],
        ));
        updates.push(StateUpdate::slot(
            POOL,
            U256::from(slot + 2),
            cumulatives[1],
        ));
        updates.push(StateUpdate::slot(
            POOL,
            U256::from(ORACLE_META),
            (metadata & !U256::from(u32::MAX))
                | U256::from(index)
                | (U256::from(cardinality) << 16_usize),
        ));
    }
    Ok(updates)
}

pub(super) fn grow(
    view: &dyn StateView,
    old: u16,
    next: u16,
) -> Result<Vec<StateUpdate>, AdapterEventError> {
    let (word, _, _, actual) = meta(view)?;
    if old != actual || next <= old {
        return Err(invalid("oracle growth contradicts prior cardinalityNext"));
    }
    // Only the new contiguous suffix is derivable from the reviewed allocation
    // invariant. Missing existing observations remain ordinary state errors.
    let mut updates = Vec::with_capacity(3 * usize::from(next - old) + 1);
    for index in old..next {
        let slot = observation(index);
        let timestamp = allocated_triple(view, index, old, next, U256::ZERO)?;
        updates.push(StateUpdate::slot(
            POOL,
            U256::from(slot),
            (timestamp & !U256::from(u32::MAX)) | U256::ONE,
        ));
        updates.push(StateUpdate::slot(POOL, U256::from(slot + 1), U256::ONE));
        updates.push(StateUpdate::slot(POOL, U256::from(slot + 2), U256::ONE));
    }
    updates.push(StateUpdate::slot(
        POOL,
        U256::from(ORACLE_META),
        (word & !(U256::from(u16::MAX) << 32_usize)) | (U256::from(next) << 32_usize),
    ));
    Ok(updates)
}
