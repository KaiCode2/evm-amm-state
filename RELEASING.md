# Releasing `evm-amm-state`

This crate is feature-complete but **not yet publishable to crates.io**. There
is exactly one hard blocker — the `evm-fork-cache` git dependency — plus a short
mechanical checklist once that clears. Work the sections in order.

## 0. Blocker: the `evm-fork-cache` dependency

[`Cargo.toml`](Cargo.toml) pins the companion crate to a **git rev on a private
repo**:

```toml
evm-fork-cache = { git = "ssh://git@github.com/KaiCode2/evm-fork-cache.git", rev = "903af3d…", version = "0.1" }
```

`cargo publish` **rejects git/path dependencies** — a dependency must resolve to
a crates.io version. So before any release:

1. **Publish `evm-fork-cache` to crates.io** (or confirm it is already
   published) at a version compatible with the `version = "0.1"` requirement
   here. As an interim step for *source* consumers, merging `evm-fork-cache#12`
   to its `main` and re-pinning this `rev` to that merge commit is enough — but
   crates.io publication is required for *this* crate to publish.
2. In [`Cargo.toml`](Cargo.toml), **drop `git` and `rev`**, keeping the
   crates.io requirement:
   ```toml
   evm-fork-cache = "0.1"
   ```
3. `cargo update -p evm-fork-cache` and commit the refreshed `Cargo.lock`.

> `alloy-transport-balancer` is already a normal crates.io dependency — no
> action needed there.

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
  `categories`, `rust-version` (MSRV `1.88` — the CI `msrv` job guards this; it
  builds clean on 1.88 today).
- Inspect what would be shipped. `.env` is gitignored (and untracked), so cargo
  already excludes it — this step is a confirmation, not a likely failure. There
  is no `include`/`exclude` in `Cargo.toml`, so all tracked non-ignored files
  ship (`tests/fixtures` is only tiny mock `.sol`/`.hex` files):
  ```bash
  cargo package --list          # requires the git dep resolved (step 0)
  cargo publish --dry-run
  ```
  `cargo publish --dry-run` failing with a git-dependency error is the signal
  that step 0 is not yet complete.

## 3. Version & changelog

- Decide the version (initial release = `0.1.0`) and set it in `Cargo.toml`.
- In [`CHANGELOG.md`](CHANGELOG.md), promote the `[Unreleased]` section to
  `## [0.1.0] - YYYY-MM-DD` and add a fresh empty `[Unreleased]`. Also add the
  matching link references at the bottom: a `[0.1.0]` target (e.g.
  `.../releases/tag/v0.1.0`) and repoint `[Unreleased]` to a compare URL
  (`.../compare/v0.1.0...HEAD`).
- Commit: `release: v0.1.0`.

## 4. Tag & publish

```bash
git tag -a v0.1.0 -m "evm-amm-state 0.1.0"
git push origin v0.1.0
cargo publish
```

## 5. Post-publish cleanup

Once `evm-fork-cache` is public on crates.io and consumed by version:

- Remove all three `Authenticate git for private companion crates` steps (the
  `check`, `isolation`, and `msrv` jobs) and the `CARGO_NET_GIT_FETCH_WITH_CLI`
  env from [`.github/workflows/ci.yml`](.github/workflows/ci.yml), and delete the
  `PRIVATE_REPO_TOKEN` repo secret — CI no longer needs private git access.
- Verify the published docs render on docs.rs.

## Quick status

| Gate | State |
| --- | --- |
| Feature-complete (5 protocols, full pipeline) | ✅ |
| Tests green (unit + offline + RPC parity + WS soak) | ✅ |
| CI green (fmt / clippy×N / tests×3 / docs / per-protocol isolation matrix) | ✅ |
| MSRV 1.88 builds clean (CI-guarded) | ✅ |
| License files present | ✅ |
| `evm-fork-cache` resolvable from crates.io | ❌ **blocker** |
| `cargo publish --dry-run` clean | ❌ (gated on the above) |
