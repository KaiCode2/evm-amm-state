# Phase A4 — Slice 2: Balancer V2 cold-start (discover → verify)

Status: implementation handoff. Manager owns the spec + the Balancer acceptance
tests in `tests/cold_start_adoption.rs`. Implementation agent owns the production
`BalancerV2ColdStartPlanner` + wiring.

Builds on slice 1 (`AdapterColdStartPlanner` trait, `Bridge`,
`AdapterRegistry::cold_start`, `cold_start_planner` trait method). Stacks on
PR #9.

## Goal

Give Balancer V2 a real `cold_start_planner` — **net-new behavior** (the adapter
currently uses the `Unsupported` default). Balancer pool state is not at
predictable slots, so use **access-list discovery**: call `getPoolTokens(poolId)`
on the vault via the driver's `discover` phase, capture the touched `(vault, slot)`
pairs (`restrict_to = [vault]`), verify them in a second round, and decode the
token list from the call's return data.

## Behavior — `BalancerV2ColdStartPlanner`

**Factory** (`BalancerV2Adapter::cold_start_planner`):
- Resolve the vault: `BalancerV2Metadata.vault`, falling back to
  `pool.state_addresses.first()`. No vault → `Err(UnsupportedReason::MissingMetadata("Balancer vault"))`.
- Resolve the poolId: `pool.key.bytes32()` (the `BalancerV2(B256)` key). None →
  `Err(UnsupportedReason::Custom("Balancer V2 pool key is not bytes32-keyed"))`.
- Return `Ok(Box::new(BalancerV2ColdStartPlanner { vault, pool_id, policy, .. }))`.

**Round 1 — discover** (`initial_plan`):
```rust
ColdStartPlan {
    accounts: vec![self.vault],            // ensure the vault's code before the call
    discover: vec![ColdStartCall {
        from: Address::ZERO,
        to: self.vault,
        calldata: IBalancerVault::getPoolTokensCall { poolId: self.pool_id }.abi_encode().into(),
        restrict_to: Some(vec![self.vault]),
    }],
    ..Default::default()
}
```

**`on_results`:**
- Phase `Discover`:
  - `let call = &results.discovered[0];`
  - Decode the return data from `call.result` (revm `ExecutionResult::output()` →
    `IBalancerVault::getPoolTokensCall::abi_decode_returns(..)`), storing
    `tokens`/`balances` on the planner. A reverted/undecodable call → record a
    repair and `Done` (treat as unsupported/degraded, not a panic).
  - Collect the discovered slots: `call.access.slots` filtered to the vault
    (already restricted). **Empty capture is a distinguishable signal** — if no
    slots were touched, record a `discover-yielded-no-slots` repair and `Done`
    (do not `Continue` into an empty no-op verify round).
  - Otherwise → `phase = Verify`, `Continue(ColdStartPlan { verify: discovered_slots, ..Default })`.
- Phase `Verify`: the vault balance slots are now warm → `Done`.

**`finish`:**
- Set `pool.metadata = BalancerV2(BalancerV2Metadata { vault: Some(vault),
  pool_address: Some(addr_from_pool_id), tokens: decoded_tokens })`
  (pool address = first 20 bytes of the poolId, matching Balancer's poolId
  encoding). Set `pool.status = Ready`. Build the `ColdStartReport`.
- Discover-failed / empty-capture / undecodable-return → `NeedsRepair(report, ..)`
  with `status = Degraded`.

**`IBalancerVault` ABI:** add a local `sol! { ... getPoolTokens(bytes32 poolId)
returns (address[] tokens, uint256[] balances, uint256 lastChangeBlock); }` in
`balancer_v2.rs` (do NOT depend on the simulation-gated `cache_sync` copy).

**Policy:** for this slice Balancer runs the full discover→verify flow for all
policies (balances are the hot state). `HotSlotsOnly`/`Lazy` nuances can be
refined later; do not over-engineer — but keep the planner policy-aware in shape.

## Runtime note

The discover phase runs `ensure_account` (async, sync-bridged) + an EVM call, so
`run_cold_start` for Balancer requires a **multi-thread tokio runtime** (the
manager tests use `#[tokio::test(flavor = "multi_thread")]`).

## Acceptance criteria (manager tests in `tests/cold_start_adoption.rs`)

The manager adds Balancer tests + an `install_mock_vault` helper (installs the
`tests/fixtures/mock_balancer_vault_runtime.hex` stub at the vault, seeds slots
0–4) and a `setup_cache_with_asserter` helper. These must pass UNMODIFIED:
- **Happy path** (`balancer_cold_start_discover_verify_ready`): install
  `Address::ZERO` (gas beneficiary) + the vault stub; seed slots 0–4 (token0,
  token1, balance0, balance1, lastChangeBlock); a `stub_fetcher` returns those
  slots for the verify round. `registry.cold_start(Eager)` → `Ready`;
  `metadata.tokens == [token0, token1]`; the discovered vault balance slots are
  warm (`cached_storage_value(..).is_some()`); **no RPC** (`asserter.read_q().is_empty()`).
- **Missing vault** (`balancer_cold_start_missing_vault_unsupported`): a Balancer
  registration with no vault and no `state_addresses` → `Unsupported`.

## Constraints / prohibitions

- Do not modify the manager-authored Balancer tests; make them pass.
- Do not weaken the slice-1 tests or the V2/V3 planners.
- The discover call runs on `EvmCache` inside the driver — no `AdapterCache` change.
- No new production dependencies (revm/alloy already present; `getPoolTokens` ABI
  is a local `sol!`).
- Look up discovered slots from `call.access.slots` (a set); the `fetched`/`probed`
  ordering caveat does not apply to discover, but treat the access list as a set.
- Run `cargo fmt` on changed files (not the manager test file).

## Verification (run all; report output)

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
