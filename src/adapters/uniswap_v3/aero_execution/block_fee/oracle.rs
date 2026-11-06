//! Tick-only projection of the reviewed Solidity0.7.6 pool Oracle. Unknown
//! cells are errors, not the hook's caught on-chain `OLD`/revert branch.

use super::{AdapterEventError, StateView, read, unsupported};
use alloy_primitives::{Address, U256};

#[derive(Clone, Copy)]
struct Observation {
    timestamp: u32,
    tick: i64,
    initialized: bool,
}

fn signed(word: U256, bits: u32) -> i64 {
    let raw = (word & ((U256::from(1) << bits) - U256::from(1))).to::<u64>();
    ((raw << (64 - bits)) as i64) >> (64 - bits)
}

fn wrap56(value: i128) -> i64 {
    ((value as i64) << 8) >> 8
}

fn observation(
    pool: Address,
    state: &dyn StateView,
    index: u32,
) -> Result<Observation, AdapterEventError> {
    if index >= 65_535 {
        return Err(unsupported("Aero oracle index exceeds fixed ring"));
    }
    let value = read(state, pool, U256::from(17 + index))?;
    let flag = (value >> 248_usize).to::<u8>();
    if flag > 1 {
        return Err(unsupported("invalid Aero oracle initialized flag"));
    }
    Ok(Observation {
        timestamp: (value & U256::from(u32::MAX)).to::<u32>(),
        tick: signed(value >> 32_usize, 56),
        initialized: flag == 1,
    })
}

fn lte(now: u32, a: u32, b: u32) -> bool {
    if a <= now && b <= now {
        return a <= b;
    }
    let adjust = |t| u64::from(t) + if t > now { 0 } else { 1_u64 << 32 };
    adjust(a) <= adjust(b)
}

fn transform(observation: Observation, timestamp: u32, tick: i64) -> i64 {
    wrap56(
        i128::from(observation.tick)
            + i128::from(tick) * i128::from(timestamp.wrapping_sub(observation.timestamp)),
    )
}

pub(super) fn tick_distance(
    pool: Address,
    state: &dyn StateView,
    now: u32,
    lookback: u32,
) -> Result<u32, AdapterEventError> {
    let slot0 = read(state, pool, U256::from(6))?;
    let tick = signed(slot0 >> 160_usize, 24);
    if !(-887_272..=887_272).contains(&tick) {
        return Err(unsupported("Aero oracle current tick out of bounds"));
    }
    let index = ((slot0 >> 184_usize) & U256::from(u16::MAX)).to::<u32>();
    let cardinality = ((slot0 >> 200_usize) & U256::from(u16::MAX)).to::<u32>();
    if cardinality == 0 {
        return Ok(0);
    }
    if index >= cardinality {
        return Err(unsupported("Aero oracle index exceeds cardinality"));
    }
    let mut oldest = observation(pool, state, (index + 1) % cardinality)?;
    if !oldest.initialized {
        oldest = observation(pool, state, 0)?;
    }
    let age = now
        .checked_sub(oldest.timestamp)
        .ok_or_else(|| unsupported("Aero hook oldest timestamp underflow"))?;
    if lookback > age {
        return Ok(0);
    }
    if !oldest.initialized {
        return Err(unsupported("Aero oracle oldest observation absent"));
    }
    let latest = observation(pool, state, index)?;
    if !latest.initialized {
        return Err(unsupported("Aero oracle latest observation absent"));
    }
    let target = now.wrapping_sub(lookback);
    let historical = if lte(now, latest.timestamp, target) {
        transform(latest, target, tick)
    } else {
        // The hook's prior oldest-age check proves OLD is not the reason to
        // return zero here. Inconsistent/missing cached history fails closed.
        search(pool, state, now, target, index, cardinality)?
    };
    let current = transform(latest, now, tick);
    let delta = current
        .checked_sub(historical)
        .filter(|value| (-(1_i64 << 55)..(1_i64 << 55)).contains(value))
        .ok_or_else(|| unsupported("Aero hook int56 cumulative difference overflow"))?;
    let denominator = i64::from(lookback);
    // The reviewed hook narrows before its checked negative-floor decrement.
    let average = delta / denominator;
    let mut average = (average << 40) >> 40;
    if delta < 0 && delta % denominator != 0 {
        average = average
            .checked_sub(1)
            .filter(|value| (-(1_i64 << 23)..(1_i64 << 23)).contains(value))
            .ok_or_else(|| unsupported("Aero hook int24 average decrement overflow"))?;
    }
    let distance = tick
        .checked_sub(average)
        .filter(|value| (-(1_i64 << 23)..(1_i64 << 23)).contains(value))
        .ok_or_else(|| unsupported("Aero hook int24 tick difference overflow"))?;
    if distance == -(1_i64 << 23) {
        return Err(unsupported("Aero hook int24 negation overflow"));
    }
    u32::try_from(distance.unsigned_abs())
        .map_err(|_| unsupported("Aero tick distance exceeds uint24"))
}

fn search(
    pool: Address,
    state: &dyn StateView,
    now: u32,
    target: u32,
    index: u32,
    cardinality: u32,
) -> Result<i64, AdapterEventError> {
    let mut left = (index + 1) % cardinality;
    let mut right = left + cardinality - 1;
    for _ in 0..16 {
        if left > right {
            break;
        }
        let middle = (left + right) / 2;
        let before = observation(pool, state, middle % cardinality)?;
        if !before.initialized {
            left = middle + 1;
            continue;
        }
        let after = observation(pool, state, (middle + 1) % cardinality)?;
        if lte(now, before.timestamp, target) && lte(now, target, after.timestamp) {
            if !after.initialized {
                return Err(unsupported("Aero oracle successor is uninitialized"));
            }
            if target == before.timestamp {
                return Ok(before.tick);
            }
            if target == after.timestamp {
                return Ok(after.tick);
            }
            let duration = after.timestamp.wrapping_sub(before.timestamp);
            if duration == 0 {
                return Err(unsupported("Aero oracle interpolation duration is zero"));
            }
            let delta = wrap56(i128::from(after.tick) - i128::from(before.tick));
            return Ok(wrap56(
                i128::from(before.tick)
                    + i128::from(delta / i64::from(duration))
                        * i128::from(target.wrapping_sub(before.timestamp)),
            ));
        }
        if !lte(now, before.timestamp, target) {
            right = middle
                .checked_sub(1)
                .ok_or_else(|| unsupported("Aero oracle binary-search underflow"))?;
        } else {
            left = middle + 1;
        }
    }
    Err(unsupported("Aero oracle search exceeded16 bounded steps"))
}
