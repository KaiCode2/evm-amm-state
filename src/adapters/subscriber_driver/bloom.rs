//! Conservative absence proof only. Does not choose reconciliation policy.
use alloy_primitives::{Bloom, BloomInput};
use alloy_rpc_types_eth::Filter;

pub(super) fn excludes_every_filter<'a>(
    bloom: &Bloom,
    filters: impl IntoIterator<Item = &'a Filter>,
) -> bool {
    let admits = |value: &[u8]| bloom.contains_input(BloomInput::Raw(value));
    let mut saw_filter = false;
    for filter in filters {
        saw_filter = true;
        // Preserve existing conservative policy even when constrained topics
        // alone could exclude an address-wildcard filter.
        if filter.address.is_empty() {
            return false;
        }
        if !filter
            .address
            .iter()
            .any(|address| admits(address.as_slice()))
        {
            continue;
        }
        if filter.topics.iter().any(|topics| {
            !topics.is_empty() && !topics.iter().any(|topic| admits(topic.as_slice()))
        }) {
            continue;
        }
        return false;
    }
    saw_filter
}

#[cfg(test)]
mod tests;
