# Phase S0 (pulled forward): Feature-Model Split

Status: design approved; implementation pending. Decided at the A3 checkpoint
(see `docs/phase-a3-checkpoint.md`); grounded by a per-module dependency mapping.

## Goal

Make `amms`, `amm-math`, and `rayon` optional so an adapter-only build compiles
without them, via three dependency tiers plus per-protocol **adapter** flags.
Back-compat default (current behavior preserved). Per-protocol **simulation**
gating of the `LocalAMM` enum (191 refs across 7 files) is deferred to the S2
`LocalAMM` rebuild — gating it now would be invasive and largely throwaway.

## Tier classification (from the mapping)

- **Always-on (no heavy deps):** `adapters/*`, `tuning`, `progress`.
- **`simulation` (`amms` + `amm-math`):** `amm_wrapper`, all `*_pool` modules,
  `cache_sync`, `configured_amms`, `discovery`, `events`, `data`, the
  `balancer_math`/`profit` re-exports, and `stableswap_math`/`cryptoswap_math`
  (dep-free but simulation-only consumers).
- **`search` (`rayon`, implies `simulation`):** `routing`.

## Cargo.toml

Make `amms`, `amm-math`, `rayon` `optional = true`. Features:

```toml
[features]
default = ["full-protocols", "simulation", "search", "toml"]

adapters   = []                              # always-compiled marker (no heavy deps)
simulation = ["dep:amms", "dep:amm-math"]
search     = ["simulation", "dep:rayon"]
toml       = ["dep:toml"]

# Per-protocol ADAPTER flags (simulation-side gating deferred to S2)
uniswap-v2  = ["adapters"]
uniswap-v3  = ["adapters"]
pancake-v3  = ["uniswap-v3"]   # served by the V3-family adapter
slipstream  = ["uniswap-v3"]   # served by the V3-family adapter
solidly-v2  = ["adapters"]
balancer-v2 = ["adapters"]
balancer-v3 = ["adapters"]
curve       = ["adapters"]
erc4626     = ["adapters"]
uniswap-v4  = ["adapters"]

full-protocols   = ["uniswap-v2","uniswap-v3","pancake-v3","slipstream","solidly-v2","balancer-v2","balancer-v3","curve","erc4626","uniswap-v4"]
common-protocols = ["uniswap-v2","uniswap-v3","balancer-v2","curve"]
```

`required-features` on targets so `--no-default-features --all-targets` skips
heavy targets:
- examples: `triangular_arbitrage` → `["search"]`; `programmatic_loading`,
  `event_subscription` → `["simulation"]`; `toml_loading` → `["toml","simulation"]`.
- bench `simulation` → `["search"]`.
- tests: `adapter_a1`, `adapter_reactive`, `adapter_core` → the protocol-adapter
  features each uses (e.g. `["uniswap-v2","uniswap-v3","balancer-v2"]`; agent
  sets per actual usage).

## Gating points

- **lib.rs:** `#[cfg(feature = "simulation")]` on the simulation modules + the
  `data` module + the inline `balancer_math`/`profit` re-export modules +
  `stableswap_math`/`cryptoswap_math`; `#[cfg(feature = "search")]` on `routing`.
  Keep `cargo doc` on default features (crate-doc intra-doc links reference
  simulation modules).
- **adapters/mod.rs:** gate the per-protocol adapter modules + their re-exports:
  `uniswap_v2` under `uniswap-v2`, `uniswap_v3` (V3-family) under `uniswap-v3`,
  `balancer_v2` under `balancer-v2`. Shared infra (`types`/`traits`/`cache`/
  `registry`/`reactive`/`repair`/`storage`/`driver`) stays always-on.
- **Decouple `repair.rs` from the gated adapter:** `repair.rs` (always-on) calls
  `uniswap_v3::layout_for`. Move `layout_for` (+ any V3 tick decode helpers it
  needs) into `storage.rs` (always-on) so the always-on repair/reactive path
  doesn't reference the `uniswap-v3`-gated module. This is the one real refactor
  the gating requires.

## CI (.github/workflows/ci.yml)

- Keep the default build/clippy/test/doc (all features) — unchanged.
- The existing `--no-default-features` clippy/test steps become adapter-only
  validation once examples/benches/tests carry `required-features`; add
  `--all-targets` to the no-default test step.
- Add an explicit adapter-only step that enables the protocol adapters so the
  adapter tests actually run without heavy deps:
  `cargo test --no-default-features --features uniswap-v2,uniswap-v3,balancer-v2`.
- Add a dep-absence assertion: `cargo tree --no-default-features -e normal`
  shows no `amms`/`amm-math`/`rayon`.
- Optional: a `--features simulation` (no `search`) combo check.

## Acceptance (build-level — Cargo/cfg refactor, no new unit tests)

1. `cargo check --no-default-features --features uniswap-v2,uniswap-v3,balancer-v2 --all-targets`
   compiles with **no** `amms`/`amm-math`/`rayon` (verified via `cargo tree`);
   the adapter tests compile and pass.
2. Default build/test/examples/benches unchanged; `--features simulation`
   (no search) and full both compile.
3. Full CI matrix green.

## Notes / corrections to the earlier proposal

- The checkpoint sketch had `discovery = ["adapters"]`; the mapping shows
  `discovery` uses `LocalAMM` + `cache_sync`, so it is **simulation-tier**. No
  separate `discovery` feature this slice — it's gated under `simulation`.
- Per-protocol **simulation** gating (the `LocalAMM` enum + match arms) is the
  deferred half; it lands with the S2 `LocalAMM` rebuild.
