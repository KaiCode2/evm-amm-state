//! A real-time AMM state engine built on a forked-EVM state cache
//! ([`evm_fork_cache`]).
//!
//! `evm-amm-state` composes a forked-EVM state cache ([`evm_fork_cache`]) with a
//! set of protocol [`adapters`] into a pipeline that tracks a working set of
//! AMMs, cold-starts their on-chain state into the cache, keeps exact-write
//! protocols current straight from chain log events, and emits bounded repair
//! requests for protocols whose events don't carry final storage values. Once a
//! pool's quote read-set is warmed and current, swap simulations run fast and
//! offline against it.
//!
//! The pieces, roughly in pipeline order:
//!
//! - [`adapters`] — per-protocol adapters (Uniswap V2, the Uniswap V3 family,
//!   Balancer V2, Solidly V2, and Curve — StableSwap/NG + CryptoSwap/Tricrypto-NG)
//!   over a single [`adapters::AmmAdapter`] trait. Each adapter knows how to cold-start
//!   a pool's storage into an [`evm_fork_cache::cache::EvmCache`], which log
//!   events to subscribe to, how to apply those events reactively, and how to
//!   `simulate_swap` against the cached state. The
//!   [`adapters::AdapterRegistry`] dispatches by pool key, and
//!   [`adapters::AmmSyncEngine`] drives the resync-capable live path on top of
//!   [`adapters::AmmReactiveHandler`] and the `evm_fork_cache` reactive runtime.
//!
//! See the crate's `examples/adapter_pipeline.rs` for an end-to-end demo that
//! cold-starts a pool, subscribes to its events over a WebSocket endpoint,
//! applies them reactively, and simulates a swap against the live-synced state.

// The public API is broad and stability-sensitive; require docs on every public
// item so the surface stays fully documented as it grows (CI's `-D warnings`
// promotes this to an error).
#![warn(missing_docs)]
// docs.rs builds with `--cfg docsrs` (see `[package.metadata.docs.rs]`), which
// enables nightly `doc_cfg`: every feature-gated item renders with its
// "Available on crate feature … only" badge, derived automatically from the
// existing `#[cfg]`s with no per-item annotations (auto-cfg is part of
// `doc_cfg` since 1.92, rust-lang/rust#138907). Inert on stable builds.
#![cfg_attr(docsrs, feature(doc_cfg))]

/// Re-export of [`alloy_primitives`]: the `Address` / `U256` / `B256` / `Log`
/// vocabulary this crate's API speaks.
///
/// Import from here (`evm_amm_state::alloy_primitives`) to use exactly the
/// version this crate's signatures expect without pinning `alloy-primitives`
/// yourself.
pub use alloy_primitives;
/// Re-export of the [`evm_fork_cache`] companion crate: `EvmCache`, the
/// reactive runtime, storage programs, and the typed errors that appear on
/// this crate's driver seam.
///
/// `evm-fork-cache` is a 0.x **public dependency**: a semver-breaking bump
/// there (e.g. 0.2 → 0.3) is necessarily a breaking release of this crate too,
/// and the two are released in lockstep. Importing it through this re-export
/// (`evm_amm_state::evm_fork_cache`) guarantees the versions match.
pub use evm_fork_cache;

// Always compiled — the adapter layer has no heavy deps.
pub mod adapters;

/// Compiles the README's code samples as doctests so the quickstart cannot
/// drift from the real API. Exists only under `cfg(doctest)` — it is never
/// part of the built crate or the rendered docs. Gated on `uniswap-v3`
/// because the quickstart registers the V3-family adapter; the default and
/// all-features test runs (local + CI) still compile it.
#[cfg(all(doctest, feature = "uniswap-v3"))]
#[doc = include_str!("../README.md")]
pub struct ReadmeDoctests;
