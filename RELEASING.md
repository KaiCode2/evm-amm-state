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

Last run 2026-07-05 against the published `evm-fork-cache` 0.2.1: **all green.**

Optional but recommended — the env-gated network tests against an archive node
(`E2E_RPC_URL` lives in `.env`, gitignored; never commit or echo it):

```bash
set -a; . ./.env; set +a
cargo test --test adapter_swap_sim_rpc -- --ignored          # RPC parity (mainnet + Base)
cargo test --test reactive_ws_e2e --test reactive_curve_ws_e2e -- --ignored --nocapture  # live WS soak
```

## 2. Package hygiene

- Confirm `Cargo.toml` metadata is release-ready: `version`, `license`
  (`MIT OR Apache-2.0`, with `LICENSE-APACHE` + `LICENSE-MIT` present),
  `description`, `repository`, `documentation`, `readme`, `keywords`,
  `categories`, `rust-version` (MSRV `1.88`).
- The `[package].exclude` list drops the test suite, CI config, maintainer docs
  (ROADMAP/RELEASING), and superseded design specs. **Keep `exclude` inside the
  `[package]` table** — writing it after a `[package.metadata.*]` header
  silently reparents it under that sub-table and Cargo ships everything (this
  regressed once; fixed in `9036873`).
- Inspect what would ship:
  ```bash
  cargo package --list        # ~50 files: src, examples, benches, the five
                              # docs/ guides, README, licenses, changelog
  cargo publish --dry-run
  ```
  Last run 2026-07-05: **50 files, 768.6 KiB (207.6 KiB compressed), verifies
  clean.** (`cargo publish` prints `ignoring test …` for the excluded `[[test]]`
  targets — expected and harmless.)

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

## 5. Post-publish cleanup

- Remove the three `Authenticate git for private companion crates` steps (in the
  `check`, `isolation`, and `msrv` jobs) and the `CARGO_NET_GIT_FETCH_WITH_CLI`
  env from [`.github/workflows/ci.yml`](.github/workflows/ci.yml), and delete the
  `PRIVATE_REPO_TOKEN` repo secret — CI no longer needs private git access now
  that the dependency is on crates.io. (The `ci.yml` push must go over SSH: the
  gh OAuth token lacks `workflow` scope.)
- Verify the published docs render on docs.rs.

## Quick status

| Gate | State |
| --- | --- |
| Feature-complete (5 protocols, full pipeline) | ✅ |
| Tests green (unit + offline + RPC parity + WS soak) | ✅ |
| CI matrix green vs published 0.2.1 (fmt / clippy×N / tests×3 / docs / isolation / dep-leak / MSRV 1.88) | ✅ |
| License files present (`LICENSE-APACHE` + `LICENSE-MIT`) | ✅ |
| `evm-fork-cache` resolvable from crates.io | ✅ |
| `cargo publish --dry-run` clean (50 files) | ✅ |
| Release work merged to `main` | ⏳ pending |
| Tagged `v0.1.0` + `cargo publish` | ⏳ pending |
