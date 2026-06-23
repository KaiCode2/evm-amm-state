# Phase A4 — Slice 1: cold-start adoption (V2/V3 planners, verify-only)

Status: implementation handoff. Manager (parent session) owns the spec + the
acceptance tests in `tests/cold_start_adoption.rs`. Implementation agent owns the
production code + porting the existing trait tests.

## Goal

Adopt the new `evm_fork_cache::cold_start` mechanism. **Replace** the imperative
`AmmAdapter::cold_start` with a per-adapter `ColdStartPlanner` driven by
`EvmCache::run_cold_start`, and replace the `cached_storage(..).is_none()` proxy
with the per-slot `SlotFetch` classification. V2 and V3 only this slice (Balancer
= slice 2; full reactive `RepairAction::ColdStart` wiring = slice 3).

The pinned `evm-fork-cache` rev is already bumped to the `cold-start-sync` tip
(`903af3d`); `evm_fork_cache::cold_start` is available.

## Upstream API (already shipped; do not modify upstream)

```rust
// evm_fork_cache::cold_start
trait ColdStartPlanner {
    fn initial_plan(&mut self, state: &dyn StateView) -> ColdStartPlan;
    fn on_results(&mut self, results: &ColdStartResults, state: &dyn StateView) -> ColdStartStep;
}
impl EvmCache {
    fn run_cold_start(&mut self, planner: &mut dyn ColdStartPlanner, config: ColdStartConfig)
        -> Result<ColdStartRunReport, ColdStartError>;
}
ColdStartPlan { verify, probe, accounts, discover }      // Default; we use `verify` only this slice
ColdStartResults { verified: Vec<SlotChange>, fetched: Vec<SlotOutcome>, probed, discovered }
SlotOutcome { address, slot, fetch: SlotFetch }
enum SlotFetch { Value(U256), Zero, FetchFailed { reason: String }, NotAttempted }
enum ColdStartStep { Done, Continue(ColdStartPlan) }
ColdStartConfig::default() == { max_rounds: 8, pin: CachePinned }
```

Two upstream contracts to respect:
- **`fetched`/`probed` order is unspecified** — look up an outcome by `(address, slot)`
  via `.iter().find(...)`, never by index.
- `run_cold_start` returns the report **only on `Ok`**; on `Err(ColdStartError)` the
  report is gone. For V2/V3 verify-only with a configured fetcher a hard error is
  not expected (a per-slot fetch failure is classified as `SlotFetch::FetchFailed`,
  **not** a run error), so propagate any `ColdStartError` as the entry's `Err`.

## New consumer API (this is the contract the acceptance tests pin)

### Trait change — `src/adapters/traits.rs`

Remove `AmmAdapter::cold_start`. Add a planner factory:

```rust
fn cold_start_planner(
    &self,
    pool: &PoolRegistration,
    policy: ColdStartPolicy,
) -> Result<Box<dyn AdapterColdStartPlanner>, UnsupportedReason> {
    Err(UnsupportedReason::Protocol(self.protocol()))
}
```

### New always-on module — `src/adapters/cold_start.rs`

`evm_fork_cache::cold_start` is available unconditionally (we already depend on the
default-on `reactive` feature via `adapters/reactive.rs`), so this module is
always compiled (not behind a protocol flag).

```rust
/// An adapter cold-start planner: an upstream `ColdStartPlanner` that also
/// finalizes into a `ColdStartOutcome` and applies decoded metadata/status to
/// the pool. `'static` (planners own their address/layout/policy/state).
pub trait AdapterColdStartPlanner {
    fn initial_plan(&mut self, state: &dyn StateView) -> ColdStartPlan;
    fn on_results(&mut self, results: &ColdStartResults, state: &dyn StateView) -> ColdStartStep;
    /// Finalize after the run: mutate `pool` (metadata + status) and return the outcome.
    fn finish(&mut self, pool: &mut PoolRegistration, report: &ColdStartRunReport) -> ColdStartOutcome;
}

impl AdapterRegistry {
    /// Cold-start `pool` through its adapter's planner, driving
    /// `EvmCache::run_cold_start`. Maps the planner + report back to the existing
    /// `ColdStartOutcome`. Missing adapter / unsupported map to `Ok(Unsupported(..))`;
    /// an upstream `ColdStartError` propagates as `Err`.
    pub fn cold_start(
        &self,
        pool: &mut PoolRegistration,
        cache: &mut EvmCache,
        policy: ColdStartPolicy,
    ) -> Result<ColdStartOutcome, ColdStartError>;
}
```

Driver body (private `Bridge` newtype forwards the adapter planner to the upstream
trait — avoids trait-upcasting):

```rust
let Some(adapter) = self.adapter(pool.protocol()) else {
    return Ok(ColdStartOutcome::Unsupported(UnsupportedReason::Protocol(pool.protocol())));
};
let mut planner = match adapter.cold_start_planner(pool, policy) {
    Ok(p) => p,
    Err(reason) => return Ok(ColdStartOutcome::Unsupported(reason)),
};
struct Bridge<'a>(&'a mut dyn AdapterColdStartPlanner);
impl evm_fork_cache::cold_start::ColdStartPlanner for Bridge<'_> { /* forward both */ }
let report = { let mut b = Bridge(planner.as_mut()); cache.run_cold_start(&mut b, ColdStartConfig::default())? };
Ok(planner.finish(pool, &report))
```

### Per-protocol planners (in the gated adapter modules)

**`UniswapV2ColdStartPlanner`** (`src/adapters/uniswap_v2.rs`) — single round, verify-only.
- `initial_plan`: `verify = [reserves(8)]` always; for `Strict`/`Eager` also push
  `token0(6)`, `token1(7)`. For `Lazy`: `verify = [8]` now, record `token0/token1`
  as deferred. `HotSlotsOnly`: `verify = [8]` only.
- `on_results`: look up the reserves `SlotOutcome` by `(addr, 8)`:
  - `Value(_)` → ready; `Zero` → degenerate-pool repair; `FetchFailed{..}` →
    historical-unavailable repair (these last two must be **distinguishable** —
    different `RepairAction` and/or recorded reason). Decode `token0`/`token1`
    from `state.storage(addr, 6/7)` (warmed) when present (merge; preserve config
    `fee_bps`). Return `Done`.
- `finish`: set `pool.metadata = UniswapV2(...)` (merge tokens, preserve `fee_bps`),
  set `pool.status`, build `ColdStartReport` from the run report + accumulated
  state, return `Ready` / `NeedsRepair(report, repair)` / `ReadyWithDeferred(report, deferred)`.

This must reproduce the **current** `uniswap_v2.rs::cold_start` end-state (slots
6/7/8, metadata merge, the Lazy/HotSlotsOnly behavior), only sourcing the repair
decision from `SlotFetch` instead of `cached_storage(..).is_none()`.

**`UniswapV3ColdStartPlanner`** (`src/adapters/uniswap_v3.rs`) — multi-round, preserves the A3 current-tick-word warm-up.
- Resolve layout via `layout_for(pool)` in the factory; **no layout → factory
  returns `Err(UnsupportedReason::MissingMetadata("V3 storage layout"))`**; non-address
  key → `Err(UnsupportedReason::Custom(..))` (preserve current messages).
- Round 1: `verify = [slot0, liquidity]`. `on_results`: if slot0 `FetchFailed`/`Zero`
  (cold) → `NeedsRepair`, `Done`. Else decode the current tick from the warmed
  `slot0` (`state.storage`), and **per policy**:
  - `Strict`/`Eager` → `Continue` round 2 = the current-tick `tickBitmap` word;
    then round 3 (`Continue`) = the `{0,3}` info slots of the ticks initialized in
    that word (extracted from the warmed word). Then `Done`.
  - `HotSlotsOnly` → `Done` (slot0+liquidity only).
  - `Lazy` → `Done`, defer the bitmap word as `DeferredWork::VerifySlots`.
- This is the existing `uniswap_v3.rs::cold_start` logic (lines 67–179) re-expressed
  as planner rounds; the **multi-word adaptive scan stays deferred** (future
  `Continue` rounds, not this slice). Preserve config V3 metadata (do not overwrite).

### `ColdStartPolicy` mapping + Lazy == Eager invariant

Map policy to per-round slot sets as above. Preserve the invariant: the set Lazy
warms now **plus** what it defers equals the set Eager warms. Do not fold Lazy into
eager up-front warming.

### Replace the `is_none()` proxy

Delete the `cached_storage(addr, slot).is_none()` repair decisions at
`uniswap_v2.rs:73` and `uniswap_v3.rs:107`; the planner's `on_results` now decides
from `SlotFetch` (`Value`/`Zero`/`FetchFailed`).

## Acceptance criteria

- `AdapterRegistry::cold_start(&mut pool, &mut EvmCache, policy)` exists and the
  manager tests in `tests/cold_start_adoption.rs` pass **unmodified**.
- V2: `Value` reserves → `Ready` (tokens merged, `fee_bps` preserved); `Zero` →
  `NeedsRepair`; `FetchFailed` → `NeedsRepair` with a **distinct** repair from the
  `Zero` case (the archive-miss improvement).
- V3: `Ready` decodes the current tick from the warmed slot0; the
  `then_swap_applies_exact_no_resync` synergy holds via the new entry; missing
  layout → `Unsupported(MissingMetadata)`; cold slot0 → `NeedsRepair`.
- Lazy defers exactly the slots Eager warms eagerly (V2 tokens; V3 tick word).
- `AmmAdapter::cold_start` is removed; the ~13 existing trait-level cold-start tests
  in `tests/adapter_reactive.rs` are **ported** to the new entry, preserving their
  assertions' intent (outcome variant, warmed/deferred slots, metadata). Flag any
  that cannot map rather than weakening them.
- Full CI matrix green: `cargo fmt --all --check`; clippy default + adapters-only
  (`--no-default-features --features adapters,uniswap-v2,uniswap-v3,balancer-v2`) +
  no-default, all `--all-targets -D warnings`; `cargo test` default + adapters-only +
  no-default; `cargo doc --no-deps` with `RUSTDOCFLAGS="-D warnings"`. The adapter
  cold-start code must compile in the adapters-only build (no `amms`/`amm-math`/`rayon`).

## Constraints / prohibitions

- Do not modify `tests/cold_start_adoption.rs` (manager-authored) — make it pass.
- Do not weaken/skip/rewrite the ported `adapter_reactive.rs` assertions; preserve intent.
- Do not introduce new production dependencies.
- Keep `AdapterCache` (the facade) — the new path simply doesn't use it for cold start.
- No unrelated formatting churn. Run `cargo fmt` on files you change (not on
  manager test files).
- The cold-start entry runs on a multi-thread tokio runtime (verify-only V2/V3 do
  not strictly need it, but keep tests on `#[tokio::test]` consistent with the file).

## Verification commands

```
CARGO_NET_GIT_FETCH_WITH_CLI=true cargo test --test cold_start_adoption --test adapter_reactive --test adapter_a1 --test adapter_core
cargo fmt --all --check
cargo clippy --all-targets --no-deps -- -D warnings
cargo clippy --no-default-features --features adapters,uniswap-v2,uniswap-v3,balancer-v2 --all-targets --no-deps -- -D warnings
cargo clippy --no-default-features --all-targets --no-deps -- -D warnings
cargo test
cargo test --no-default-features --features adapters,uniswap-v2,uniswap-v3,balancer-v2
cargo test --no-default-features
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps
```
