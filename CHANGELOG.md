# Changelog

All notable changes to `evm-amm-state` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

The first public release (`0.1.0`) is **pending**. Publishing to crates.io is
blocked on the [`evm-fork-cache`] companion crate being published — it is
currently consumed as a git pin ([`Cargo.toml`]), and `cargo publish` rejects
git dependencies. See [`RELEASING.md`](RELEASING.md) for the unblock steps.

Everything below ships in `main` and is the intended content of `0.1.0`.

### Added

**Adapters pipeline** — a single [`AmmAdapter`] trait with five stages:
register → cold-start → subscribe → react → simulate.

- `AdapterRegistry` dispatches by `PoolKey`; `AmmReactiveHandler` bridges the
  adapters into the `evm_fork_cache` `ReactiveRuntime`; `AdapterDriver` applies
  decoded logs to an `AdapterCache` in caller order.
- `run_deferred` driver executes the `DeferredWork` produced by `Lazy`
  cold-start (warming the deferred read-set on demand).

**Protocols** (each behind its own feature flag, all on by `default`):

- **Uniswap V2** — `getAmountsOut` quotes; named-slot cold-start; `Sync` →
  exact masked reserve write (event-sourced, no refetch).
- **Uniswap V3 family** (V3, PancakeSwap V3, Slipstream) — `QuoterV2`
  quotes; slot0 + liquidity + a bounded, fixed-radius **multi-word tick-window
  warm-up** at cold-start; `Swap` → slot0/liquidity, `Mint`/`Burn` → tick-range
  resync.
- **Balancer V2** — `Vault.queryBatchSwap` quotes; discover→verify cold-start
  (`getPoolTokens` read-set); `Swap` → balance-slot resync.
- **Solidly V2** (Aerodrome / Velodrome) — pool `getAmountOut` quotes;
  config-supplied storage layout; `Sync` → two exact slot writes.
- **Curve** — StableSwap, StableSwap-NG, CryptoSwap v2, and Tricrypto-NG
  dialects through one adapter; pool `get_dy` quotes; discover→verify
  cold-start; `TokenExchange` + liquidity events → discovered-slot resync.
  Event signatures and `get_dy` ABIs verified on-chain per dialect.

**Simulation** — `simulate_swap` runs each pool's **own** canonical on-chain
quote entrypoint inside a local revm against the warmed cache, then decodes the
result. There is **no reimplemented AMM math**.

**Reactive synchronization** — fully offline (no RPC in the hot path). Pools
whose events carry absolute state are event-sourced with exact writes (Uniswap
V2 / Solidly `Sync`); pools whose events carry deltas re-verify just the
affected slots (Uniswap V3 tick ranges, Balancer / Curve `VerifySlots`).

**Testing & CI**

- Unit, offline reactive, cold-start adoption, and full register→cold-start→
  react→simulate pipeline tests.
- Env-gated, `#[ignore]`d network tests: RPC parity (fork at a pinned block,
  cold-start a real pool, assert `simulate_swap` == on-chain `eth_call` quote —
  mainnet pools plus a Base pool for Solidly) and a live WebSocket soak that
  keeps state in sync from events only.
- CI runs fmt, clippy (all-features + a **per-protocol isolation matrix** +
  no-default-features), tests (all-features / default / no-default), doc
  (`-D warnings`), and a heavy-dependency leak guard.

### Design notes

- **No reimplemented AMM math.** Every quote is the pool's own on-chain
  entrypoint executed in revm, so quotes cannot drift from the live contracts.
- **Adapters-only crate.** The legacy `LocalAMM` / `amm-math` /
  `configured_amms` / arbitrage-routing simulation+search path was removed
  during development (it was never part of a release). A routing/arbitrage layer
  is rebuildable on top of `simulate_swap`.

[`evm-fork-cache`]: https://github.com/KaiCode2/evm-fork-cache
[`Cargo.toml`]: Cargo.toml
[`AmmAdapter`]: src/adapters/traits.rs
[Unreleased]: https://github.com/KaiCode2/evm-amm-state/commits/main
