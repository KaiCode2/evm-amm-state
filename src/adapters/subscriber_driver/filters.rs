//! Preserve each source's predicate when coalescing exact-block reads.
use alloy_rpc_types_eth::Filter;
use std::collections::BTreeSet;

pub(super) fn reconciliation_filters(filters: &[Filter], max_addresses: usize) -> Vec<Filter> {
    // Only equal topic tuples can share an address union. Combining arbitrary
    // address/topic sets creates a cross product; dropping indexed topics can
    // turn a position-specific interest into an entire protocol feed.
    let mut groups: Vec<Filter> = Vec::new();
    for filter in filters {
        if let Some(group) = groups
            .iter_mut()
            .find(|group| group.topics == filter.topics)
        {
            if group.address.is_empty() || filter.address.is_empty() {
                group.address = Default::default();
            } else {
                group.address = group
                    .address
                    .iter()
                    .chain(filter.address.iter())
                    .copied()
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect::<Vec<_>>()
                    .into();
            }
        } else {
            groups.push(filter.clone());
        }
    }
    groups
        .into_iter()
        .flat_map(|group| {
            if group.address.is_empty() {
                return vec![group];
            }
            group
                .address
                .iter()
                .copied()
                .collect::<Vec<_>>()
                .chunks(max_addresses.max(1))
                .map(|chunk| group.clone().address(chunk.to_vec()))
                .collect()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address, B256};

    #[test]
    fn preserves_emitter_topic_pairs_and_every_indexed_constraint() {
        let first = Filter::new()
            .address(Address::repeat_byte(1))
            .event_signature(B256::repeat_byte(2))
            .topic1(B256::repeat_byte(3));
        let second = Filter::new()
            .address(Address::repeat_byte(4))
            .event_signature(B256::repeat_byte(5))
            .topic3(B256::repeat_byte(6));
        assert_eq!(
            reconciliation_filters(&[first.clone(), second.clone()], 256),
            vec![first, second]
        );
    }

    #[test]
    fn equal_predicates_coalesce_and_chunk_without_losing_topics() {
        let filters: Vec<_> = (0..=255)
            .map(|i| {
                Filter::new()
                    .address(Address::repeat_byte(i))
                    .event_signature(B256::repeat_byte(2))
                    .topic1(B256::repeat_byte(3))
                    .topic2(B256::repeat_byte(4))
            })
            .collect();
        let result = reconciliation_filters(&filters, 100);
        assert_eq!(
            result.iter().map(|f| f.address.len()).collect::<Vec<_>>(),
            [100, 100, 56]
        );
        assert!(result.iter().all(|f| f.topics == filters[0].topics));
    }
    #[test]
    fn wildcard_and_disjoint_predicates_preserve_the_original_union() {
        let a = Address::repeat_byte(1);
        let b = Address::repeat_byte(2);
        let t = B256::repeat_byte(3);
        let filters = vec![
            Filter::new()
                .event_signature(t)
                .topic2(B256::repeat_byte(4)),
            Filter::new().address(a).topic1(B256::repeat_byte(5)),
            Filter::new()
                .address(b)
                .event_signature(B256::repeat_byte(6)),
        ];
        let result = reconciliation_filters(&filters, 1);
        let matches = |filters: &[Filter], address, topics: [B256; 4]| {
            filters.iter().any(|f| {
                (f.address.is_empty() || f.address.matches(&address))
                    && f.topics
                        .iter()
                        .zip(topics.iter())
                        .all(|(expected, topic)| expected.is_empty() || expected.matches(topic))
            })
        };
        for address in [a, b, Address::ZERO] {
            for first in [t, B256::repeat_byte(6), B256::ZERO] {
                for second in [B256::repeat_byte(5), B256::ZERO] {
                    for third in [B256::repeat_byte(4), B256::ZERO] {
                        let topics = [first, second, third, B256::ZERO];
                        assert_eq!(
                            matches(&filters, address, topics),
                            matches(&result, address, topics)
                        );
                    }
                }
            }
        }
    }
}
