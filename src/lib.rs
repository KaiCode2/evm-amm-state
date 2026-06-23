//! A real-time AMM state and routing engine, composed from
//! [`evm_fork_cache`] and [`amm_math`].
//!
//! `evm-amm-state` shows how to compose a forked-EVM state cache
//! ([`evm_fork_cache`]) with deterministic pool math ([`amm_math`]) into a
//! pipeline that tracks a set of AMMs, keeps them current from chain events,
//! and runs fast, parallel, fully-offline swap simulations against them.
//!
//! The pieces, roughly in pipeline order:
//!
//! - [`amm_wrapper::LocalAMM`] — a unified pool enum over Uniswap V2/V3,
//!   PancakeSwap V3, Balancer V2/V3, Curve, Solidly V2, Slipstream, ERC4626,
//!   and a Uniswap V4 stub, implementing the `amms` `AutomatedMarketMaker`
//!   trait so every type simulates through one interface.
//! - [`configured_amms`] — load a working set of pools from an
//!   [`evm_fork_cache::cache::EvmCache`], either from programmatically-built
//!   [`configured_amms::AmmConfigEntry`] records or (optionally) from an
//!   `amms.toml` file behind the `toml` feature.
//! - [`cache_sync`] — initialize and incrementally refresh each pool family
//!   from forked storage, including adaptive V3 tick scanning.
//! - [`events`] — keep pools current from a log subscription: decode swaps and
//!   liquidity events and apply them in place, with no RPC, then optionally
//!   mirror the new state back into the cache.
//! - [`routing`] — enumerate and evaluate multi-leg routes (e.g. triangular
//!   arbitrage) over an immutable pool snapshot, in parallel and fully offline.
//! - [`discovery`] — discover pools for caller-supplied token pairs from
//!   configured factories.
//!
//! See the crate's `examples/` directory for an end-to-end bot that subscribes
//! to a set of pools and searches for 3-leg arbitrage on each update.

// Always compiled — the adapter layer and lightweight utilities have no heavy deps.
pub mod adapters;
pub mod tuning;

// Pure-Rust pool math + cache-sync initialization. Requires `amms` + `amm-math`.
// `progress` is a private helper used only by the simulation sync paths.
#[cfg(feature = "simulation")]
pub mod amm_wrapper;
#[cfg(feature = "simulation")]
pub mod balancer_pool;
#[cfg(feature = "simulation")]
pub mod balancer_v3_pool;
#[cfg(feature = "simulation")]
pub mod cache_sync;
#[cfg(feature = "simulation")]
pub mod configured_amms;
#[cfg(feature = "simulation")]
pub mod cryptoswap_math;
#[cfg(feature = "simulation")]
pub mod curve_pool;
#[cfg(feature = "simulation")]
pub mod data;
#[cfg(feature = "simulation")]
pub mod discovery;
#[cfg(feature = "simulation")]
pub mod events;
#[cfg(feature = "simulation")]
mod progress;
#[cfg(feature = "simulation")]
pub mod slipstream_pool;
#[cfg(feature = "simulation")]
pub mod solidly_v2_pool;
#[cfg(feature = "simulation")]
pub mod stableswap_math;
#[cfg(feature = "simulation")]
pub mod uniswap_v4_pool;

// Parallel multi-leg route search. Requires `rayon` (and simulation).
#[cfg(feature = "search")]
pub mod routing;

/// Re-export shared pool math for convenience.
#[cfg(feature = "simulation")]
pub mod balancer_math {
    pub use amm_math::balancer_math::*;
}

/// Re-export pure profit helpers for convenience.
#[cfg(feature = "simulation")]
pub mod profit {
    pub use amm_math::profit::*;
}
