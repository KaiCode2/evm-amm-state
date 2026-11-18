use super::*;

struct Chain {
    index: U256,
    time_index: U256,
    timestamp: u64,
    updates: Vec<StateUpdate>,
}

fn chain(
    reviewed: ReviewedAeroClRuntime,
    state: &dyn StateView,
    timestamp: u64,
) -> Result<Chain, AdapterEventError> {
    let voter = reviewed.voter;
    let rate = read(state, voter, U256::from(2))?;
    let mut index = read(state, voter, U256::from(3))?;
    let mut time_index = read(state, voter, U256::from(4))?;
    let word = read(state, voter, U256::from(5))?;
    let mut cursor = (word & MASK_48).to::<u64>();
    if timestamp < cursor || timestamp / WEEK - cursor / WEEK > MAX_WEEKS {
        return Err(unsupported(
            "LeafVoter global settlement exceeds retained schedule",
        ));
    }
    let mut updates = Vec::new();
    // Known cold-start schedule bounds this loop to at most two boundaries.
    let from = cursor / WEEK + 1;
    for week in from..=timestamp / WEEK {
        let boundary = week * WEEK;
        advance(rate, boundary, &mut cursor, &mut index, &mut time_index)?;
        updates.push(StateUpdate::slot(
            voter,
            mapping(U256::from(boundary), U256::from(6)),
            index,
        ));
        updates.push(StateUpdate::slot(
            voter,
            mapping(U256::from(boundary), U256::from(7)),
            time_index,
        ));
    }
    advance(rate, timestamp, &mut cursor, &mut index, &mut time_index)?;
    updates.extend([
        StateUpdate::slot(voter, U256::from(3), index),
        StateUpdate::slot(voter, U256::from(4), time_index),
        StateUpdate::slot(
            voter,
            U256::from(5),
            (word & !MASK_48) | U256::from(timestamp),
        ),
    ]);
    Ok(Chain {
        index,
        time_index,
        timestamp,
        updates,
    })
}

pub(super) fn derive(
    reviewed: ReviewedAeroClRuntime,
    event: GaugeCapSettled,
    state: &dyn StateView,
    context: &AdapterEventContext,
) -> Result<Vec<StateUpdate>, AdapterEventError> {
    let timestamp = context
        .block_timestamp
        .ok_or_else(|| unsupported("missing voter settlement timestamp"))?;
    let mut chain = chain(reviewed, state, timestamp)?;
    let surplus = read(state, reviewed.voter, U256::from(15))?
        .checked_add(U256::from(event.surplusDelta))
        .ok_or_else(arithmetic)?;
    chain
        .updates
        .push(StateUpdate::slot(reviewed.voter, U256::from(15), surplus));
    if event.gauge != reviewed.gauge.0 {
        return Ok(chain.updates);
    }
    let (base, mut words) = gauge_words(reviewed, state)?;
    let last = (words[1] & MASK_48).to::<u64>();
    if last >= timestamp || timestamp / WEEK - last / WEEK > MAX_WEEKS {
        return Err(unsupported(
            "LeafVoter gauge settlement exceeds retained schedule or repeats",
        ));
    }
    let mut bias = (words[4] & MASK_128).to::<u128>() as i128;
    let mut slope = (words[4] >> 128_usize).to::<u128>() as i128;
    let permanent = (words[5] >> 48_usize) & MASK_128;
    let mut cursor = last;
    for week in last / WEEK + 1..=timestamp / WEEK {
        let boundary = week * WEEK;
        bias = bias
            .checked_sub(
                slope
                    .checked_mul(i128::from(boundary - cursor))
                    .ok_or_else(arithmetic)?,
            )
            .ok_or_else(arithmetic)?;
        let slot = mapping(
            U256::from(boundary),
            solidity_address_mapping_key(event.gauge, 9),
        );
        let delta = (read(state, reviewed.voter, slot)? & MASK_128).to::<u128>() as i128;
        slope = slope.checked_sub(delta).ok_or_else(arithmetic)?.max(0);
        cursor = boundary;
    }
    bias = bias
        .checked_sub(
            slope
                .checked_mul(i128::from(timestamp - cursor))
                .ok_or_else(arithmetic)?,
        )
        .ok_or_else(arithmetic)?;
    let weight = if bias < 0 {
        if let Some(weight) = permanent.checked_sub(U256::from(bias.unsigned_abs())) {
            weight
        } else {
            bias = 0;
            slope = 0;
            permanent
        }
    } else {
        permanent
            .checked_add(U256::from(bias as u128))
            .ok_or_else(arithmetic)?
    };
    if weight != event.gaugeWeight {
        return Err(unsupported(
            "LeafVoter settled weight contradicts known slope schedule",
        ));
    }
    let ceiling = (words[0] & MASK_128)
        .checked_add(U256::from(event.effectiveShare))
        .filter(|n| *n <= MASK_128)
        .ok_or_else(arithmetic)?;
    words[0] = (words[0] & !MASK_128) | ceiling;
    words[1] = (words[1] & !MASK_48) | U256::from(chain.timestamp);
    words[2] = chain.index;
    words[3] = chain.time_index;
    words[4] = U256::from(bias as u128) | (U256::from(slope as u128) << 128_usize);
    words[5] = (words[5] & !MASK_48) | U256::from(timestamp);
    chain
        .updates
        .extend(words.into_iter().enumerate().map(|(i, word)| {
            StateUpdate::slot(reviewed.voter, base.wrapping_add(U256::from(i)), word)
        }));
    Ok(chain.updates)
}

fn advance(
    rate: U256,
    to: u64,
    cursor: &mut u64,
    index: &mut U256,
    time_index: &mut U256,
) -> Result<(), AdapterEventError> {
    let dt = U256::from(to.checked_sub(*cursor).ok_or_else(arithmetic)?);
    let squared_dt = U256::from(to) * U256::from(to) - U256::from(*cursor) * U256::from(*cursor);
    *index = index
        .checked_add(rate.checked_mul(dt).ok_or_else(arithmetic)?)
        .ok_or_else(arithmetic)?;
    *time_index = time_index
        .checked_add(rate.checked_mul(squared_dt).ok_or_else(arithmetic)?)
        .ok_or_else(arithmetic)?;
    *cursor = to;
    Ok(())
}
