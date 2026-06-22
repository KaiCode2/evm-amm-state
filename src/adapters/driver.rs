use alloy_primitives::Log;
use anyhow::{Result, anyhow};

use super::{AdapterCache, AdapterEventReport, AdapterRegistry, PoolRegistration, RepairAction};

/// Applies AMM adapter events to an [`AdapterCache`] in caller-provided order.
#[derive(Clone, Debug)]
pub struct AdapterDriver {
    registry: AdapterRegistry,
}

impl AdapterDriver {
    pub fn new(registry: AdapterRegistry) -> Self {
        Self { registry }
    }

    pub fn registry(&self) -> &AdapterRegistry {
        &self.registry
    }

    pub fn into_registry(self) -> AdapterRegistry {
        self.registry
    }

    pub fn apply_log<C>(&self, cache: &mut C, log: &Log) -> Result<Option<AdapterEventReport>>
    where
        C: AdapterCache,
    {
        let Some(pool) = self.registry.route_log(log) else {
            return Ok(None);
        };
        self.apply_routed_log(cache, pool, log)
    }

    pub fn apply_logs<C>(&self, cache: &mut C, logs: &[Log]) -> Result<Vec<AdapterEventReport>>
    where
        C: AdapterCache,
    {
        let mut reports = Vec::new();
        for log in logs {
            if let Some(report) = self.apply_log(cache, log)? {
                reports.push(report);
            }
        }
        Ok(reports)
    }

    fn apply_routed_log<C>(
        &self,
        cache: &mut C,
        pool: &PoolRegistration,
        log: &Log,
    ) -> Result<Option<AdapterEventReport>>
    where
        C: AdapterCache,
    {
        let protocol = pool.protocol();
        let adapter = self
            .registry
            .adapter(protocol)
            .ok_or_else(|| anyhow!("no adapter registered for protocol {protocol:?}"))?;

        let result = adapter.decode_event(pool, log, cache);
        if let Some(error) = result.error {
            return Err(anyhow!("adapter decode error for {protocol:?}: {error:?}"));
        }

        let Some(event) = result.event else {
            return Ok(None);
        };

        let applied = cache.apply_updates(&event.updates);
        let post_apply_repair = adapter.after_apply(pool, &event, &applied);
        let repair = combine_repair(event.repair.clone(), post_apply_repair);

        Ok(Some(AdapterEventReport {
            pool: pool.key.clone(),
            event,
            applied,
            repair,
        }))
    }
}

fn combine_repair(event_repair: RepairAction, post_apply_repair: RepairAction) -> RepairAction {
    match (event_repair, post_apply_repair) {
        (RepairAction::None, repair) | (repair, RepairAction::None) => repair,
        (RepairAction::VerifySlots(mut left), RepairAction::VerifySlots(right)) => {
            for slot in right {
                if !left.contains(&slot) {
                    left.push(slot);
                }
            }
            RepairAction::VerifySlots(left)
        }
        (
            RepairAction::PurgeSlots {
                address: left_address,
                slots: mut left_slots,
            },
            RepairAction::PurgeSlots {
                address: right_address,
                slots: right_slots,
            },
        ) if left_address == right_address => {
            for slot in right_slots {
                if !left_slots.contains(&slot) {
                    left_slots.push(slot);
                }
            }
            RepairAction::PurgeSlots {
                address: left_address,
                slots: left_slots,
            }
        }
        (_, post_apply_repair) => post_apply_repair,
    }
}
