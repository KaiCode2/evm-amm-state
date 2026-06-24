# E2E WS3 — Cut the legacy path

Manager-owned spec. Make the crate adapters-only and agent-traversable by deleting
the legacy simulation/search stack. The adapters path is **fully self-contained**
(verified: zero `use crate::<non-adapters>` in `src/adapters/`), and the legacy
stack is cleanly isolated behind the `simulation`/`search` features — so this is a
bounded deletion, not a risky untangle.

## Delete

All of these are legacy (behind `#[cfg(feature = "simulation")]` unless noted):
- Modules: `src/amm_wrapper.rs`, `src/balancer_pool.rs`, `src/balancer_v3_pool.rs`,
  `src/cache_sync/`, `src/configured_amms.rs`, `src/cryptoswap_math.rs`,
  `src/curve_pool.rs`, `src/data.rs`, `src/discovery.rs`, `src/events/`,
  `src/progress.rs`, `src/slipstream_pool.rs`, `src/solidly_v2_pool.rs`,
  `src/stableswap_math.rs`, `src/uniswap_v4_pool.rs`, and the inline
  `balancer_math` + `profit` modules in `src/lib.rs`.
- `src/routing/` (behind `#[cfg(feature = "search")]`) — it is built on the legacy
  `LocalAMM`, so it cannot survive the cut. **Treated as legacy: delete it.** (The
  arbitrage search can be rebuilt later on the adapters `simulate_swap`; out of
  scope here.)
- Examples (all legacy — use `EventRouter`/`LocalAMM`/`configured_amms`):
  `examples/event_subscription.rs`, `triangular_arbitrage.rs`,
  `programmatic_loading.rs`, `toml_loading.rs`, `examples/amms.toml`, and their
  `[[example]]` entries in `Cargo.toml`.
- `Cargo.toml`: the `simulation`, `search`, `full-protocols`, and `toml` features
  (and any now-orphaned protocol sub-features they aggregated), plus dependencies
  used ONLY by the deleted code (e.g. the `amms` crate, the AMM-math crates,
  `tracing-subscriber` if only examples used it). Determine orphans by building;
  do NOT remove a dep still used by `adapters`/`tuning`/tests (e.g. `futures` is
  used by `tests/reactive_ws_e2e.rs` — KEEP it as a dev-dep).

## Keep

- `src/adapters/` (entire), `src/tuning.rs` (always-on core), `src/lib.rs` (cleaned
  of the deleted module declarations + inline modules).
- All adapter tests + fixtures: `tests/adapter_a1.rs`, `adapter_core.rs`,
  `adapter_reactive.rs`, `cold_start_adoption.rs`, `adapter_swap_sim.rs`,
  `adapter_swap_sim_rpc.rs`, `pipeline_e2e.rs`, `reactive_ws_e2e.rs`, `tests/fixtures/`.
- The `docs/` specs.

## New default features

Change `default` from `["full-protocols", "simulation", "search", "toml"]` to the
new pipeline: `["adapters", "uniswap-v2", "uniswap-v3", "balancer-v2"]` (the three
finished protocols). Keep the per-protocol + `adapters` features. Ensure
`--no-default-features` still builds (adapters core only).

## New example (replaces the deleted ones)

`examples/adapter_pipeline.rs` — a runnable adapters-path demo: build an
`EvmCache`, register a pool, cold-start it, subscribe to its events over a WS
endpoint, apply them through the reactive runtime, and `simulate_swap` against the
live-synced state. **Template: `tests/reactive_ws_e2e.rs`** (same plumbing). Gate
on an env var (e.g. `ETH_WS_URL`/`E2E_RPC_URL`); print a friendly message and exit
if unset (do not panic). Add its `[[example]]` entry.

## Acceptance criteria

- `cargo build`, `cargo test`, `cargo clippy --all-targets -- -D warnings`, and
  `cargo doc -D warnings` all GREEN on the new default features.
- The full adapter test suite passes unchanged (a1 22, core 4, reactive 28,
  cold_start 12, swap_sim 8, pipeline_e2e 3; live/RPC tests still `#[ignore]`).
- No source/doc/Cargo reference to any deleted module, the `amms` crate, or the
  removed features. `grep -rn "cache_sync\|amm_wrapper\|configured_amms\|EventRouter\|LocalAMM"` in `src/` returns nothing.
- `cargo build --example adapter_pipeline` compiles.
- `cargo build --no-default-features` and `--no-default-features --features adapters,uniswap-v2,uniswap-v3,balancer-v2` both build.
- No behavior change to the adapters path.

## Verification

```
CARGO_NET_GIT_FETCH_WITH_CLI=true cargo build --all-targets
cargo fmt --all --check
cargo clippy --all-targets --no-deps -- -D warnings
cargo clippy --no-default-features --all-targets --no-deps -- -D warnings
cargo test
cargo test --no-default-features
cargo build --example adapter_pipeline
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps
grep -rn "cache_sync\|amm_wrapper\|configured_amms\|EventRouter\|LocalAMM\|stableswap_math" src/ Cargo.toml || echo "clean: no legacy refs"
```

## Notes / prohibitions

- Do NOT touch `src/adapters/` logic or the adapter tests (only remove dead
  references if a deleted module was imported — it is not).
- Do NOT remove a dependency still used by kept code; verify by building.
- If something in the delete-set turns out to be referenced by kept code (it
  should not be, per recon), STOP and report rather than hacking around it.
- Keep the diff to deletions + the lib.rs/Cargo.toml cleanup + the one new example.
