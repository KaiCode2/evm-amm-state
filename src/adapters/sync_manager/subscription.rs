//! Exact pool source comparison, independent of transport filter caches/order.

use super::super::EventSource;

pub(super) fn same_event_sources(left: &[EventSource], right: &[EventSource]) -> bool {
    let contains = |sources: &[EventSource], candidate: &EventSource| {
        sources.iter().any(|source| {
            source.emitter == candidate.emitter
                && source.route == candidate.route
                && source
                    .topics
                    .iter()
                    .all(|topic| candidate.topics.contains(topic))
                && candidate
                    .topics
                    .iter()
                    .all(|topic| source.topics.contains(topic))
        })
    };
    left.iter().all(|source| contains(right, source))
        && right.iter().all(|source| contains(left, source))
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "live-runtime")]
    use super::super::super::reactive::event_source_interest;
    use super::*;
    use alloy_primitives::{Address, B256};
    #[cfg(feature = "live-runtime")]
    use evm_fork_cache::reactive::ReactiveInterest;

    #[test]
    #[cfg(feature = "live-runtime")]
    fn equivalent_sources_do_not_inherit_debug_filter_cache_differences() {
        let source = EventSource::direct(
            Address::repeat_byte(1),
            vec![B256::repeat_byte(2), B256::repeat_byte(3)],
        );
        let old = event_source_interest(source.clone());
        let new = event_source_interest(source.clone());
        let ReactiveInterest::Logs(logs) = &old else {
            unreachable!()
        };
        let _ = logs.provider_filter.address_bloom_filter();
        let _ = logs.provider_filter.topics_bloom_filter();
        assert_ne!(
            format!("{old:?}"),
            format!("{new:?}"),
            "legacy comparison confuses cache state with subscription identity"
        );
        assert!(same_event_sources(
            std::slice::from_ref(&source),
            std::slice::from_ref(&source)
        ));
    }

    #[test]
    fn source_comparison_preserves_emitter_topic_and_routing_semantics() {
        let source = EventSource::direct(
            Address::repeat_byte(1),
            vec![B256::repeat_byte(2), B256::repeat_byte(3)],
        );
        let mut reordered = source.clone();
        reordered.topics.reverse();
        assert!(same_event_sources(
            std::slice::from_ref(&source),
            &[reordered]
        ));
        for changed in [
            EventSource::direct(Address::repeat_byte(4), source.topics.clone()),
            EventSource::direct(source.emitter, vec![B256::repeat_byte(2)]),
            EventSource::direct(source.emitter, vec![]),
            EventSource::indexed_address(source.emitter, source.topics.clone(), 1),
        ] {
            assert!(!same_event_sources(
                std::slice::from_ref(&source),
                &[changed]
            ));
        }
    }
}
