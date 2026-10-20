use super::*;
use alloy_primitives::{Address, B256, Bytes, Log};

fn log(address: u8, topics: &[u8]) -> Log {
    Log::new_unchecked(
        Address::repeat_byte(address),
        topics.iter().map(|t| B256::repeat_byte(*t)).collect(),
        Bytes::new(),
    )
}
fn bloom(logs: &[Log]) -> Bloom {
    let mut bloom = Bloom::ZERO;
    for log in logs {
        bloom.accrue(BloomInput::Raw(log.address.as_slice()));
        for topic in log.topics() {
            bloom.accrue(BloomInput::Raw(topic.as_slice()));
        }
    }
    bloom
}

#[test]
fn actual_filter_matches_never_have_bloom_absence() {
    // Exhaustive small-domain matrix uses Alloy's real exact predicate, not a
    // duplicate filter matcher: address OR, topic OR within a slot, AND across
    // slots, wildcards, fewer topics, and multi-interest unions.
    let addresses = [
        vec![],
        vec![Address::repeat_byte(1)],
        vec![Address::repeat_byte(1), Address::repeat_byte(2)],
    ];
    let topics = [
        vec![],
        vec![B256::repeat_byte(3)],
        vec![B256::repeat_byte(3), B256::repeat_byte(4)],
    ];
    for address in addresses {
        for first in &topics {
            for second in &topics {
                let filter = Filter::new()
                    .address(address.clone())
                    .event_signature(first.clone())
                    .topic1(second.clone());
                for emitter in [1, 2, 9] {
                    for ts in [vec![], vec![3], vec![4], vec![3, 4], vec![4, 3], vec![9, 9]] {
                        let candidate = log(emitter, &ts);
                        if filter.matches(&candidate) {
                            assert!(!excludes_every_filter(
                                &bloom(std::slice::from_ref(&candidate)),
                                [&filter]
                            ));
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn address_or_topic_slot_and_and_union_are_conservative() {
    let filter = Filter::new()
        .address(vec![Address::repeat_byte(1), Address::repeat_byte(2)])
        .event_signature(vec![B256::repeat_byte(3), B256::repeat_byte(4)])
        .topic1(B256::repeat_byte(5));
    assert!(!excludes_every_filter(
        &bloom(&[log(2, &[4, 5])]),
        [&filter]
    ));
    assert!(excludes_every_filter(&bloom(&[log(2, &[4, 6])]), [&filter]));
    // Separate logs can supply each required bit. A bloom cannot prove
    // co-occurrence or position, so this false positive must remain admitted.
    assert!(!excludes_every_filter(
        &bloom(&[log(2, &[4]), log(9, &[5])]),
        [&filter]
    ));
    let other = Filter::new().address(Address::repeat_byte(9));
    assert!(!excludes_every_filter(
        &bloom(&[log(9, &[])]),
        [&filter, &other]
    ));
    assert!(!excludes_every_filter(&Bloom::repeat_byte(255), [&filter]));
}

#[test]
fn empty_bloom_wildcard_and_no_interest_preserve_existing_policy() {
    let concrete = Filter::new().address(Address::repeat_byte(1));
    assert!(excludes_every_filter(&Bloom::ZERO, [&concrete]));
    let wildcard = Filter::new().event_signature(B256::repeat_byte(3));
    // Conservative but avoidable reconciliation; this audit does not change it.
    assert!(!excludes_every_filter(&Bloom::ZERO, [&wildcard]));
    assert!(!excludes_every_filter(&Bloom::ZERO, std::iter::empty()));
}
