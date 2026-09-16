//! Validate the actor's event-free branch proof against the actual rollback.

use evm_fork_cache::reactive::{BlockRef, InputRef, ReactiveBatchReport, ReactiveReport};

pub(super) fn verify_event_free_reorg(report: &ReactiveBatchReport, proof: &[BlockRef]) -> bool {
    if proof.is_empty() {
        return false;
    }
    let mut reorgs = report.reports.iter().filter_map(|report| {
        if let ReactiveReport::Reorg(reorg) = report.as_ref() {
            Some(reorg)
        } else {
            None
        }
    });
    let Some(reorg) = reorgs.next() else {
        return false;
    };
    reorgs.next().is_none()
        && reorg.dropped_blocks.len() == proof.len()
        && reorg
            .dropped_blocks
            .iter()
            .zip(proof)
            .all(|(actual, expected)| {
                actual.number == expected.number && actual.hash == expected.hash
            })
        && reorg
            .dropped_inputs
            .iter()
            .all(|input| matches!(input, InputRef::Block { .. }))
        && reorg.rollback_updates.is_empty()
        && reorg.purge_updates.is_empty()
        && reorg.canceled_resyncs.is_empty()
}
