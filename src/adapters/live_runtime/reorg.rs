//! Bounded proof that both sides of a branch replacement contain no tracked logs.
//!
//! This is an optimization proof, not canonical authority. Losing history or
//! installing non-journaled state discards it. The reactive engine still validates
//! ancestry, performs rollback, and publishes the reorg incident.

use std::collections::VecDeque;

use evm_fork_cache::reactive::BlockRef;

use super::AmmStatePoint;

const RETAINED_BLOCKS: usize = 64;

struct Entry {
    point: AmmStatePoint,
    has_logs: bool,
}

pub(super) struct EventFreeReorgHistory {
    entries: VecDeque<Entry>,
}

impl EventFreeReorgHistory {
    pub(super) fn new(baseline: AmmStatePoint) -> Self {
        let mut history = Self {
            entries: VecDeque::new(),
        };
        history.reset(baseline);
        history
    }

    /// Imported state cannot be rolled back using an empty event journal.
    pub(super) fn reset(&mut self, baseline: AmmStatePoint) {
        self.entries.clear();
        self.entries.push_back(Entry {
            point: baseline,
            has_logs: true,
        });
    }

    pub(super) fn replacement_proof(&self, block: &BlockRef, has_logs: bool) -> Vec<BlockRef> {
        if has_logs {
            return Vec::new();
        }
        let Some(parent) = self.parent_index(block) else {
            return Vec::new();
        };
        let dropped = self.entries.iter().skip(parent + 1);
        if dropped.clone().any(|entry| entry.has_logs) {
            return Vec::new();
        }
        dropped
            .map(|entry| BlockRef {
                number: entry.point.block_number(),
                hash: entry.point.block_hash(),
                parent_hash: None,
                timestamp: None,
            })
            .collect()
    }

    pub(super) fn record(&mut self, block: BlockRef, has_logs: bool, chain_id: u64) {
        let point = AmmStatePoint::post_block(chain_id, block.number, block.hash);
        if let Some(parent) = self.parent_index(&block) {
            self.entries.truncate(parent + 1);
            self.entries.push_back(Entry { point, has_logs });
            while self.entries.len() > RETAINED_BLOCKS {
                self.entries.pop_front();
            }
        } else {
            self.reset(point);
        }
    }

    fn parent_index(&self, block: &BlockRef) -> Option<usize> {
        self.entries.iter().position(|entry| {
            entry.point.block_number().checked_add(1) == Some(block.number)
                && Some(entry.point.block_hash()) == block.parent_hash
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::B256;

    fn point(number: u64, byte: u8) -> AmmStatePoint {
        AmmStatePoint::post_block(1, number, B256::repeat_byte(byte))
    }

    fn block(number: u64, byte: u8, parent: u8) -> BlockRef {
        BlockRef {
            number,
            hash: B256::repeat_byte(byte),
            parent_hash: Some(B256::repeat_byte(parent)),
            timestamp: Some(number),
        }
    }

    #[test]
    fn event_free_proof_requires_both_branches_and_a_retained_ancestor() {
        let mut history = EventFreeReorgHistory::new(point(100, 100));
        history.record(block(101, 101, 100), false, 1);
        history.record(block(102, 102, 101), false, 1);
        assert_eq!(
            history
                .replacement_proof(&block(101, 201, 100), false)
                .len(),
            2
        );
        assert!(
            history
                .replacement_proof(&block(101, 201, 100), true)
                .is_empty()
        );
        assert!(
            history
                .replacement_proof(&block(100, 200, 99), false)
                .is_empty()
        );
        assert!(
            history
                .replacement_proof(&block(101, 201, 99), false)
                .is_empty()
        );
        history.record(block(102, 202, 101), true, 1);
        assert!(
            history
                .replacement_proof(&block(101, 201, 100), false)
                .is_empty()
        );
    }

    #[test]
    fn prepared_state_resets_proof_even_if_the_block_had_no_logs() {
        let mut history = EventFreeReorgHistory::new(point(100, 100));
        history.record(block(101, 101, 100), false, 1);
        history.reset(point(101, 101));
        assert!(
            history
                .replacement_proof(&block(101, 201, 100), false)
                .is_empty()
        );
        history.record(block(102, 102, 101), false, 1);
        assert_eq!(
            history
                .replacement_proof(&block(102, 202, 101), false)
                .len(),
            1
        );
    }

    #[test]
    fn history_is_bounded_and_gaps_discard_proof() {
        let mut history = EventFreeReorgHistory::new(point(0, 0));
        for number in 1u8..100 {
            history.record(block(u64::from(number), number, number - 1), false, 1);
        }
        assert_eq!(history.entries.len(), RETAINED_BLOCKS);
        assert!(
            history
                .replacement_proof(&block(1, 201, 0), false)
                .is_empty()
        );
        history.record(block(105, 105, 104), false, 1);
        assert_eq!(history.entries.len(), 1);
        assert!(
            history
                .replacement_proof(&block(105, 205, 104), false)
                .is_empty()
        );
    }
}
