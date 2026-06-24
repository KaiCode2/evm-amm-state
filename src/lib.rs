//! A real-time AMM state engine built on a forked-EVM state cache
//! ([`evm_fork_cache`]).
//!
//! `evm-amm-state` composes a forked-EVM state cache ([`evm_fork_cache`]) with a
//! set of protocol [`adapters`] into a pipeline that tracks a working set of
//! AMMs, cold-starts their on-chain state into the cache, keeps them current
//! from chain log events with no RPC, and runs fast, fully-offline swap
//! simulations against the live-synced state.
//!
//! The pieces, roughly in pipeline order:
//!
//! - [`adapters`] — per-protocol adapters (Uniswap V2/V3, Balancer V2, …) over a
//!   single [`adapters::AmmAdapter`] trait. Each adapter knows how to cold-start
//!   a pool's storage into an [`evm_fork_cache::cache::EvmCache`], which log
//!   events to subscribe to, how to apply those events reactively, and how to
//!   `simulate_swap` against the cached state. The
//!   [`adapters::AdapterRegistry`] dispatches by pool key, and
//!   [`adapters::AmmReactiveHandler`] bridges the adapters into the
//!   `evm_fork_cache` reactive runtime.
//! - [`tuning`] — always-on tuning knobs shared across the adapters path.
//!
//! See the crate's `examples/adapter_pipeline.rs` for an end-to-end demo that
//! cold-starts a pool, subscribes to its events over a WebSocket endpoint,
//! applies them reactively, and simulates a swap against the live-synced state.

// Always compiled — the adapter layer and lightweight utilities have no heavy deps.
pub mod adapters;
pub mod tuning;
