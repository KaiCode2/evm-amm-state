# Releasing `evm-amm-state`

This crate is feature-complete and, as of 2026-07-05, **publishable**: the one
hard blocker (the `evm-fork-cache` dependency) has cleared. What remains is a
short mechanical checklist. Work the sections in order.

## 0. Dependency: the `evm-fork-cache` companion crate — ✅ CLEARED (2026-07-05)

Historically this crate pinned the companion crate to a **git rev on a private
repo**, which `cargo publish` rejects (a dependency must resolve to a crates.io
version). That is resolved:

- `evm-fork-cache` **0.2.1 is published on crates.io**.
- [`Cargo.toml`](Cargo.toml) depends on it by version alone — no `git`, no
  `path`:
  ```toml
  evm-fork-cache = "0.2.1"
  ```
- `Cargo.lock` resolves it from the crates.io registry (the path pin was dropped
  in commit `6fc2345`).

The remaining `alloy-*` and `revm` dependencies are all ordinary crates.io
crates — no action needed.

## 1. Pre-publish verification

Run the full matrix locally (or confirm green CI on the release commit). These
mirror `.github/workflows/ci.yml`:

```bash
cargo fmt --all --check
cargo clippy --all-targets --all-features --no-deps -- -D warnings
cargo clippy --no-default-features --all-targets --no-deps -- -D warnings
# per-protocol isolation (catches cfg-gating bugs):
for f in uniswap-v2 uniswap-v3 pancake-v3 slipstream balancer-v2 solidly-v2 curve; do
  cargo clippy --no-default-features --features "adapters,$f" --all-targets --no-deps -- -D warnings
done
cargo test --all-features
cargo test                           # default features
cargo test --no-default-features
RUSTDOCFLAGS="-D warnings" cargo doc --all-features --no-deps
# heavy-dependency leak guard (must print nothing):
cargo tree -e normal --all-features | grep -E '(amms|amm-math|rayon) v[0-9]' || echo "clean: no heavy deps"
# MSRV guard (matches the CI `msrv` job):
cargo +1.88 check --all-features
```

Last full run 2026-07-08 on the v0.1.0 release-polish branch: **all green.**

**Required before a release** — the env-gated live suites are the only
on-chain ground truth (quote parity per protocol, factory base-slot/CREATE2
constants, one-shot V3 sync parity, and the per-transaction `stateDiff`
write-parity suites for the event-sourced paths). Run the whole ignored set
against an archive node (`E2E_RPC_URL` lives in `.env`, gitignored; never
commit or echo it):

```bash
set -a; . ./.env; set +a
cargo test --all-features -- --ignored --nocapture
```

That covers all eight live files: `adapter_swap_sim_rpc` (mainnet + Base),
`v3_liquidity_rpc` + `balancer_liquidity_rpc` (per-tx write parity),
`v3_full_sync_rpc`, `discovery_cl_rpc`, `discovery_solidly_rpc`, and the
`reactive_ws_e2e` / `reactive_curve_ws_e2e` WS soaks. The same set runs
weekly in CI via `.github/workflows/live.yml` (needs the `E2E_RPC_URL`
repo secret).

## 2. Package hygiene

- Confirm `Cargo.toml` metadata is release-ready: `version`, `license`
  (`MIT OR Apache-2.0`, with `LICENSE-APACHE` + `LICENSE-MIT` present),
  `description`, `repository`, `documentation`, `readme`, `keywords`,
  `categories`, `rust-version` (MSRV `1.88`).
- The `[package].exclude` list drops the test suite, CI config, maintainer docs
  (ROADMAP/RELEASING), and superseded design specs — the seven user-facing
  `docs/` guides, all examples, both benches, and `.cargo/audit.toml` ship. **Keep `exclude` inside the
  `[package]` table** — writing it after a `[package.metadata.*]` header
  silently reparents it under that sub-table and Cargo ships everything (this
  regressed once; fixed in `9036873`).
- Inspect what would ship:
  ```bash
  cargo package --list        # ~56 files: src, examples, both benches, the
                              # seven docs/ guides, README, licenses, changelog
  cargo publish --dry-run
  ```
  Last run 2026-07-08: **56 files, verifies clean.** (`cargo publish` prints
  `ignoring test …` for the excluded `[[test]]` targets — expected and
  harmless.)

## 3. Version & changelog

- Version is `0.1.0` in `Cargo.toml`.
- In [`CHANGELOG.md`](CHANGELOG.md), promote the `[Unreleased]` section to
  `## [0.1.0] - 2026-07-05` and add a fresh empty `[Unreleased]`. Drop the
  preamble's "publishing is blocked on a local path pin" note. Add the link
  references: a `[0.1.0]` target (`.../releases/tag/v0.1.0`) and repoint
  `[Unreleased]` to `.../compare/v0.1.0...HEAD`.
- Commit: `release: v0.1.0`.

## 4. Tag & publish

```bash
git tag -a v0.1.0 -m "evm-amm-state 0.1.0"
git push origin v0.1.0
cargo publish
```

**If commits land on `main` after the tag exists** (this happened during
v0.1.0 prep: six PRs merged after the tag was cut), the tag must be re-pointed
at the final commit *before* `cargo publish` — a stale tag silently publishes
old code's provenance (`.cargo_vcs_info.json`, the GitHub release link, and
the changelog's `[x.y.z]` anchor all disagree with the crate contents):

```bash
git push origin :refs/tags/v0.1.0          # delete the remote tag
git tag -fa v0.1.0 -m "evm-amm-state 0.1.0" <final-commit>
git push origin v0.1.0
```

## 5. Post-publish checks

- Verify the published docs render on docs.rs: feature badges appear on the
  gated modules (`v3_sync`, the per-protocol adapters) and no `sol!`-generated
  ABI types leak into the item list.
- Confirm the README badges resolve (crates.io version, docs.rs).
- Configure the `E2E_RPC_URL` repo secret and run the `Live (env-gated) tests`
  workflow once via `workflow_dispatch` to seed the weekly schedule.
  (Reminder: any `.github/workflows/*` push must go over SSH — the gh OAuth
  token lacks `workflow` scope.)

## Quick status

| Gate | State |
| --- | --- |
| Feature-complete (5 protocols, full pipeline) | ✅ |
| Tests green (unit + offline + RPC parity + WS soak) | ✅ |
| CI matrix green vs published 0.2.1 (fmt / clippy×N / tests×3 / docs / isolation / dep-leak / MSRV 1.88) | ✅ |
| License files present (`LICENSE-APACHE` + `LICENSE-MIT`) | ✅ |
| `evm-fork-cache` resolvable from crates.io | ✅ |
| `cargo publish --dry-run` clean (56 files) | ✅ |
| Release work merged to `main` | ✅ (hardening tiers 0–2, #30/#31/#33, release-review polish) |
| `v0.1.0` tag re-pointed at the final commit | ⏳ pending (currently on `6187d50`, six merges behind) |
| `cargo publish` | ⏳ pending |
