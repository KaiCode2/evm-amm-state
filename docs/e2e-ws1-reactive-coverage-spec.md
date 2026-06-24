# E2E WS1 — Reactive event → cache coverage (new pipeline)

Manager-owned spec. Top-priority workstream: strong guarantees the new pipeline's
event→storage path works. (The zero-coverage gap the readiness audit found was in
the LEGACY `src/events/`, which WS3 deletes — so the effort targets the **adapters
reactive path** we are keeping.)

## Objective

Comprehensive, behavior-level coverage of the adapters reactive path: decoded
events actually mutate the expected cache slots, the cold-start→reactive lifecycle
holds on a shared registry+cache, and edge/negative cases are handled.

## Scope (WS1a — parallel-safe, independent of WS2)

Covers **Uniswap V2 + Uniswap V3 only** (Balancer reactive is built in WS2; its
coverage lands in WS1b). All offline.

1. **Per-event cache-mutation assertions**: for each handled event, assert the
   exact resulting cache state (slot value/mask), not just "an event was emitted":
   - V2 `Sync` → packed reserves at `V2_RESERVES_SLOT` (`slot_masked`, mask 224).
   - V3 `Swap` → `slot0` (sqrtPriceX96/tick) + `liquidity` updates.
   - V3 `Mint`/`Burn` → liquidity (and any tick/observation slots the adapter
     decodes today). Assert what the adapter currently emits; if an expected
     mutation is missing/wrong, that is a real bug — report it to the manager.
2. **Chained lifecycle** (the gap with no test today): one test per protocol that
   runs `AdapterRegistry::cold_start` THEN ingests reactive events on the **same
   registry + EvmCache**, asserting the post-event cached slot reflects the event.
3. **Edge / negative**: malformed log (wrong topic0 / bad data) → `ignored`/`error`
   without mutation or panic; a multi-event batch applies all; `after_apply`
   repair fires when a masked write is skipped (V2 `Sync` + `diff.has_skipped()`
   → `RepairAction::VerifySlots`, mirroring `uniswap_v2.rs`); an event for an
   untracked pool is routed to nothing.

## Non-goals

- Balancer reactive coverage and any `simulate_swap` assertions → WS1b (after WS2).
- Touching legacy `src/events/` (deleted in WS3) — add NO tests there.
- Changing reactive production code, except a minimal fix for a bug a new test
  reveals — and only after flagging it to the manager.

## Test plan

Extend `tests/adapter_reactive.rs` (reuse its `ReactiveRuntime`/`ingest_batch`
harness + fixtures + helpers; follow existing naming). Add a `tests/pipeline_e2e.rs`
only if the chained lifecycle test does not fit the existing harness. All tests
offline (mocked fetcher / seeded cache), asserting concrete post-state.

## Acceptance criteria

- Every reactive event kind the V2 and V3 adapters handle has a test asserting the
  exact cache mutation (or documented no-op).
- A chained cold-start→reactive test exists for V2 and for V3 on a shared
  registry+cache.
- Negative/edge cases above are covered; no test is skipped or has loosened
  assertions; suite is green (any red ⇒ a real bug, fixed within the minimal-fix
  rule or escalated).

## Verification

```
CARGO_NET_GIT_FETCH_WITH_CLI=true cargo test --test adapter_reactive --test cold_start_adoption
cargo fmt --all --check
cargo clippy --all-targets --no-deps -- -D warnings
cargo test && cargo test --no-default-features
```

## Risks / assumptions

- Assumption: the V3 adapter's `decode_event` already emits slot0/liquidity
  updates; WS1a characterizes and pins them. If V3 reactive is thinner than
  assumed, scope is "cover what exists + report gaps," not "build V3 reactive."
- Coordinate with WS2: do not edit `balancer_v2.rs` or add Balancer reactive tests
  here (WS2 owns that), to avoid conflicts.
