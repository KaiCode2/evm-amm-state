# Hardening #2 — DeferredWork driver

Manager-owned spec. Implementation agent builds to it; manager authors the
acceptance tests and reviews.

## Outcome

`AdapterRegistry::cold_start` returns `ColdStartOutcome::ReadyWithDeferred(report,
Vec<DeferredWork>)` for the `Lazy` policy (V2 defers its token slots; V3 defers
the bitmap-word window), but **nothing executes the deferred work** — so a Lazy
cold-start can never be completed. Add a driver that executes deferred work
against the cache, so a consumer can warm the deferred slots when ready.

## Scope / design

Add `AdapterRegistry::run_deferred(&self, deferred: &[DeferredWork], cache: &mut
dyn AdapterCache) -> Result<DeferredOutcome>`:
- `DeferredWork::VerifySlots(slots)` → `cache.verify_slots(slots)`; accumulate the
  returned `SlotChange`s.
- `DeferredWork::Repair(RepairAction::VerifySlots(slots))` → same (warm the slots).
- `DeferredWork::ColdStart { .. }`, `DeferredWork::Custom(..)`, and any other
  `Repair(..)` variant → **not executed in this item**; return them verbatim in
  the outcome's `unhandled` list (honest — these belong to item #3 / future). Do
  NOT silently drop them.

`DeferredOutcome` (new, in `types.rs`): `{ verified: Vec<SlotChange>, unhandled:
Vec<DeferredWork> }` (or a close equivalent — derive `Debug`; a small accessor or
`is_fully_handled()` helper is welcome). Errors from `verify_slots` propagate via
`Result`.

The only `DeferredWork` variant actually produced today is `VerifySlots` (V2/V3
`Lazy`), so this driver completes every current Lazy cold-start; the `unhandled`
list future-proofs the other variants.

## Non-goals

- Executing `ColdStart`/`Repair`(non-VerifySlots)/`Custom` deferred work — that
  is item #3 / later (it needs repair execution + re-cold-start-by-key).
- Auto-running deferred work inside `cold_start` (that would defeat `Lazy`'s
  purpose). The driver is an explicit, consumer-invoked step.
- No new production dependencies.

## Affected files

`src/adapters/registry.rs` (the method), `src/adapters/types.rs`
(`DeferredOutcome`), `src/adapters/mod.rs` (re-export `DeferredOutcome` if the
others are re-exported). No change to `cold_start` behavior.

## Acceptance criteria (manager test in `tests/cold_start_adoption.rs`)

Must pass UNMODIFIED:
- `v2_run_deferred_warms_lazy_deferred_slots`: Lazy cold-start a V2 pool →
  `ReadyWithDeferred(_, deferred)` (token slots NOT warmed yet) → `registry
  .run_deferred(&deferred, &mut cache)?` → the deferred token slots are now warmed
  (`cached_storage_value(..).is_some()`), reserves still warm. (Currently does not
  compile — `run_deferred` does not exist — which is the red state; green once
  added.)
- Existing `v2_cold_start_lazy_defers_exactly_what_eager_warms` and the V3 tests
  still pass.

## Verification

```
CARGO_NET_GIT_FETCH_WITH_CLI=true cargo test --test cold_start_adoption --test adapter_reactive --test pipeline_e2e
cargo fmt --all --check
cargo clippy --all-targets --no-deps -- -D warnings
cargo clippy --no-default-features --all-targets --no-deps -- -D warnings
cargo test && cargo test --no-default-features
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps
```

## Assumptions

- `run_deferred` lives on `AdapterRegistry` (sibling of `cold_start`); it takes
  `&self` (no registry mutation needed for `VerifySlots`). If the implementer
  finds a cleaner home/signature, justify to the manager. The `unhandled` return
  for non-`VerifySlots` variants is required (do not panic / drop).
