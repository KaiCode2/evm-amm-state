use alloy_primitives::Log;
use anyhow::{Result, anyhow};

use super::{AdapterCache, AdapterEventReport, AdapterRegistry, PoolRegistration};

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
        let repair = event.repair.clone().combine(post_apply_repair);

        Ok(Some(AdapterEventReport {
            pool: pool.key.clone(),
            event,
            applied,
            repair,
        }))
    }
}
