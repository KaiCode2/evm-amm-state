# Curve StableSwap adapter — implementation spec (slice 1)

Manager-authored spec for the `curve` feature: a new `CurveAdapter` (Curve
StableSwap *plain* pools, e.g. 3pool) plugged into the adapters pipeline
(track → react → simulate). Mirrors the established `AmmAdapter` contract; the
closest existing analog is `BalancerV2Adapter` (discover→verify cold-start +
resync-on-event reactive).

## Scope

In scope (slice 1):
- Classic Curve **StableSwap plain pools** with a self-contained
  `get_dy(int128 i, int128 j, uint256 dx)` quote (no external rate-oracle /
  lending calls): 3pool (DAI/USDC/USDT), and shape-identical plain pools.
- `simulate_swap` via the pool's own `get_dy` (chain code does the StableSwap
  invariant — **no reimplemented math**, consistent with every other adapter).
- Discover→verify cold-start that warms `get_dy`'s SLOAD set.
- Reactive resync on `TokenExchange` + liquidity events.

Non-goals (deferred, documented as limitations):
- CryptoSwap (Curve v2) and StableSwap-NG pools, which use
  `get_dy(uint256,uint256,uint256)` (uint256 indices) — a future metadata flag
  selecting the index ABI.
- Metapools and lending/underlying pools (`get_dy_underlying`,
  `TokenExchangeUnderlying`) whose `get_dy` makes external calls — the
  `restrict_to=[pool]` discover capture would be incomplete for them.
- Discovering `coins` on-chain (slice 1 takes them as config).
- Arity-independent liquidity-event routing. The `AddLiquidity`/`RemoveLiquidity`/
  `RemoveLiquidityImbalance` topic hashes depend on `n_coins` (the `uint256[N]`
  arity is part of the event signature); slice 1 fixes N=3, so liquidity events
  route for 3-coin pools (3pool, validated) but not 2-/4-coin pools.
  `TokenExchange` (the swap event, no arrays) routes universally, so swap-driven
  resync is unaffected. Slice 2: derive the topic hashes from `n_coins`.

## Behavioral requirements

### Metadata — `CurveMetadata`
```rust
pub struct CurveMetadata {
    /// Pool coins in index order; coins[i] is the get_dy index i. Config-
    /// supplied (static pool identity). Required for simulate_swap's
    /// token -> index mapping.
    pub coins: Vec<Address>,
    /// Storage slots warmed by the cold-start discover pass (the get_dy
    /// read-set: balances + amplification + fee, wherever the Vyper build
    /// placed them). Persisted so the reactive path re-verifies exactly them.
    /// Slot-only; all live on the pool address. Empty until cold-start runs.
    pub discovered_slots: Vec<U256>,
}
```
Add `ProtocolMetadata::Curve(CurveMetadata)` + its manual `Debug` arm. Derive
`Clone, Debug, Default, PartialEq, Eq`.

### `protocol()` → `ProtocolId::Curve`.

### `event_sources(pool)`
Direct source on the pool address for the classic StableSwap topics:
`TokenExchange`, `AddLiquidity`, `RemoveLiquidity`, `RemoveLiquidityOne`,
`RemoveLiquidityImbalance`. (No indexed routing — the pool *is* the emitter.)
Empty if the pool key is not address-keyed.

Event ABIs (classic StableSwap):
```solidity
event TokenExchange(address indexed buyer, int128 sold_id, uint256 tokens_sold, int128 bought_id, uint256 tokens_bought);
event AddLiquidity(address indexed provider, uint256[<N>] token_amounts, uint256[<N>] fees, uint256 invariant, uint256 token_supply);
event RemoveLiquidity(address indexed provider, uint256[<N>] token_amounts, uint256[<N>] fees, uint256 token_supply);
event RemoveLiquidityOne(address indexed provider, uint256 token_amount, uint256 coin_amount);
event RemoveLiquidityImbalance(address indexed provider, uint256[<N>] token_amounts, uint256[<N>] fees, uint256 invariant, uint256 token_supply);
```
N is pool-specific in the real ABI; for **topic routing only the signature hash
matters**, and we do not decode the liquidity-event payloads (we resync). So
declare the topic hashes from the canonical signatures. For `TokenExchange` we
*do* validate the log decodes (guard against malformed logs), matching Balancer.

### `cold_start_planner(pool, policy)` — discover → verify
Mirror `BalancerV2ColdStartPlanner` exactly in shape:
- **Round 1 (Discover):** `accounts=[pool]`; one discover `ColdStartCall`:
  `get_dy(0, 1, DISCOVER_DX)` from `Address::ZERO` to the pool,
  `restrict_to=Some([pool])`. `DISCOVER_DX` is a fixed nonzero value (e.g.
  `1_000_000`); its magnitude is irrelevant — `get_dy` SLOADs the full balance
  set + A + fee unconditionally, so any non-reverting `dx` captures the read-set.
- **on_results (Discover):** classify off `call.result.is_success()` first
  (DiscoverFailed on revert/halt/no-output). Collect captured pool slots
  (`access.slots` filtered to the pool). Empty capture → NoSlotsDiscovered.
  Continue to a verify round over the captured slots.
- **Round 2 (Verify):** verify the captured slots. Per-slot `SlotFetch`
  classification: any `FetchFailed`/`NotAttempted`/missing → BalancesUnfetched.
  A genuine `Zero` is acceptable.
- **finish:**
  - Ready → persist `CurveMetadata { coins: <config coins preserved>,
    discovered_slots: <sorted, deduped captured slots> }`, `status = Ready`.
  - DiscoverFailed / NoSlotsDiscovered → `Degraded` + `RepairAction::ColdStart`
    (re-run discovery; Curve pools are standalone contracts, but ColdStart re-run
    is the consistent, safe repair).
  - BalancesUnfetched → `Degraded` + `RepairAction::VerifySlots(captured)`.
- Pool key not address-keyed → `UnsupportedReason::Custom`. (No layout to
  validate — discovery handles it, so there is no MissingMetadata path here.
  `coins` may be empty at cold-start; it is only required at simulate time.)

Policy handling: like Balancer, the run executes for every policy (the pool
state is the hot set). Thread `policy` into the report.

### `decode_event(pool, log, _view)` — resync
- Topic0 not one of the configured topics → `ignored()`.
- `TokenExchange`: validate it decodes (`error(MalformedLog)` if not). Liquidity
  events: route on topic only (no payload decode).
- Build the repair from `CurveMetadata.discovered_slots`:
  - non-empty → `RepairAction::VerifySlots((pool, slot) for slot in slots)`.
  - empty (cold-start has not run / found them) → `RepairAction::None`
    (conservative fallback, mirrors Balancer).
- Emit `AdapterEvent { updates: vec![], quality: ConservativeInvalidation,
  repair, kind: Swap for TokenExchange / Liquidity for the rest, topic0,
  emitter, pool }`. (Use the existing `AdapterEventKind` variants; pick `Swap`
  for `TokenExchange` and the nearest liquidity-ish kind for the others —
  inspect `AdapterEventKind` and match house usage.)
- Missing/`Unknown` metadata → treat like empty slots (`RepairAction::None`),
  do **not** return an error that would fail the whole `ingest_batch`
  (this is the batch-robustness lesson from the Solidly audit).

### `simulate_swap(pool, cache, token_in, token_out, amount_in, _config)`
- Resolve pool address from `pool.key.address()` (else MissingMetadata).
- Resolve `coins` from `CurveMetadata.coins` (else
  `MissingMetadata("Curve coins")`).
- Map `token_in -> i`, `token_out -> j` by index in `coins`. Either not found →
  `MissingMetadata("Curve token not in pool")` (or a `Custom`/`MalformedOutput`
  — pick the closest existing `SimError`; document the choice).
- Call `get_dy(i as int128, j as int128, amount_in)` via `run_quote(cache,
  pool, calldata)`; decode the `uint256` result → `SwapQuote::new(dy)`.
- The `get_dy` `sol!` ABI lives in `sim.rs` (shared quote-ABI module), gated to
  include `feature = "curve"` in the existing
  `any(uniswap-v2, uniswap-v3, balancer-v2, solidly-v2, curve)` cfg blocks
  (Bytes/ExecutionResult/AdapterCache imports + `run_quote`). **This is the
  Solidly HIGH-finding regression guard — get it right.**

## Affected files
- `src/adapters/types.rs` — `CurveMetadata`, `ProtocolMetadata::Curve` + Debug.
- `src/adapters/sim.rs` — `get_dy` ABI in the `sol!` block; add `"curve"` to the
  cfg-any gates on Bytes/ExecutionResult/AdapterCache/`run_quote`.
- `src/adapters/curve.rs` — NEW: `CurveAdapter` + `CurveColdStartPlanner`.
- `src/adapters/mod.rs` — `#[cfg(feature="curve")] pub mod curve;` + exports
  `CurveAdapter`, `CurveMetadata`.
- `Cargo.toml` — `curve = ["adapters"]`; add to `default`; add `"curve"` to the
  required-features of the adapter test entries that use it.

## Test plan (manager-authored; see below for what's already written)
- `adapter_swap_sim.rs`: offline `get_dy` quote via a mock pool (return slot0);
  + token-not-in-pool / missing-coins error paths.
- `adapter_reactive.rs`: `TokenExchange` → `VerifySlots` over `discovered_slots`;
  empty `discovered_slots` → `None`; non-Curve/unknown metadata no-mutation
  (batch-robust, no error).
- `cold_start_adoption.rs`: discover→verify→Ready warms + persists
  `discovered_slots`; archive-miss (FetchFailed) → VerifySlots repair;
  discover-revert → ColdStart repair.
- `adapter_swap_sim_rpc.rs`: `#[ignore]` parity — fork mainnet at FORK_BLOCK
  (20_000_000), cold-start the live 3pool
  `0xbEbc44782C7dB0a1A60Cb6fe97d0b483032FF1C7` (coins DAI/USDC/USDT), assert
  `simulate_swap(DAI, USDC, 1e18)` == `eth_call get_dy(0,1,1e18)` ground truth
  (probe value at this block: 999900). **Manager runs this.**

## Verification commands (manager runs)
- `cargo fmt --all -- --check`
- `cargo clippy --all-targets --all-features -- -D warnings`
- `cargo clippy --no-default-features -- -D warnings`
- per-protocol isolation builds incl. `--no-default-features --features curve`
  (Solidly regression guard).
- `cargo test` (default), `cargo test --no-default-features`.
- `cargo test --test adapter_swap_sim_rpc -- --ignored` (with `E2E_RPC_URL`).
- `cargo doc --no-deps -D warnings`.

## Probe-confirmed facts (mainnet 3pool @ block 20_000_000)
- `0xbEbc44782C7dB0a1A60Cb6fe97d0b483032FF1C7`; coins[0]=DAI
  `0x6B17…1d0F`, coins[1]=USDC `0xA0b8…eB48`, coins[2]=USDT `0xdAC1…1ec7`;
  coins[3] reverts (n_coins=3).
- `get_dy(int128,int128,uint256)` maps to native `i128` in the `sol!` macro.
- `get_dy(0, 1, 1e18)` = `999900`. A()=2000, fee()=1e6.
- `balances[0]` is NOT in slots 0..12 → no predictable layout → discover-based
  is mandatory.
```
```
