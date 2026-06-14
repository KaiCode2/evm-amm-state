//! EVM-backed AMM state loading and synchronization.
//!
//! This crate provides pool configuration parsing, forked-state initialization,
//! incremental refresh, and local simulation wrappers for
//! V2/V3/Balancer/Curve/Solidly-style pools.
//!
//! It builds on two companion crates: [`evm_fork_cache`] for the forked EVM
//! state cache, and [`amm_math`] for deterministic pool math.

pub mod amm_wrapper;
pub mod balancer_pool;
pub mod balancer_v3_pool;
pub mod cache_sync;
pub mod configured_amms;
pub mod cryptoswap_math;
pub mod curve_pool;
pub mod data;
pub mod discovery;
mod progress;
pub mod slipstream_pool;
pub mod solidly_v2_pool;
pub mod stableswap_math;
pub mod tuning;
pub mod uniswap_v4_pool;

/// Re-export shared pool math for convenience.
pub mod balancer_math {
    pub use amm_math::balancer_math::*;
}

/// Re-export pure profit helpers for convenience.
pub mod profit {
    pub use amm_math::profit::*;
}
