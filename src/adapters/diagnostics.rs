//! Bounded decode causes retained with an immutable state publication.
use super::{AdapterEventError, AmmReactiveSignal, PoolInstanceId};
use evm_fork_cache::reactive::{InputRef, ReactiveBatchReport};

/// Maximum decoder failures retained in one commit. Additional failures are
/// counted by [`AmmDecodeDiagnostics::omitted`].
pub const MAX_COMMITTED_DECODE_ERRORS: usize = 32;

/// Original decoder failure, attributed before subsequent cache invalidation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AmmDecodeDiagnostic {
    pool: PoolInstanceId,
    input: InputRef,
    error: AdapterEventError,
}

impl AmmDecodeDiagnostic {
    /// Generation-scoped pool whose decoder failed.
    pub const fn pool(&self) -> &PoolInstanceId {
        &self.pool
    }
    /// Exact source input, including block/transaction/log identity for a log.
    pub const fn input(&self) -> &InputRef {
        &self.input
    }
    /// Typed original cause. A subsequent storage miss is a separate symptom.
    pub const fn error(&self) -> &AdapterEventError {
        &self.error
    }
    /// Stable, low-cardinality classification suitable for a metric label.
    pub fn error_class(&self) -> &'static str {
        super::reactive::adapter_error_class(&self.error)
    }
}

/// Bounded causes attached to one synchronous report or committed change set.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AmmDecodeDiagnostics {
    entries: Vec<AmmDecodeDiagnostic>,
    omitted: usize,
}

impl AmmDecodeDiagnostics {
    pub(crate) fn from_report(report: &ReactiveBatchReport) -> Self {
        let mut result = Self::default();
        for applied in &report.applied {
            for hook in &applied.hook_signals {
                let Some(AmmReactiveSignal::PoolDecodeError { instance, error }) = hook
                    .payload
                    .as_deref()
                    .and_then(|payload| payload.downcast_ref::<AmmReactiveSignal>())
                else {
                    continue;
                };
                if result.entries.len() == MAX_COMMITTED_DECODE_ERRORS {
                    result.omitted = result.omitted.saturating_add(1);
                    continue;
                }
                let mut error = error.clone();
                if let AdapterEventError::Custom(message) = &mut error {
                    // Third-party adapter strings cannot grow a commit without bound.
                    let mut end = message.len().min(512);
                    while !message.is_char_boundary(end) {
                        end -= 1;
                    }
                    message.truncate(end);
                }
                result.entries.push(AmmDecodeDiagnostic {
                    pool: instance.clone(),
                    input: applied.input_ref,
                    error,
                });
            }
        }
        result
    }
    /// Retained failures in application order; at most 32 entries.
    pub fn entries(&self) -> &[AmmDecodeDiagnostic] {
        &self.entries
    }
    /// Number of additional failures omitted from this publication.
    pub const fn omitted(&self) -> usize {
        self.omitted
    }
}
