//! Bounded, observation-only input clocks. Never alters assembly deadlines,
//! canonicality, readiness, or the legacy timing used by execution consumers.
use super::*;
use alloy_primitives::B256;
use evm_fork_cache::reactive::CanonicalIngressTiming;

pub(super) fn fingerprint(log: &RpcLog) -> B256 {
    let mut hash = alloy_primitives::Keccak256::new();
    hash.update(log.inner.address);
    hash.update((log.inner.data.topics().len() as u64).to_be_bytes());
    for topic in log.inner.data.topics() {
        hash.update(topic);
    }
    hash.update(&log.inner.data.data);
    hash.update(log.transaction_index.unwrap_or(u64::MAX).to_be_bytes());
    hash.update([u8::from(log.removed)]);
    hash.finalize()
}

const MAX_BLOCKS: usize = 64;
const MAX_LOGS: usize = 4096;

#[derive(Default)]
pub(super) struct InputClocks {
    blocks: BTreeMap<(u64, alloy_primitives::B256), BlockClocks>,
}
struct BlockClocks {
    first_driver_input: Instant,
    header: Option<CanonicalIngressTiming>,
    header_seen: bool,
    logs: BTreeMap<
        (alloy_primitives::B256, u64),
        (Option<CanonicalIngressTiming>, alloy_primitives::B256),
    >,
    overflow: bool,
}
impl InputClocks {
    pub(super) fn observe(
        &mut self,
        record: &evm_fork_cache::reactive::SubscriberInputRecord<Ethereum>,
        driver_input: Instant,
    ) {
        let identity = match &record.record().input {
            ReactiveInput::Log(log) => log.block_number.zip(log.block_hash),
            ReactiveInput::BlockHeader(header) => Some((header.inner.number, header.hash)),
            _ => None,
        };
        let Some(key) = identity else {
            return;
        };
        if !self.blocks.contains_key(&key) && self.blocks.len() == MAX_BLOCKS {
            // Do not evict and then let an old duplicate manufacture a new root.
            // Missing clocks are explicit; normal retirement permits later blocks.
            return;
        }
        let block = self.blocks.entry(key).or_insert_with(|| BlockClocks {
            first_driver_input: driver_input,
            header: None,
            header_seen: false,
            logs: BTreeMap::new(),
            overflow: false,
        });
        match &record.record().input {
            ReactiveInput::BlockHeader(_) => {
                if !block.header_seen {
                    block.header_seen = true;
                    block.header = record.canonical_timing();
                }
            }
            ReactiveInput::Log(log) => {
                let Some(key) = log.transaction_hash.zip(log.log_index) else {
                    return;
                };
                if !block.logs.contains_key(&key) && block.logs.len() == MAX_LOGS {
                    block.overflow = true;
                    return;
                }
                block
                    .logs
                    .entry(key)
                    .or_insert((record.canonical_timing(), fingerprint(log)));
            }
            _ => {}
        }
    }

    pub(super) fn capture(
        &self,
        number: u64,
        hash: alloy_primitives::B256,
        events: impl Iterator<Item = (alloy_primitives::B256, u64, alloy_primitives::B256)>,
        assembly_released: Instant,
        prepared: Instant,
        reconciliation_started: Option<Instant>,
    ) -> Option<super::super::snapshot::AmmCanonicalInputTiming> {
        let block = self.blocks.get(&(number, hash))?;
        let mut first_event: Option<CanonicalIngressTiming> = None;
        let mut last_event_received = None;
        let mut event_count = 0_u64;
        let mut missing_events = 0_u64;
        for (tx, index, digest) in events {
            event_count = event_count.saturating_add(1);
            let timing = (!block.overflow)
                .then(|| {
                    block
                        .logs
                        .get(&(tx, index))
                        .filter(|(_, observed)| *observed == digest)
                        .and_then(|(timing, _)| *timing)
                })
                .flatten();
            if let Some(timing) = timing {
                if first_event
                    .is_none_or(|first| timing.receipt().received() < first.receipt().received())
                {
                    first_event = Some(timing);
                }
                last_event_received = Some(
                    last_event_received.map_or(timing.receipt().received(), |last: Instant| {
                        last.max(timing.receipt().received())
                    }),
                );
            } else {
                missing_events = missing_events.saturating_add(1);
            }
        }
        Some(super::super::snapshot::AmmCanonicalInputTiming {
            first_event,
            last_event_received,
            header: block.header,
            event_count,
            missing_events,
            driver_first_input: block.first_driver_input,
            assembly_released,
            prepared,
            reconciliation_started,
        })
    }
    pub(super) fn clear(&mut self) {
        self.blocks.clear();
    }
    pub(super) fn retire_through(&mut self, number: u64) {
        self.blocks.retain(|(block, _), _| *block > number);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use evm_fork_cache::reactive::TransportReceipt;
    #[test]
    fn grace_reconciliation_and_unknown_fetched_events_preserve_original_clocks() {
        let now = Instant::now();
        let receipt = TransportReceipt::new(B256::repeat_byte(1), now, now).unwrap();
        let input = CanonicalIngressTiming::new(receipt, now, now).unwrap();
        let hash = B256::repeat_byte(2);
        let tx = B256::repeat_byte(3);
        let mut clocks = InputClocks::default();
        clocks.blocks.insert(
            (100, hash),
            BlockClocks {
                first_driver_input: now,
                header: None,
                header_seen: true,
                logs: BTreeMap::from([((tx, 0), (Some(input), hash))]),
                overflow: false,
            },
        );
        let later = now + Duration::from_millis(120);
        let original = clocks
            .capture(100, hash, [(tx, 0, hash)].into_iter(), later, later, None)
            .unwrap();
        let repaired = clocks
            .capture(
                100,
                hash,
                [(tx, 0, hash), (tx, 1, hash)].into_iter(),
                later,
                later,
                Some(later),
            )
            .unwrap();
        assert_eq!(original.first_event.unwrap().receipt().received(), now);
        assert_eq!(repaired.first_event, original.first_event);
        assert_eq!(repaired.missing_events, 1);
        assert_eq!(repaired.event_count, 2);
        assert!(
            clocks
                .capture(
                    100,
                    B256::repeat_byte(9),
                    [(tx, 0, hash)].into_iter(),
                    later,
                    later,
                    None
                )
                .is_none()
        );
        clocks.retire_through(100);
        assert!(
            clocks
                .capture(100, hash, [(tx, 0, hash)].into_iter(), later, later, None)
                .is_none()
        );
    }
}
