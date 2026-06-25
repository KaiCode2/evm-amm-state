# Curve slice 3 — complete liquidity-event routing — spec

Completes reactive coverage for the Curve dialects the adapter already supports
(StableSwap incl. NG, and CryptoSwap v2) by routing their **liquidity** events
(not just `TokenExchange`) into the `VerifySlots` resync. Plus a live WebSocket
soak test.

All signatures below were **verified on-chain** (eth_getLogs topic0 histogram
over blocks [19_900_000, 20_000_000]) against real pools — no guessed sigs.

## Verified signatures (per dialect)

### Classic StableSwap — 3pool (already routed by slice 1, CONFIRMED)
- `TokenExchange(address,int128,uint256,int128,uint256)` (3769x)
- `AddLiquidity(address,uint256[N],uint256[N],uint256,uint256)` (435x, N=3)
- `RemoveLiquidity(address,uint256[N],uint256[N],uint256)` (3x)
- `RemoveLiquidityOne(address,uint256,uint256)` — **2-arg** (951x)
- `RemoveLiquidityImbalance(address,uint256[N],uint256[N],uint256,uint256)` (slice-1 derived)

### StableSwap-NG — crvUSD/USDT + crvUSD/USDC (fixed arrays, NOT dynamic!)
- `TokenExchange(address,int128,uint256,int128,uint256)` — same int128 as classic
- `AddLiquidity(address,uint256[N],uint256[N],uint256,uint256)` — fixed[N], already routed
- `RemoveLiquidity(address,uint256[N],uint256[N],uint256)` — already routed
- `RemoveLiquidityImbalance(address,uint256[N],uint256[N],uint256,uint256)` — already routed
- **NEW: `RemoveLiquidityOne(address,uint256,uint256,uint256)` — 3-arg** (the only NG gap)

### CryptoSwap v2 — tricrypto2 (NEW: route these)
- `TokenExchange(address,uint256,uint256,uint256,uint256)` (slice 2, verified)
- **NEW: `AddLiquidity(address,uint256[N],uint256,uint256)`** — single `uint256 fee` (not array)
- **NEW: `RemoveLiquidity(address,uint256[N],uint256)`** — no fees array
- **NEW: `RemoveLiquidityOne(address,uint256,uint256,uint256)`** — 3-arg
- (CryptoSwap v2 has **no** RemoveLiquidityImbalance.)

## Scope
- Complete liquidity-event routing for **StableSwap** (classic + NG) and
  **CryptoSwap v2**, all → `VerifySlots` resync over `discovered_slots` (the same
  delta-free resync model; payloads not decoded except `TokenExchange`).
- Live WebSocket soak test.

Non-goals (documented; verify-first follow-ups):
- **Tricrypto-NG** (e.g. `0x7F86…829B`) is a SEPARATE 4th dialect — its dominant
  event is `0x143f1f8e…` (an extended `TokenExchange`, NOT the v2 one), and its
  liquidity events differ too. It is NOT currently supported even for swaps;
  adding it is a future variant (`CryptoSwapNG`), out of scope here.
- True dynamic-`uint256[]` NG pools: none observed (both NG pools tested use
  fixed `[N]`). If one is found, verify-then-route.

## Design (extends slice 2; stays a 2-variant model)
`curve_event_topics(n_coins, variant)` and `decode_event` gain the full
liquidity set per variant:
- **StableSwap**: existing set **+** `RemoveLiquidityOne(address,uint256,uint256,uint256)`
  (3-arg, for NG). Keep the 2-arg form (classic). A pool emits only one form;
  routing both is harmless (the other never fires).
- **CryptoSwap**: existing `TokenExchange` **+**
  `AddLiquidity(address,uint256[N],uint256,uint256)` **+**
  `RemoveLiquidity(address,uint256[N],uint256)` **+**
  `RemoveLiquidityOne(address,uint256,uint256,uint256)`.

`decode_event` kind mapping: `AddLiquidity → LiquidityAdded`; all `Remove*` →
`LiquidityRemoved`; `TokenExchange → Swap`. Topic hashes derived from `n_coins`
via `format!` + `keccak256` (the slice-1 pattern), with `#[cfg(test)]` cross-checks
that the N=3 derivations equal `sol!`-macro `SIGNATURE_HASH`es (add `sol!` event
decls for the new shapes so the macro computes the reference hashes).

Batch-robustness unchanged: missing/empty config → `RepairAction::None`, never an
error. Swap path / get_dy / cold-start unchanged.

## Tests
- `curve.rs` `#[cfg(test)]`: derived topics == `sol!` macro hashes for the new
  events (CryptoSwap AddLiquidity[3]/RemoveLiquidity[3]/RemoveLiquidityOne-3arg;
  StableSwap RemoveLiquidityOne-3arg). Variant routing: CryptoSwap set contains
  its AddLiquidity/RemoveLiquidity/RemoveLiquidityOne and NOT StableSwap's; etc.
- `adapter_reactive.rs`: inject (offline, through the runtime) a CryptoSwap
  `AddLiquidity` log and an NG 3-arg `RemoveLiquidityOne` log → assert each routes
  and resyncs the discovered slot to a fresh value (deterministic liquidity-resync
  correctness). Plus a kind-assertion (LiquidityAdded/Removed).
- `reactive_curve_ws_e2e.rs` (NEW, env-gated `#[ignore]`, MANAGER runs): the WS
  soak — see below.

## WS soak test (manager-authored + run)
Model on `tests/reactive_ws_e2e.rs` (V2). Env-gated on `E2E_RPC_URL` (derive the
`wss://` URL by scheme/host swap). Duration via env (default ~3–5 min).
- Cold-start a set of active supported pools at the connect-time block: 3pool
  (StableSwap), an NG pool, tricrypto2 (CryptoSwap). Subscribe **topic-only**
  (provider quirk: address-filtered subs deliver 0 — see memory) for the union of
  all supported Curve event topics; route each log by emitter→pool and apply the
  decoded repair (resync) against the cache.
- Continuously (each handled block / periodically) assert `simulate_swap` on each
  touched pool still equals the live `get_dy` at that block — i.e. event-driven
  state stays accurate.
- Tally observed events by kind; the test PASSES if accuracy holds throughout and
  ≥1 event was processed. Liquidity events are rare per-pool, so the soak proves
  the live subscribe→route→resync→accuracy pipeline; it asserts post-resync
  accuracy on every liquidity event it *does* observe. (Deterministic
  liquidity-resync correctness is covered by the offline reactive tests above.)
- Use `set -o pipefail`; never assert on a wall-clock count that could flake.

## Verification (manager runs)
fmt; clippy `--all-targets --all-features`; clippy `--no-default-features`; 5×
per-protocol isolation builds; `cargo test` default + `--no-default-features`;
doc; the existing tricrypto2 + 3pool RPC parity; and the WS soak.
