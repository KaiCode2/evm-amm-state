//! Bounded, opt-in comparison at the driver-consumed fetch-start cutoff.
//!
//! This is diagnostic evidence, never canonical authority or proof of provider
//! loss. No input is drained while the existing reconciliation future awaits.
use super::*;
use alloy_primitives::B256;
use std::sync::atomic::AtomicBool;

const MAX_BLOCKS: usize = 64;
const MAX_LOGS: usize = 4096;

/// Cumulative comparison outcomes for one driver lifetime.
///
/// Exactly one block outcome is counted per attempted existing reconciliation.
/// Log counts exclude unavailable comparisons and deduplicate global log indices.
/// `missing` means absent in consumed subscription input at fetch start; queued,
/// late, upstream-deduplicated or backfilled data is not evidence of permanent loss.
/// Snapshots use relaxed atomics and may span a concurrent counter update.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReconciliationObservationStats {
    equal_blocks: u64,
    missing_blocks: u64,
    conflicting_blocks: u64,
    unavailable_blocks: u64,
    reference_logs: u64,
    consumed_logs: u64,
    missing_logs: u64,
    unexpected_logs: u64,
    conflicting_logs: u64,
}

impl ReconciliationObservationStats {
    /// Comparisons whose unique reference and consumed sets match, including both empty.
    pub const fn equal_blocks(self) -> u64 {
        self.equal_blocks
    }
    /// Comparisons with reference logs absent at cutoff and no conflicting or extra log.
    pub const fn missing_blocks(self) -> u64 {
        self.missing_blocks
    }
    /// Comparisons containing conflicting content or consumed logs absent from the reference.
    pub const fn conflicting_blocks(self) -> u64 {
        self.conflicting_blocks
    }
    /// Comparisons without valid complete reference and observation evidence.
    pub const fn unavailable_blocks(self) -> u64 {
        self.unavailable_blocks
    }
    /// Unique matching logs in comparable exact-hash reference responses.
    pub const fn reference_logs(self) -> u64 {
        self.reference_logs
    }
    /// Unique consumed subscription log indices in comparable buffers.
    pub const fn consumed_logs(self) -> u64 {
        self.consumed_logs
    }
    /// Reference logs absent from the consumed buffer at cutoff.
    pub const fn missing_logs(self) -> u64 {
        self.missing_logs
    }
    /// Consumed log indices absent from the matching reference set.
    pub const fn unexpected_logs(self) -> u64 {
        self.unexpected_logs
    }
    /// Log indices with differing contents or contradictory consumed duplicates.
    pub const fn conflicting_logs(self) -> u64 {
        self.conflicting_logs
    }
}

#[derive(Debug, Default)]
pub(super) struct Counters {
    enabled: AtomicBool,
    equal_blocks: AtomicU64,
    missing_blocks: AtomicU64,
    conflicting_blocks: AtomicU64,
    unavailable_blocks: AtomicU64,
    reference_logs: AtomicU64,
    consumed_logs: AtomicU64,
    missing_logs: AtomicU64,
    unexpected_logs: AtomicU64,
    conflicting_logs: AtomicU64,
}
impl Counters {
    pub(super) fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, AtomicOrdering::Relaxed);
    }
    pub(super) fn snapshot(&self) -> Option<ReconciliationObservationStats> {
        self.enabled
            .load(AtomicOrdering::Relaxed)
            .then(|| ReconciliationObservationStats {
                equal_blocks: self.equal_blocks.load(AtomicOrdering::Relaxed),
                missing_blocks: self.missing_blocks.load(AtomicOrdering::Relaxed),
                conflicting_blocks: self.conflicting_blocks.load(AtomicOrdering::Relaxed),
                unavailable_blocks: self.unavailable_blocks.load(AtomicOrdering::Relaxed),
                reference_logs: self.reference_logs.load(AtomicOrdering::Relaxed),
                consumed_logs: self.consumed_logs.load(AtomicOrdering::Relaxed),
                missing_logs: self.missing_logs.load(AtomicOrdering::Relaxed),
                unexpected_logs: self.unexpected_logs.load(AtomicOrdering::Relaxed),
                conflicting_logs: self.conflicting_logs.load(AtomicOrdering::Relaxed),
            })
    }
    pub(super) fn record(&self, result: ReconciliationObservationStats) {
        AmmSubscriberDriverCounters::add(&self.equal_blocks, result.equal_blocks);
        AmmSubscriberDriverCounters::add(&self.missing_blocks, result.missing_blocks);
        AmmSubscriberDriverCounters::add(&self.conflicting_blocks, result.conflicting_blocks);
        AmmSubscriberDriverCounters::add(&self.unavailable_blocks, result.unavailable_blocks);
        AmmSubscriberDriverCounters::add(&self.reference_logs, result.reference_logs);
        AmmSubscriberDriverCounters::add(&self.consumed_logs, result.consumed_logs);
        AmmSubscriberDriverCounters::add(&self.missing_logs, result.missing_logs);
        AmmSubscriberDriverCounters::add(&self.unexpected_logs, result.unexpected_logs);
        AmmSubscriberDriverCounters::add(&self.conflicting_logs, result.conflicting_logs);
    }
}

#[derive(Default)]
pub(super) struct Ledger {
    blocks: BTreeMap<(u64, B256), ObservedBlock>,
    retired_or_refused_through: Option<u64>,
    malformed_identity: bool,
    log_count: usize,
}
#[derive(Default)]
struct ObservedBlock {
    header_seen: bool,
    unavailable: bool,
    logs: BTreeMap<u64, ObservedLog>,
}
struct ObservedLog {
    digest: B256,
    timestamp: Option<u64>,
    conflicting: bool,
}

fn digest(log: &RpcLog) -> B256 {
    let mut hash = alloy_primitives::Keccak256::new();
    hash.update(timing::fingerprint(log));
    hash.update(log.transaction_hash.unwrap_or_default());
    hash.finalize()
}
fn unavailable() -> ReconciliationObservationStats {
    ReconciliationObservationStats {
        unavailable_blocks: 1,
        ..Default::default()
    }
}

impl Ledger {
    pub(super) fn observe(&mut self, record: &ReactiveInputRecord<Ethereum>) {
        let identity = match &record.input {
            ReactiveInput::Log(log) => log.block_number.zip(log.block_hash),
            ReactiveInput::BlockHeader(header) => Some((header.inner.number, header.hash)),
            _ => return,
        };
        let Some(key) = identity else {
            self.malformed_identity = true;
            return;
        };
        if self
            .retired_or_refused_through
            .is_some_and(|number| key.0 <= number)
        {
            return;
        }
        if !self.blocks.contains_key(&key) && self.blocks.len() == MAX_BLOCKS {
            self.mark_unavailable_through(key.0);
            return;
        }
        let observed = self.blocks.entry(key).or_default();
        match &record.input {
            ReactiveInput::BlockHeader(_) => {
                observed.header_seen = record.context.source == InputSource::Subscription;
                observed.unavailable |= !observed.header_seen;
            }
            ReactiveInput::Log(log) => {
                // A backfill may supersede a WS record in upstream deduplication.
                // Do not classify that observational ambiguity as a WS shortfall.
                if record.context.source != InputSource::Subscription
                    || log.removed
                    || log.transaction_hash.is_none()
                    || log.transaction_index.is_none()
                {
                    observed.unavailable = true;
                    return;
                }
                let Some(index) = log.log_index else {
                    observed.unavailable = true;
                    return;
                };
                if !observed.logs.contains_key(&index) && self.log_count == MAX_LOGS {
                    observed.unavailable = true;
                    return;
                }
                let fingerprint = digest(log);
                match observed.logs.entry(index) {
                    std::collections::btree_map::Entry::Vacant(entry) => {
                        entry.insert(ObservedLog {
                            digest: fingerprint,
                            timestamp: log.block_timestamp,
                            conflicting: false,
                        });
                        self.log_count += 1;
                    }
                    std::collections::btree_map::Entry::Occupied(mut entry) => {
                        let first = entry.get_mut();
                        first.conflicting |= first.digest != fingerprint
                            || first
                                .timestamp
                                .zip(log.block_timestamp)
                                .is_some_and(|(a, b)| a != b);
                        first.timestamp = first.timestamp.or(log.block_timestamp);
                    }
                }
            }
            _ => {}
        }
    }

    pub(super) fn compare(
        &self,
        block: &BlockRef,
        interests: &[ReactiveInterest<Ethereum>],
        reference: Option<&BTreeMap<u64, RpcLog>>,
    ) -> ReconciliationObservationStats {
        let Some(observed) = self.blocks.get(&(block.number, block.hash)) else {
            return unavailable();
        };
        let Some(reference) = reference else {
            return unavailable();
        };
        if self.malformed_identity
            || observed.unavailable
            || !observed.header_seen
            || self
                .retired_or_refused_through
                .is_some_and(|n| block.number <= n)
            || reference.len() > MAX_LOGS
        {
            return unavailable();
        }
        // Apply the same interested-log union as the subscriber. Local matchers
        // are existing pure selectors, not new provider requests. Owner revision
        // changes invalidate this ledger before further observations.
        let mut expected = BTreeMap::new();
        for (&index, log) in reference {
            if validated_log_key(log, block).ok() != Some(index) {
                return unavailable();
            }
            let provider_match = interests.iter().any(|interest| match interest {
                ReactiveInterest::Logs(interest) => interest.provider_filter.matches(&log.inner),
                _ => false,
            });
            if !provider_match {
                return unavailable();
            }
            if interests.iter().any(|interest| match interest {
                ReactiveInterest::Logs(interest) => interest.matches(log),
                _ => false,
            }) {
                expected.insert(index, digest(log));
            }
        }
        let mut result = ReconciliationObservationStats {
            reference_logs: expected.len() as u64,
            consumed_logs: observed.logs.len() as u64,
            ..Default::default()
        };
        for (&index, log) in &observed.logs {
            if log
                .timestamp
                .is_some_and(|timestamp| Some(timestamp) != block.timestamp)
            {
                return unavailable();
            }
            if log.conflicting {
                result.conflicting_logs += 1;
            } else if let Some(reference) = expected.get(&index)
                && log.digest != *reference
            {
                result.conflicting_logs += 1;
            }
            if !expected.contains_key(&index) {
                result.unexpected_logs += 1;
            }
        }
        result.missing_logs = expected
            .keys()
            .filter(|index| !observed.logs.contains_key(index))
            .count() as u64;
        if result.conflicting_logs > 0 || result.unexpected_logs > 0 {
            result.conflicting_blocks = 1;
        } else if result.missing_logs > 0 {
            result.missing_blocks = 1;
        } else {
            result.equal_blocks = 1;
        }
        result
    }

    pub(super) fn clear(&mut self) {
        *self = Self::default();
    }
    pub(super) fn retire_through(&mut self, number: u64) {
        self.blocks.retain(|(height, _), _| *height > number);
        self.log_count = self.blocks.values().map(|b| b.logs.len()).sum();
        self.mark_unavailable_through(number);
    }
    fn mark_unavailable_through(&mut self, number: u64) {
        self.retired_or_refused_through = Some(
            self.retired_or_refused_through
                .map_or(number, |old| old.max(number)),
        );
    }
}

#[cfg(test)]
mod tests;
