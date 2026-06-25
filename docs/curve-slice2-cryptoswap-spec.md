# Curve slice 2 — CryptoSwap (Curve v2) variant — implementation spec

Extends the slice-1 `CurveAdapter` to **CryptoSwap (Curve v2)** plain pools
(e.g. tricrypto2 USDT/WBTC/WETH). The difference from classic StableSwap is the
**index type**: CryptoSwap's `get_dy` and `TokenExchange` use `uint256` indices,
not `int128`. We add a per-pool variant selector and make the quote / cold-start
/ swap-event paths variant-aware. No reimplemented math (still the pool's own
`get_dy`).

## Probe-confirmed facts (mainnet @ block 20_000_000)
- tricrypto2 `0xD51a44d3FaE010294C616388b506AcdA1bfAAE46`: coins
  [USDT `0xdAC1…1ec7`, WBTC `0x2260…C599`, WETH `0xC02a…56Cc2`], n=3.
  **`get_dy(uint256,uint256,uint256)` works; `int128` reverts.** Ground truth
  `get_dy(0,1,100e6 USDT) = 147348` (WBTC sats).
- StableSwap-NG crvUSD/USDT: `get_dy(int128,int128,uint256)` works → **NG is
  already covered by the slice-1 (StableSwap) path; no new code for NG swaps.**

## Scope
In scope:
- A `CurveVariant` selector ({`StableSwap`, `CryptoSwap`}) on `CurveMetadata`,
  defaulting to `StableSwap` (slice-1 pools and NG unchanged & backward-compatible).
- CryptoSwap `simulate_swap` via `get_dy(uint256,uint256,uint256)`.
- CryptoSwap cold-start: discover via the uint256-index `get_dy` (else the
  discover call reverts → spurious DiscoverFailed). Same discover→verify machinery.
- CryptoSwap reactive **swap** resync: route the CryptoSwap `TokenExchange`
  (uint256 ids) → `VerifySlots` over discovered slots (same resync model).

Non-goals (documented follow-ups):
- CryptoSwap **liquidity-event** routing (AddLiquidity/RemoveLiquidity*). Their
  CryptoSwap signatures differ from StableSwap's AND from each other across v2
  generations; verifying every signature is its own task. `TokenExchange` (the
  swap, the dominant resync trigger) IS routed; liquidity-driven staleness on a
  CryptoSwap pool persists until the next swap (documented, not silent).
- StableSwap-NG **dynamic-array** (`uint256[]`) liquidity events (NG swaps work;
  NG liquidity-event routing is a follow-up — slice-1 derives fixed `uint256[N]`).
- Metapools / lending pools (external-call `get_dy`; `restrict_to=[pool]` discover
  is incomplete).

## Behavioral requirements

### `CurveVariant` (types.rs, next to `CurveMetadata`)
```rust
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CurveVariant {
    /// Classic StableSwap + StableSwap-NG: get_dy(int128,int128,uint256),
    /// TokenExchange(address,int128,uint256,int128,uint256).
    #[default]
    StableSwap,
    /// CryptoSwap (Curve v2): get_dy(uint256,uint256,uint256),
    /// TokenExchange(address,uint256,uint256,uint256,uint256).
    CryptoSwap,
}
```
Add `pub variant: CurveVariant` to `CurveMetadata` (last field; `Default` =
`StableSwap`, so existing `CurveMetadata { coins, discovered_slots }` literals
must add `variant: ...` or use `..Default::default()` — update slice-1 call sites).

### sim.rs — CryptoSwap get_dy ABI
The existing top-level `get_dy(int128,int128,uint256)` stays. Add the uint256
variant under a `sol!` **interface** to avoid a second `get_dyCall` name clash:
```rust
sol! {
    interface CurveCryptoSwap {
        function get_dy(uint256 i, uint256 j, uint256 dx) returns (uint256 dy);
    }
}
```
→ `CurveCryptoSwap::get_dyCall`. Keep it inside the existing
`#[cfg(any(... feature="curve" ...))]`-gated region usage (it's only referenced
by the curve adapter; ensure no dead-code/unused warning under each feature set).

### curve.rs
- `pool_variant(pool) -> CurveVariant` helper (StableSwap if not Curve metadata).
- **simulate_swap**: branch on variant — StableSwap builds `get_dyCall { i: i as
  i128, j: j as i128, dx }`; CryptoSwap builds `CurveCryptoSwap::get_dyCall {
  i: U256::from(i), j: U256::from(j), dx }`. Both via `run_quote` + decode U256.
  (The i==j self-swap guard + token→index mapping are variant-independent.)
- **cold-start**: `CurveColdStartPlanner` takes the variant; `initial_plan`'s
  discover `get_dy(0,1,DISCOVER_DX)` calldata uses the variant's ABI. `finish`
  must persist the variant into `CurveMetadata` (alongside coins/discovered_slots)
  so reactive + later sims keep it.
- **TokenExchange topic + events**: add the CryptoSwap `TokenExchange`
  (uint256 ids) sol! event. `curve_event_topics(n_coins, variant)` and
  `decode_event` select the swap topic by variant. StableSwap path unchanged.
  For CryptoSwap, route only `TokenExchange` (uint256-id) → `Swap` kind +
  `VerifySlots` resync (liquidity events out of scope, per non-goals). Keep the
  batch-robust `RepairAction::None` fallbacks.
  - NOTE the slice-1 arity-aware StableSwap liquidity topics stay for StableSwap.

### Quote/decode parity
CryptoSwap `simulate_swap` decode is a bare `uint256` (like StableSwap). Validate
the CryptoSwap `TokenExchange` log on decode (its own ABI) before emitting Swap.

## Affected files
- `src/adapters/types.rs` — `CurveVariant`, `CurveMetadata.variant`.
- `src/adapters/sim.rs` — `CurveCryptoSwap::get_dy` interface.
- `src/adapters/curve.rs` — variant-aware simulate_swap / cold-start / topics /
  decode_event; CryptoSwap `TokenExchange` sol! event.
- `src/adapters/mod.rs` — export `CurveVariant`.
- slice-1 test call sites constructing `CurveMetadata { coins, discovered_slots }`
  → add `variant: CurveVariant::StableSwap` (or `..Default::default()`).

## Test plan
- `adapter_swap_sim.rs`: CryptoSwap offline `get_dy` quote via the existing
  selector-agnostic `mock_curve_pool_runtime.hex` (it returns slot0 for ANY
  selector, so it serves the uint256 variant too) — assert the quote and that
  the StableSwap path is unchanged. (The mock can't distinguish ABIs; the RPC
  parity test gates the real uint256 ABI.)
- curve.rs `#[cfg(test)]`: `CurveVariant::CryptoSwap` TokenExchange topic !=
  StableSwap TokenExchange topic; both route under their variant.
- `cold_start_adoption.rs`: a CryptoSwap-variant cold-start reaches Ready +
  persists `variant: CryptoSwap` and the discovered slots (generic mock).
- `adapter_swap_sim_rpc.rs`: **live tricrypto2 parity** — cold-start tricrypto2
  (variant CryptoSwap, coins [USDT,WBTC,WETH]), `simulate_swap(USDT, WBTC, 100e6)`
  == `eth_call get_dy(0,1,100e6)` (uint256 ABI). MANAGER runs.

## Verification (manager runs)
fmt; clippy `--all-targets --all-features -D warnings`; clippy `--no-default-features`;
all 5 per-protocol isolation builds incl. `--no-default-features --features curve`;
`cargo test` default + `--no-default-features`; `cargo doc -D warnings`;
`cargo test --test adapter_swap_sim_rpc -- --ignored` (mainnet + tricrypto2).
