//! Typed repair supersession. No provider observation is installed as state.

use super::*;
use evm_fork_cache::reactive::SubscriberOwnerError;

pub(super) fn subscriber_error(
    error: super::super::AmmSubscriberDriverError,
) -> AmmRuntimeCommandError {
    if let super::super::AmmSubscriberDriverError::Owner(owner) = &error
        && let SubscriberOwnerError::BlockMismatch {
            expected_number,
            expected_hash,
            actual_number,
            actual_hash,
        } = owner.as_ref()
    {
        return AmmRuntimeCommandError::SubscriberTargetMismatch {
            expected_number: *expected_number,
            expected_hash: *expected_hash,
            actual_number: *actual_number,
            actual_hash: *actual_hash,
        };
    }
    AmmRuntimeCommandError::Subscriber(error.to_string())
}

fn superseded_points(
    error: &AmmRuntimeCommandError,
    chain_id: u64,
) -> Option<(AmmStatePoint, AmmStatePoint)> {
    match error {
        AmmRuntimeCommandError::StaleBaseline { expected, actual }
            if expected != actual
                && expected.chain_id() == chain_id
                && actual.chain_id() == chain_id =>
        {
            Some((*actual, *expected))
        }
        AmmRuntimeCommandError::SubscriberTargetMismatch {
            expected_number,
            expected_hash,
            actual_number,
            actual_hash,
        } if expected_number == actual_number && expected_hash != actual_hash => Some((
            AmmStatePoint::post_block(chain_id, *expected_number, *expected_hash),
            AmmStatePoint::post_block(chain_id, *actual_number, *actual_hash),
        )),
        _ => None,
    }
}

impl AmmRuntimeActor {
    pub(super) fn resolve_repair_commit(
        &mut self,
        work: Option<&RuntimeWorkId>,
        result: Result<Arc<AmmChangeSet>, AmmRuntimeCommandError>,
    ) -> Result<Arc<AmmChangeSet>, AmmRuntimeCommandError> {
        let (Some(work), Err(error)) = (work, &result) else {
            return result;
        };
        let Some((target, observed)) = superseded_points(error, self.point.chain_id()) else {
            return result;
        };
        let Some(scheduled) = self.scheduled_followups.get(work).cloned() else {
            return result;
        };
        // Only already-fenced, active required repairs qualify. Other provider,
        // ownership, malformed-header and cold-start failures remain failures.
        if scheduled.kind != AmmWorkKind::Repair
            || self.engine.ownership().active_pool(scheduled.pool.key()) != Some(&scheduled.pool)
            || self
                .engine
                .registry()
                .pool(scheduled.pool.key())
                .is_none_or(|pool| pool.status != super::super::PoolStatus::Degraded)
        {
            return result;
        }
        let action = match &scheduled.task {
            AmmFollowUpTask::Refresh { policy, .. } => RepairAction::ColdStart {
                pool: scheduled.pool.key().clone(),
                policy: *policy,
            },
            AmmFollowUpTask::SlotPatch { slots } => RepairAction::VerifySlots(slots.clone()),
        };
        self.next_event_sequence(2)?;
        self.scheduled_followups.remove(work);
        self.pending_pool_followup.remove(&scheduled.pool);
        self.active_work.remove(work);
        self.engine.ownership_mut().untrack_work(work);
        if scheduled.queued {
            self.adjust_queue_depth(scheduled.class, -1);
        }
        self.pending_lifecycles
            .insert(scheduled.pool.clone(), PoolRuntimeState::Degraded);
        if self.point == target {
            // The conflicting RPC hash is not a new trusted target. Wait for
            // normal canonical delivery; never spin/retry against the same pin.
            self.repair_waiting_for_canonical
                .insert(scheduled.pool.clone(), target);
        }
        self.retain_followup_intent(
            scheduled.pool.clone(),
            AmmPendingFollowUpIntent::Repair(action),
        );
        self.publish_runtime_events(vec![
            AmmRuntimeEventKind::PoolLifecycleTransition {
                pool: scheduled.pool,
                from: PoolRuntimeState::CatchingUp,
                to: PoolRuntimeState::Degraded,
            },
            AmmRuntimeEventKind::WorkSuperseded {
                work: work.clone(),
                target,
                observed,
            },
        ])?;
        self.drain_followup_intents();
        // The worker's final failure acknowledgement is now stale and ignored.
        // Only the supersession event terminates this attempt for observers.
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::B256;

    #[test]
    fn only_typed_identity_replacements_or_stale_baselines_are_superseded() {
        let mismatch =
            |actual_number, actual_hash| AmmRuntimeCommandError::SubscriberTargetMismatch {
                expected_number: 25_991_891,
                expected_hash: B256::repeat_byte(1),
                actual_number,
                actual_hash,
            };
        assert!(superseded_points(&mismatch(25_991_891, B256::repeat_byte(2)), 1).is_some());
        assert!(superseded_points(&mismatch(25_991_892, B256::repeat_byte(2)), 1).is_none());
        assert!(superseded_points(&mismatch(25_991_891, B256::repeat_byte(1)), 1).is_none());
        assert!(
            superseded_points(
                &AmmRuntimeCommandError::Subscriber("baseline is stale".into()),
                1
            )
            .is_none()
        );
        assert!(
            superseded_points(
                &AmmRuntimeCommandError::StaleBaseline {
                    expected: AmmStatePoint::post_block(1, 502, B256::repeat_byte(2)),
                    actual: AmmStatePoint::post_block(1, 501, B256::repeat_byte(1)),
                },
                1
            )
            .is_some()
        );
    }
}
