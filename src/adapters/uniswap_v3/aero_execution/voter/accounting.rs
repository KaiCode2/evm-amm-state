use super::*;

fn claimable(words: &[U256; 6]) -> Result<U256, AdapterEventError> {
    (words[0] & MASK_128)
        .checked_sub(words[0] >> 128_usize)
        .and_then(|remaining| remaining.checked_sub((words[1] >> 64_usize) & MASK_128))
        .ok_or_else(arithmetic)
}

pub(super) fn mint(
    reviewed: ReviewedAeroClRuntime,
    event: EmissionsMinted,
    state: &dyn StateView,
) -> Result<Vec<StateUpdate>, AdapterEventError> {
    if event.recipients.len() != event.amounts.len() || event.amounts.len() > 64 {
        return Err(unsupported("invalid LeafVoter emissions array lengths"));
    }
    if event.gauge != reviewed.gauge.0 {
        return Ok(Vec::new());
    }
    let (base, words) = gauge_words(reviewed, state)?;
    let total = event
        .amounts
        .iter()
        .try_fold(0_u128, |total, next| total.checked_add(*next))
        .ok_or_else(arithmetic)?;
    if total == 0 || U256::from(total) > claimable(&words)? {
        return Err(unsupported("LeafVoter mint exceeds known entitlement"));
    }
    let claimed = (words[0] >> 128_usize)
        .checked_add(U256::from(total))
        .filter(|n| *n <= MASK_128)
        .ok_or_else(arithmetic)?;
    Ok(vec![StateUpdate::slot(
        reviewed.voter,
        base,
        (words[0] & MASK_128) | (claimed << 128_usize),
    )])
}

pub(super) fn forfeit(
    reviewed: ReviewedAeroClRuntime,
    event: EmissionsForfeited,
    state: &dyn StateView,
) -> Result<Vec<StateUpdate>, AdapterEventError> {
    let total = read(state, reviewed.voter, U256::from(15))?
        .checked_add(U256::from(event.amount))
        .ok_or_else(arithmetic)?;
    let mut updates = vec![StateUpdate::slot(reviewed.voter, U256::from(15), total)];
    if event.gauge == reviewed.gauge.0 {
        let (base, words) = gauge_words(reviewed, state)?;
        if U256::from(event.amount) > claimable(&words)? {
            return Err(unsupported(
                "LeafVoter forfeiture exceeds known entitlement",
            ));
        }
        let surplus = ((words[1] >> 64_usize) & MASK_128)
            .checked_add(U256::from(event.amount))
            .filter(|n| *n <= MASK_128)
            .ok_or_else(arithmetic)?;
        updates.push(StateUpdate::slot(
            reviewed.voter,
            base.wrapping_add(U256::from(1)),
            (words[1] & !(MASK_128 << 64_usize)) | (surplus << 64_usize),
        ));
    }
    Ok(updates)
}
