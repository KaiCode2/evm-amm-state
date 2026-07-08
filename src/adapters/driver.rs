use alloy_primitives::Log;

use super::{
    AdapterCache, AdapterEventError, AdapterEventReport, AdapterRegistry, PoolRegistration,
    ProtocolId,
};

/// Error from applying an adapter event via [`AdapterDriver`].
#[non_exhaustive]
#[derive(Debug)]
pub enum DriverError {
    /// No adapter is registered for the routed pool's protocol.
    NoAdapter(ProtocolId),
    /// The adapter's `decode_event` returned a structured error.
    Decode {
        /// The protocol whose adapter reported the error.
        protocol: ProtocolId,
        /// The structured decode error the adapter produced.
        error: AdapterEventError,
    },
}

impl std::fmt::Display for DriverError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoAdapter(protocol) => {
                write!(f, "no adapter registered for protocol {protocol:?}")
            }
            Self::Decode { protocol, error } => {
                write!(f, "adapter decode error for {protocol:?}: {error:?}")
            }
        }
    }
}

impl std::error::Error for DriverError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Decode { error, .. } => Some(error),
            _ => None,
        }
    }
}

/// Applies AMM adapter events to an [`AdapterCache`] in caller-provided order.
#[derive(Clone, Debug)]
pub struct AdapterDriver {
    registry: AdapterRegistry,
}

impl AdapterDriver {
    /// A driver over `registry`.
    pub fn new(registry: AdapterRegistry) -> Self {
        Self { registry }
    }

    /// Borrow the underlying registry.
    pub fn registry(&self) -> &AdapterRegistry {
        &self.registry
    }

    /// Consume the driver, returning its registry.
    pub fn into_registry(self) -> AdapterRegistry {
        self.registry
    }

    /// Route and apply a single log, returning its report (`None` if unrouted).
    /// Returns a [`DriverError`] for a routed-but-malformed log; use
    /// [`apply_logs`](Self::apply_logs) for batch-robust application.
    pub fn apply_log<C>(
        &self,
        cache: &mut C,
        log: &Log,
    ) -> Result<Option<AdapterEventReport>, DriverError>
    where
        C: AdapterCache,
    {
        let Some(pool) = self.registry.route_log(log) else {
            return Ok(None);
        };
        self.apply_routed_log(cache, pool, log)
    }

    /// Apply a batch of logs in order, returning a report per routed-and-decoded
    /// log.
    ///
    /// Batch-robust: a single malformed / undecodable log (a
    /// [`DriverError::Decode`]) is **skipped** so the rest of the batch still
    /// applies — the same contract the reactive runtime path
    /// ([`AmmReactiveHandler`](super::AmmReactiveHandler)) follows. A
    /// [`DriverError::NoAdapter`] is a registry misconfiguration rather than
    /// per-log data, so it still propagates and aborts the batch. Use
    /// [`apply_log`](Self::apply_log) when a caller needs the structured decode
    /// error for an individual log.
    pub fn apply_logs<C>(
        &self,
        cache: &mut C,
        logs: &[Log],
    ) -> Result<Vec<AdapterEventReport>, DriverError>
    where
        C: AdapterCache,
    {
        // Routed-and-decoded logs are the common case; reserve for all of them.
        let mut reports = Vec::with_capacity(logs.len());
        for log in logs {
            match self.apply_log(cache, log) {
                Ok(Some(report)) => reports.push(report),
                Ok(None) => {}
                Err(DriverError::Decode { .. }) => {}
                Err(err @ DriverError::NoAdapter(_)) => return Err(err),
            }
        }
        Ok(reports)
    }

    fn apply_routed_log<C>(
        &self,
        cache: &mut C,
        pool: &PoolRegistration,
        log: &Log,
    ) -> Result<Option<AdapterEventReport>, DriverError>
    where
        C: AdapterCache,
    {
        let protocol = pool.protocol();
        let adapter = self
            .registry
            .adapter(protocol)
            .ok_or(DriverError::NoAdapter(protocol))?;

        let result = adapter.decode_event(pool, log, cache);
        if let Some(error) = result.error {
            return Err(DriverError::Decode { protocol, error });
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
