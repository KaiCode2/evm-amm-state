use super::*;
use alloy_primitives::{Address, Bytes, Log};
use alloy_rpc_types_eth::{Filter, Header};
use evm_fork_cache::reactive::LogInterest;

fn context(source: InputSource) -> ReactiveContext {
    ReactiveContext {
        chain_id: Some(1),
        source,
        chain_status: ChainStatus::Included {
            block: block(1),
            confirmations: 0,
        },
        block: None,
        transaction_index: None,
        log_index: None,
    }
}
fn block(number: u64) -> BlockRef {
    BlockRef {
        number,
        hash: B256::repeat_byte(1),
        parent_hash: None,
        timestamp: Some(100),
    }
}
fn log(block: &BlockRef, index: u64) -> RpcLog {
    RpcLog {
        inner: Log::new_unchecked(
            Address::repeat_byte(2),
            vec![B256::repeat_byte(3)],
            Bytes::new(),
        ),
        block_hash: Some(block.hash),
        block_number: Some(block.number),
        block_timestamp: block.timestamp,
        transaction_hash: Some(B256::repeat_byte(4)),
        transaction_index: Some(0),
        log_index: Some(index),
        removed: false,
    }
}
fn interests() -> Vec<ReactiveInterest<Ethereum>> {
    vec![ReactiveInterest::Logs(LogInterest {
        provider_filter: Filter::new().address(Address::repeat_byte(2)),
        local_matcher: None,
        route_key: None,
    })]
}
fn header(ledger: &mut Ledger, block: &BlockRef) {
    let header = Header {
        hash: block.hash,
        inner: alloy_consensus::Header {
            number: block.number,
            ..Default::default()
        },
        ..Default::default()
    };
    ledger.observe(&ReactiveInputRecord::new(
        ReactiveInput::BlockHeader(header),
        context(InputSource::Subscription),
    ));
}
fn observe(ledger: &mut Ledger, log: RpcLog) {
    ledger.observe(&ReactiveInputRecord::new(
        ReactiveInput::Log(log),
        context(InputSource::Subscription),
    ));
}
fn reference(block: &BlockRef, indices: &[u64]) -> BTreeMap<u64, RpcLog> {
    indices.iter().map(|i| (*i, log(block, *i))).collect()
}

#[test]
fn equal_missing_and_empty_compare_unique_sets_not_notification_counts() {
    let b = block(1);
    let mut ledger = Ledger::default();
    header(&mut ledger, &b);
    assert_eq!(
        ledger
            .compare(&b, &interests(), Some(&BTreeMap::new()))
            .equal_blocks(),
        1
    );
    observe(&mut ledger, log(&b, 0));
    observe(&mut ledger, log(&b, 0));
    let equal = ledger.compare(&b, &interests(), Some(&reference(&b, &[0])));
    assert_eq!(
        (
            equal.equal_blocks(),
            equal.reference_logs(),
            equal.consumed_logs()
        ),
        (1, 1, 1)
    );
    let missing = ledger.compare(&b, &interests(), Some(&reference(&b, &[0, 1])));
    assert_eq!(
        (
            missing.missing_blocks(),
            missing.missing_logs(),
            missing.conflicting_blocks()
        ),
        (1, 1, 0)
    );
    // A late consumed record can close a later comparison. The earlier result
    // is a cutoff observation, never an assertion of permanent loss.
    observe(&mut ledger, log(&b, 1));
    assert_eq!(
        ledger
            .compare(&b, &interests(), Some(&reference(&b, &[0, 1])))
            .equal_blocks(),
        1
    );
}

#[test]
fn contradictory_content_extra_and_duplicate_poison_are_conflicting() {
    let b = block(1);
    let mut ledger = Ledger::default();
    header(&mut ledger, &b);
    let mut altered = log(&b, 0);
    altered.transaction_hash = Some(B256::repeat_byte(9));
    observe(&mut ledger, altered);
    let result = ledger.compare(&b, &interests(), Some(&reference(&b, &[0])));
    assert_eq!(
        (result.conflicting_blocks(), result.conflicting_logs()),
        (1, 1)
    );
    observe(&mut ledger, log(&b, 0));
    assert_eq!(
        ledger
            .compare(&b, &interests(), Some(&reference(&b, &[0])))
            .conflicting_logs(),
        1
    );
    observe(&mut ledger, log(&b, 2));
    let result = ledger.compare(&b, &interests(), Some(&reference(&b, &[0, 1])));
    assert_eq!(
        (
            result.conflicting_blocks(),
            result.missing_logs(),
            result.unexpected_logs()
        ),
        (1, 1, 1)
    );
}

#[test]
fn errors_malformed_reference_and_backfill_are_unavailable_not_missing() {
    let b = block(1);
    let mut ledger = Ledger::default();
    header(&mut ledger, &b);
    assert_eq!(
        ledger.compare(&b, &interests(), None).unavailable_blocks(),
        1
    );
    let mut bad = reference(&b, &[0]);
    bad.get_mut(&0).unwrap().block_hash = Some(B256::repeat_byte(8));
    assert_eq!(
        ledger
            .compare(&b, &interests(), Some(&bad))
            .unavailable_blocks(),
        1
    );
    let mut bad = reference(&b, &[0]);
    bad.get_mut(&0).unwrap().inner.address = Address::repeat_byte(8);
    assert_eq!(
        ledger
            .compare(&b, &interests(), Some(&bad))
            .unavailable_blocks(),
        1
    );
    ledger.observe(&ReactiveInputRecord::new(
        ReactiveInput::Log(log(&b, 0)),
        context(InputSource::Backfill),
    ));
    let result = ledger.compare(&b, &interests(), Some(&reference(&b, &[0])));
    assert_eq!(
        (
            result.unavailable_blocks(),
            result.missing_logs(),
            result.reference_logs()
        ),
        (1, 0, 0)
    );
}

#[test]
fn hashes_reset_retirement_and_capacity_never_manufacture_complete_observation() {
    let b = block(1);
    let mut ledger = Ledger::default();
    header(&mut ledger, &b);
    observe(&mut ledger, log(&b, 0));
    let mut replacement = b;
    replacement.hash = B256::repeat_byte(5);
    assert_eq!(
        ledger
            .compare(
                &replacement,
                &interests(),
                Some(&reference(&replacement, &[0]))
            )
            .unavailable_blocks(),
        1
    );
    ledger.clear();
    assert_eq!(
        ledger
            .compare(&b, &interests(), Some(&reference(&b, &[0])))
            .unavailable_blocks(),
        1
    );
    header(&mut ledger, &b);
    ledger.retire_through(1);
    header(&mut ledger, &b);
    observe(&mut ledger, log(&b, 0));
    assert_eq!(
        ledger
            .compare(&b, &interests(), Some(&reference(&b, &[0])))
            .unavailable_blocks(),
        1
    );
    ledger.clear();
    for n in 0..MAX_BLOCKS as u64 {
        header(&mut ledger, &block(n));
    }
    let refused = block(MAX_BLOCKS as u64);
    header(&mut ledger, &refused);
    ledger.retire_through(1);
    header(&mut ledger, &refused);
    assert_eq!(
        ledger
            .compare(&refused, &interests(), Some(&BTreeMap::new()))
            .unavailable_blocks(),
        1
    );
    assert!(ledger.blocks.len() <= MAX_BLOCKS);
    ledger.clear();
    header(&mut ledger, &b);
    for n in 0..=MAX_LOGS as u64 {
        observe(&mut ledger, log(&b, n));
    }
    assert_eq!(ledger.log_count, MAX_LOGS);
    assert_eq!(
        ledger
            .compare(&b, &interests(), Some(&reference(&b, &[0])))
            .unavailable_blocks(),
        1
    );
}

#[test]
fn timestamps_are_optional_but_must_not_contradict_the_header() {
    let b = block(1);
    let mut ledger = Ledger::default();
    header(&mut ledger, &b);
    let mut observed = log(&b, 0);
    observed.block_timestamp = None;
    observe(&mut ledger, observed);
    assert_eq!(
        ledger
            .compare(&b, &interests(), Some(&reference(&b, &[0])))
            .equal_blocks(),
        1
    );
    let mut bad = log(&b, 1);
    bad.block_timestamp = Some(999);
    observe(&mut ledger, bad);
    assert_eq!(
        ledger
            .compare(&b, &interests(), Some(&reference(&b, &[0, 1])))
            .unavailable_blocks(),
        1
    );
}

#[test]
fn opt_out_has_no_snapshot_and_enabled_counts_are_cumulative() {
    let config = AmmSubscriberDriverConfig::default();
    assert!(!config.reconciliation_observation_enabled());
    let enabled = config.with_reconciliation_observation(true);
    assert!(enabled.reconciliation_observation_enabled());
    assert_eq!(
        enabled.canonical_log_source(),
        config.canonical_log_source()
    );
    let counters = Counters::default();
    assert_eq!(counters.snapshot(), None);
    counters.set_enabled(true);
    counters.record(unavailable());
    counters.record(unavailable());
    assert_eq!(counters.snapshot().unwrap().unavailable_blocks(), 2);
}

#[test]
fn pure_local_selector_narrows_reference_before_missing_classification() {
    struct OnlyFirst;
    impl evm_fork_cache::reactive::LogMatcher for OnlyFirst {
        fn matches(&self, log: &RpcLog) -> bool {
            log.log_index == Some(0)
        }
    }
    let interests = vec![ReactiveInterest::Logs(LogInterest {
        provider_filter: Filter::new().address(Address::repeat_byte(2)),
        local_matcher: Some(Arc::new(OnlyFirst)),
        route_key: None,
    })];
    let b = block(1);
    let mut ledger = Ledger::default();
    header(&mut ledger, &b);
    observe(&mut ledger, log(&b, 0));
    let result = ledger.compare(&b, &interests, Some(&reference(&b, &[0, 1])));
    assert_eq!(
        (
            result.equal_blocks(),
            result.reference_logs(),
            result.missing_logs()
        ),
        (1, 1, 0)
    );
}

#[test]
fn malformed_consumed_identity_is_unavailable_and_revision_reset_recovers() {
    let b = block(1);
    let mut ledger = Ledger::default();
    header(&mut ledger, &b);
    let mut malformed = log(&b, 0);
    malformed.block_hash = None;
    observe(&mut ledger, malformed);
    assert_eq!(
        ledger
            .compare(&b, &interests(), Some(&reference(&b, &[0])))
            .unavailable_blocks(),
        1
    );
    ledger.clear();
    header(&mut ledger, &b);
    observe(&mut ledger, log(&b, 0));
    assert_eq!(
        ledger
            .compare(&b, &interests(), Some(&reference(&b, &[0])))
            .equal_blocks(),
        1
    );
}

#[path = "performance.rs"]
mod performance;
