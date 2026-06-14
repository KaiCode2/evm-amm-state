//! Minimal progress facade for long-running sync operations.

use tracing::info;

pub(crate) struct ProgressBar {
    len: u64,
    prefix: String,
}

pub(crate) fn progress_bar(len: u64, prefix: &str) -> ProgressBar {
    ProgressBar {
        len,
        prefix: prefix.to_string(),
    }
}

impl ProgressBar {
    pub(crate) fn set_message(&self, _msg: impl Into<String>) {}

    pub(crate) fn inc(&self, _delta: u64) {}
}

pub(crate) fn finish_with_message(pb: &ProgressBar, msg: &str) {
    info!(
        prefix = pb.prefix,
        total = pb.len,
        message = msg,
        "Progress checkpoint"
    );
}
