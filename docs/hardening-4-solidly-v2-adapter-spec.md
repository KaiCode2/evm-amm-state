# Hardening #4 — Solidly V2 adapter (cold-start + reactive + swap-sim)

Manager-owned spec. Implementation agent builds to it; manager authors the core
acceptance tests and reviews. Adds a real adapter for `ProtocolId::SolidlyV2`
(Aerodrome / Velodrome V2 style: reserves AMM with stable + volatile pools).

## Mechanics (vs the existing Uniswap V2 adapter)

Solidly V2 pools are reserves-based and emit `Sync`, like V2 — but:
- Reserves are **two separate `uint256` storage slots** (`reserve0`, `reserve1`),
  not V2's single packed `(uint112,uint112,uint32)` slot. So no masked writes —
  two plain slot writes.
- `Sync(uint256 reserve0, uint256 reserve1)` (Velodrome V2 / Aerodrome — implementer
  CONFIRM the exact signature for the target fork).
- Pools are `stable` (x³y+y³x) or `volatile` (xy=k); the **pool's own**
  `getAmountOut(uint256 amountIn, address tokenIn) returns (uint256)` handles both
  in-EVM. Sim calls it via revm — **no math reimplementation**.

## New types (in `types.rs` / `storage.rs`, re-exported from `mod.rs`)

- `SolidlyStorageLayout { reserve0_slot, reserve1_slot, token0_slot, token1_slot: U256 }`
  (mirrors `V3StorageLayout`'s config approach). Provide a `velodrome_v2()` default
  whose slot indices the implementer determines from the **verified Aerodrome /
  Velodrome V2 `Pool` contract** (and a test-friendly `new(...)`). Config-overridable.
- `SolidlyV2Metadata { token0: Option<Address>, token1: Option<Address>, stable:
  Option<bool>, storage_layout: Option<SolidlyStorageLayout> }`.
- `ProtocolMetadata::SolidlyV2(SolidlyV2Metadata)` variant.
- A `solidly-v2` cargo feature (`= ["adapters"]`), added to `default`.

## Adapter behaviour (`src/adapters/solidly_v2.rs`, new — mirror `uniswap_v2.rs`)

- `protocol()` = `SolidlyV2`. `event_sources`: the pool's `Sync` topic.
- `cold_start_planner`: resolve the layout from metadata (none →
  `UnsupportedReason::MissingMetadata`). Verify `[reserve0_slot, reserve1_slot,
  token0_slot, token1_slot]`. `reserve0`+`reserve1` are **mandatory**, classified
  from their per-slot `SlotFetch` (mirror V2): both `Value` (not both zero) →
  `Ready`; a `FetchFailed`/`NotAttempted` → `NeedsRepair(VerifySlots(reserve
  slots))` / Degraded; genuine all-zero → a DISTINCT degenerate repair (e.g.
  `PurgeSlots`), so archive-miss vs degenerate stay distinguishable (the SlotFetch
  point). `HotSlotsOnly`: reserves only. `Lazy`: reserves now, defer token slots.
  Metadata: merge `token0`/`token1` decoded from the warmed token slots, preserve
  config `stable`/`storage_layout` (V2-style merge is fine; or preserve — choose
  and document).
- `decode_event`: `Sync` → `updates: vec![StateUpdate::slot(reserve0_slot, r0),
  StateUpdate::slot(reserve1_slot, r1)]` (two exact writes from the event payload,
  no fetch), `UpdateQuality::ExactIfApplied`. Malformed/other topic → ignored/error.
- `after_apply`: if a write was skipped (`diff.has_skipped()`) →
  `RepairAction::VerifySlots` of the reserve slots (mirror V2).
- `simulate_swap`: build `getAmountOut(amount_in, token_in)` calldata, run
  `cache.call_raw(ZERO, pool_address, calldata, commit=false)` where `pool_address
  = pool.key.address()`, decode `amount_out`. Revert/halt → `SimError::Reverted`.
  (The pool reads its own reserves + `stable`/decimals, all warm or immutable.)

## Acceptance criteria

**Manager-authored** (must pass UNMODIFIED — these will be compile-red until the
new types/adapter exist, which is the red state):
1. `solidly_cold_start_ready_warms_reserves_and_tokens` (cold_start_adoption.rs):
   Eager cold-start with a test `SolidlyStorageLayout`, seeded reserve0/reserve1/
   token0/token1 → `Ready`; all four slots warmed.
2. `solidly_sync_writes_both_reserve_slots_through_runtime` (adapter_reactive.rs):
   a `Sync(r0, r1)` log through `ReactiveRuntime` writes `reserve0_slot == r0` and
   `reserve1_slot == r1` exactly (offline, no fetch).

**Implementation-agent-authored** (manager reviews): an offline `simulate_swap`
test with a mock Solidly pool fixture (`getAmountOut` returns a deterministic
value; assert no RPC + revert→`Reverted`); a zero-reserves degenerate repair test
(distinct from archive-miss); and a `#[ignore]` RPC-parity test (`sim ==
eth_call getAmountOut` for a real Aerodrome/Velodrome V2 pool at a pinned block —
this is what validates the real `velodrome_v2()` layout + Sync/getAmountOut ABIs).

## Verification

```
CARGO_NET_GIT_FETCH_WITH_CLI=true cargo test --test cold_start_adoption --test adapter_reactive --test adapter_swap_sim --test adapter_a1 --test pipeline_e2e
cargo fmt --all --check
cargo clippy --all-targets --no-deps -- -D warnings
cargo clippy --no-default-features --features adapters,uniswap-v2,uniswap-v3,balancer-v2,solidly-v2 --all-targets --no-deps -- -D warnings
cargo clippy --no-default-features --all-targets --no-deps -- -D warnings
cargo test && cargo test --no-default-features
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps
# manager runs the parity test: E2E_RPC_URL=<archive> cargo test --test adapter_swap_sim_rpc -- --ignored
```

## Constraints / assumptions

- Do NOT modify manager-authored tests. Confirm the Sync / getAmountOut ABIs and
  the `velodrome_v2()` slot layout against the verified contract; the gated parity
  test is the live check. Reference (math only, NOT storage layout): the legacy
  `git show fc6c63e:src/solidly_v2_pool.rs`.
- No new production dependencies (ABIs are local `sol!`). Reuse the V2 patterns;
  no unrelated churn; `cargo fmt` changed files.
- Keep exact event-sourced reserve writes (the V2-like value) — do NOT make the
  reactive path refetch.
