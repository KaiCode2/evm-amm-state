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
    unavailable_through: Option<u64>,
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
        input: &ReactiveInput<Ethereum>,
        timing: Option<CanonicalIngressTiming>,
        driver_input: Instant,
    ) {
        let identity = match input {
            ReactiveInput::Log(log) => log.block_number.zip(log.block_hash),
            ReactiveInput::BlockHeader(header) => Some((header.inner.number, header.hash)),
            _ => None,
        };
        let Some(key) = identity else {
            return;
        };
        if !self.blocks.contains_key(&key) {
            if self
                .unavailable_through
                .is_some_and(|height| key.0 <= height)
            {
                return;
            }
            if self.blocks.len() == MAX_BLOCKS {
                // Remember refusal after retirement frees capacity: a duplicate
                // of this input must never manufacture a later, shorter root.
                self.mark_unavailable_through(key.0);
                return;
            }
        }
        let block = self.blocks.entry(key).or_insert_with(|| BlockClocks {
            first_driver_input: driver_input,
            header: None,
            header_seen: false,
            logs: BTreeMap::new(),
            overflow: false,
        });
        match input {
            ReactiveInput::BlockHeader(_) => {
                if !block.header_seen {
                    block.header_seen = true;
                    block.header = timing;
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
                let digest = fingerprint(log);
                block
                    .logs
                    .entry(key)
                    .and_modify(|(first, observed)| {
                        if *observed != digest {
                            *first = None;
                        }
                    })
                    .or_insert((timing, digest));
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
        self.unavailable_through = None;
    }
    pub(super) fn retire_through(&mut self, number: u64) {
        self.blocks.retain(|(block, _), _| *block > number);
        self.mark_unavailable_through(number);
    }
    fn mark_unavailable_through(&mut self, number: u64) {
        self.unavailable_through = Some(
            self.unavailable_through
                .map_or(number, |old| old.max(number)),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use evm_fork_cache::reactive::TransportReceipt;

    fn timing(at: Instant) -> CanonicalIngressTiming {
        CanonicalIngressTiming::new(
            TransportReceipt::new(B256::repeat_byte(1), at, at).unwrap(),
            at,
            at,
        )
        .unwrap()
    }
    fn log(number: u64, index: u64) -> RpcLog {
        RpcLog {
            block_number: Some(number),
            block_hash: Some(B256::repeat_byte(2)),
            transaction_hash: Some(B256::repeat_byte(3)),
            log_index: Some(index),
            ..Default::default()
        }
    }
    fn capture(
        clocks: &InputClocks,
        log: &RpcLog,
        at: Instant,
    ) -> Option<super::super::super::snapshot::AmmCanonicalInputTiming> {
        clocks.capture(
            log.block_number.unwrap(),
            log.block_hash.unwrap(),
            [(
                log.transaction_hash.unwrap(),
                log.log_index.unwrap(),
                fingerprint(log),
            )]
            .into_iter(),
            at,
            at,
            None,
        )
    }
    #[test]
    fn refused_capacity_cannot_be_resurrected_by_duplicate_after_retirement() {
        let now = Instant::now();
        let mut clocks = InputClocks::default();
        for number in 1..=64 {
            clocks.observe(&ReactiveInput::Log(log(number, 0)), Some(timing(now)), now);
        }
        let refused = log(65, 0);
        clocks.observe(&ReactiveInput::Log(refused.clone()), Some(timing(now)), now);
        assert!(capture(&clocks, &refused, now).is_none());
        clocks.retire_through(1);
        let later = now + Duration::from_millis(120);
        clocks.observe(
            &ReactiveInput::Log(refused.clone()),
            Some(timing(later)),
            later,
        );
        assert!(capture(&clocks, &refused, later).is_none());
        assert_eq!(clocks.blocks.len(), 63);
        let next = log(66, 0);
        clocks.observe(
            &ReactiveInput::Log(next.clone()),
            Some(timing(later)),
            later,
        );
        assert_eq!(
            capture(&clocks, &next, later).unwrap().first_event,
            Some(timing(later))
        );
        // Existing retained roots stay valid despite a later dropped height.
        assert_eq!(
            capture(&clocks, &log(2, 0), later).unwrap().first_event,
            Some(timing(now))
        );
        // Retired records cannot create a new root either.
        clocks.observe(&ReactiveInput::Log(log(1, 0)), Some(timing(later)), later);
        assert!(capture(&clocks, &log(1, 0), later).is_none());
        clocks.clear();
        clocks.observe(
            &ReactiveInput::Log(refused.clone()),
            Some(timing(later)),
            later,
        );
        assert!(capture(&clocks, &refused, later).is_some());
    }
    #[test]
    fn unknown_first_header_and_log_are_not_upgraded_by_later_duplicate() {
        let now = Instant::now();
        let later = now + Duration::from_millis(100);
        let mut clocks = InputClocks::default();
        let item = log(100, 0);
        let mut header: RpcHeader = RpcHeader::default();
        header.inner.number = 100;
        header.hash = item.block_hash.unwrap();
        clocks.observe(&ReactiveInput::BlockHeader(header.clone()), None, now);
        clocks.observe(&ReactiveInput::Log(item.clone()), None, now);
        clocks.observe(
            &ReactiveInput::BlockHeader(header),
            Some(timing(later)),
            later,
        );
        clocks.observe(
            &ReactiveInput::Log(item.clone()),
            Some(timing(later)),
            later,
        );
        let captured = capture(&clocks, &item, later).unwrap();
        assert_eq!(captured.driver_first_input, now);
        assert!(captured.header.is_none());
        assert!(captured.first_event.is_none());
        assert_eq!(captured.missing_events, 1);
    }
    #[test]
    fn log_capacity_overflow_keeps_the_whole_block_unknown_after_duplicates() {
        let now = Instant::now();
        let later = now + Duration::from_millis(100);
        let mut clocks = InputClocks::default();
        for index in 0..=MAX_LOGS {
            clocks.observe(
                &ReactiveInput::Log(log(100, index as u64)),
                Some(timing(now)),
                now,
            );
        }
        for index in [0, MAX_LOGS] {
            let item = log(100, index as u64);
            clocks.observe(
                &ReactiveInput::Log(item.clone()),
                Some(timing(later)),
                later,
            );
            let captured = capture(&clocks, &item, later).unwrap();
            assert_eq!(captured.driver_first_input, now);
            assert_eq!(captured.missing_events, 1);
            assert!(captured.first_event.is_none());
            assert!(captured.last_event_received.is_none());
        }
        let block = clocks
            .blocks
            .get(&(100, log(100, 0).block_hash.unwrap()))
            .unwrap();
        assert!(block.overflow);
        assert_eq!(block.logs.len(), MAX_LOGS);
    }
    #[test]
    fn exact_content_and_reconciliation_preserve_original_receipt_or_unknown() {
        let now = Instant::now();
        let later = now + Duration::from_millis(120);
        let mut clocks = InputClocks::default();
        let item = log(100, 0);
        clocks.observe(&ReactiveInput::Log(item.clone()), Some(timing(now)), now);
        clocks.observe(
            &ReactiveInput::Log(item.clone()),
            Some(timing(later)),
            later,
        );
        assert_eq!(
            capture(&clocks, &item, later).unwrap().first_event,
            Some(timing(now))
        );
        let fetched = log(100, 1);
        let repaired = clocks
            .capture(
                100,
                item.block_hash.unwrap(),
                [(&item), (&fetched)].into_iter().map(|log| {
                    (
                        log.transaction_hash.unwrap(),
                        log.log_index.unwrap(),
                        fingerprint(log),
                    )
                }),
                later,
                later,
                Some(later),
            )
            .unwrap();
        assert_eq!(repaired.event_count, 2);
        assert_eq!(repaired.missing_events, 1);
        assert_eq!(repaired.first_event, Some(timing(now)));
        assert_eq!(repaired.reconciliation_started, Some(later));
        let mut conflict = item.clone();
        conflict.inner.address = alloy_primitives::Address::repeat_byte(9);
        assert_eq!(
            capture(&clocks, &conflict, later).unwrap().missing_events,
            1
        );
        clocks.observe(
            &ReactiveInput::Log(conflict.clone()),
            Some(timing(later)),
            later,
        );
        assert_eq!(capture(&clocks, &item, later).unwrap().missing_events, 1);
        assert_eq!(
            capture(&clocks, &conflict, later).unwrap().missing_events,
            1
        );
        let mut fork = item.clone();
        fork.block_hash = Some(B256::repeat_byte(9));
        assert!(capture(&clocks, &fork, later).is_none());
        clocks.observe(
            &ReactiveInput::Log(fork.clone()),
            Some(timing(later)),
            later,
        );
        assert_eq!(
            capture(&clocks, &fork, later).unwrap().first_event,
            Some(timing(later))
        );
    }
}
