//! Bounded retention of the already validated canonical input records.
use alloy_rpc_types_eth::Log;

pub(crate) const MAX_LOGS: usize = 4_096;
pub(crate) const MAX_BYTES: usize = 2 * 1024 * 1024;
// Conservative retained envelope allowance, including address, all optional
// block/transaction metadata, lengths and allocation bookkeeping. Topics and
// data are accounted separately; this is not JSON wire-byte accounting.
const ENVELOPE_BYTES: usize = 256;

pub(crate) fn checked_size<'a>(logs: impl IntoIterator<Item = &'a Log>) -> Option<usize> {
    let mut count = 0_usize;
    let mut bytes = 0_usize;
    for log in logs {
        count = count.checked_add(1)?;
        if count > MAX_LOGS || log.topics().len() > 4 {
            return None;
        }
        bytes = bytes
            .checked_add(ENVELOPE_BYTES)?
            .checked_add(log.topics().len().checked_mul(32)?)?
            .checked_add(log.inner.data.data.len())?;
        if bytes > MAX_BYTES {
            return None;
        }
    }
    Some(bytes)
}
