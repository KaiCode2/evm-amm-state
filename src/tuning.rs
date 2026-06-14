//! Runtime tuning knobs for AMM state synchronization.

use alloy_primitives::Address;
use std::sync::atomic::{AtomicU8, Ordering};

/// Controls RPC consumption for account and storage prefetch work.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum SyncSpeedMode {
    Fast = 0,
    Normal = 1,
    #[default]
    Slow = 2,
    XSlow = 3,
}

impl SyncSpeedMode {
    fn from_u8(value: u8) -> Self {
        match value {
            0 => Self::Fast,
            1 => Self::Normal,
            2 => Self::Slow,
            3 => Self::XSlow,
            _ => Self::Slow,
        }
    }
}

static SYNC_SPEED_MODE: AtomicU8 = AtomicU8::new(SyncSpeedMode::Slow as u8);

/// Set the process-wide AMM sync speed mode.
pub fn set_sync_speed_mode(mode: SyncSpeedMode) {
    SYNC_SPEED_MODE.store(mode as u8, Ordering::Relaxed);
}

/// Return the current process-wide AMM sync speed mode.
pub fn sync_speed_mode() -> SyncSpeedMode {
    SyncSpeedMode::from_u8(SYNC_SPEED_MODE.load(Ordering::Relaxed))
}

/// Protocol-specific addresses used by discovery and flavor detection.
#[derive(Clone, Debug, Default)]
pub struct ProtocolAddresses {
    pub balancer_v2_vault: Option<Address>,
    pub balancer_v3_vault: Option<Address>,
    pub uniswap_v3_factory: Option<Address>,
    pub pancake_v3_factory: Option<Address>,
    pub slipstream_factory: Option<Address>,
    pub solidly_router: Option<Address>,
    pub universal_router: Option<Address>,
    pub permit2: Option<Address>,
    pub uniswap_v2_factory: Option<Address>,
    pub sushiswap_v2_factory: Option<Address>,
}

impl ProtocolAddresses {
    /// Construct an empty protocol-address set.
    pub fn none() -> Self {
        Self::default()
    }
}
