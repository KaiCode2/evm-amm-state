//! Canonical record retention is bounded and preserves the validated envelope.
use super::*;
use alloy_consensus::Header as ConsensusHeader;
use alloy_primitives::{Address, B256, Bytes, Log, LogData};
use alloy_rpc_types_eth::Log as RpcLog;
use evm_fork_cache::reactive::{InputSource, ReactiveContext, ReactiveInputRecord};

fn header() -> RpcHeader {
    RpcHeader::new(ConsensusHeader {
        number: 42,
        timestamp: 100,
        parent_hash: B256::repeat_byte(9),
        ..Default::default()
    })
}
fn record(header: &RpcHeader, index: u64, bytes: usize) -> ReactiveInputRecord<Ethereum> {
    let block = BlockRef {
        number: header.number,
        hash: header.hash,
        parent_hash: Some(header.parent_hash),
        timestamp: Some(header.timestamp),
    };
    ReactiveInputRecord::new(
        ReactiveInput::Log(RpcLog {
            inner: Log {
                address: Address::repeat_byte(7),
                data: LogData::new(vec![B256::repeat_byte(8)], Bytes::from(vec![1; bytes]))
                    .unwrap(),
            },
            block_hash: Some(header.hash),
            block_number: Some(header.number),
            block_timestamp: Some(header.timestamp),
            transaction_hash: Some(B256::repeat_byte(4)),
            transaction_index: Some(2),
            log_index: Some(index),
            removed: false,
        }),
        ReactiveContext {
            chain_id: Some(1),
            source: InputSource::Batch,
            chain_status: ChainStatus::Included {
                block,
                confirmations: 0,
            },
            block: Some(block),
            transaction_index: Some(2),
            log_index: Some(index),
        },
    )
}
fn batch(
    records: Vec<ReactiveInputRecord<Ethereum>>,
) -> Result<AmmCanonicalBatch, AmmCanonicalBatchError> {
    AmmCanonicalBatch::from_verified_block(1, header(), 0, ReactiveInputBatch::new(records))
}
#[test]
fn bounded_canonical_logs_preserve_full_metadata_and_observed_empty() {
    let h = header();
    let original = record(&h, 3, 96);
    let ReactiveInput::Log(expected) = &original.input else {
        unreachable!()
    };
    let got = batch(vec![original.clone()]).unwrap();
    assert_eq!(got.header.logs().unwrap(), std::slice::from_ref(expected));
    assert_eq!(batch(vec![]).unwrap().header.logs(), Some([].as_slice()));
    assert!(
        AmmCanonicalHeader::from_verified_parts(1, h)
            .logs()
            .is_none()
    );
}
#[test]
fn canonical_log_count_topics_and_total_payload_reject_without_truncation() {
    use super::super::snapshot::canonical_header::logs::{MAX_BYTES, MAX_LOGS};
    let h = header();
    assert!(batch((0..MAX_LOGS).map(|i| record(&h, i as u64, 0)).collect()).is_ok());
    assert!(matches!(
        batch((0..=MAX_LOGS).map(|i| record(&h, i as u64, 0)).collect()),
        Err(AmmCanonicalBatchError::PayloadLimitExceeded)
    ));
    assert!(matches!(
        batch(vec![record(&h, 0, MAX_BYTES)]),
        Err(AmmCanonicalBatchError::PayloadLimitExceeded)
    ));
    let mut too_many = record(&h, 0, 0);
    let ReactiveInput::Log(log) = &mut too_many.input else {
        unreachable!()
    };
    log.inner.data = LogData::new_unchecked(vec![B256::ZERO; 5], Bytes::new());
    assert!(matches!(
        batch(vec![too_many]),
        Err(AmmCanonicalBatchError::PayloadLimitExceeded)
    ));
}
#[test]
fn retained_canonical_logs_require_matching_pin_and_strict_order() {
    let h = header();
    assert!(matches!(
        batch(vec![record(&h, 2, 0), record(&h, 1, 0)]),
        Err(AmmCanonicalBatchError::OutOfOrderRecord { .. })
    ));
    let mut wrong = record(&h, 0, 0);
    let ReactiveInput::Log(log) = &mut wrong.input else {
        unreachable!()
    };
    log.block_hash = Some(B256::repeat_byte(1));
    assert!(matches!(
        batch(vec![wrong]),
        Err(AmmCanonicalBatchError::MalformedRecord { .. })
    ));
}
