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

pub mod adapters;
pub mod amm_wrapper;
pub mod balancer_pool;
pub mod balancer_v3_pool;
pub mod cache_sync;
pub mod configured_amms;
pub mod cryptoswap_math;
pub mod curve_pool;
pub mod data;
pub mod discovery;
pub mod events;
mod progress;
pub mod routing;
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
