# Curve slice 4 — Tricrypto-NG variant — spec

Adds the **Tricrypto-NG** dialect (Curve's newest 3-coin crypto pools, e.g.
tricryptoUSDC `0x7F86…829B`, tricryptoUSDT `0xf5f5…e2B4`). It shares CryptoSwap
v2's `uint256` `get_dy`, but emits **extended** events (extra `fee` /
`packed_price_scale` fields). A new `CurveVariant::CryptoSwapNG` selects the
extended event routing while reusing the v2 quote/cold-start path.

All signatures verified on-chain across BOTH pools (eth_getLogs topic0 histogram,
blocks [19_900_000, 20_000_000]) — no guessed signatures.

## Probe-confirmed facts
- `get_dy(uint256,uint256,uint256)` works; `int128` reverts → reuse the existing
  `CurveCryptoSwap::get_dy` (uint256) path. Ground truth `get_dy(0,1,1e6)` =
  1476 (tricryptoUSDC USDC→WBTC), 1471 (tricryptoUSDT). coins[3] reverts (n=3).
- Events emitted (confirmed on both pools):
  - `TokenExchange(address,uint256,uint256,uint256,uint256,uint256,uint256)` — 7-arg
    (buyer indexed; sold_id, tokens_sold, bought_id, tokens_bought, fee,
    packed_price_scale). Distinct from v2's 5-arg.
  - `AddLiquidity(address,uint256[3],uint256,uint256,uint256)` — 5-arg
    (token_amounts[N], fee, token_supply, packed_price_scale).
  - `RemoveLiquidity(address,uint256[3],uint256)` — same as CryptoSwap v2.
  - `RemoveLiquidityOne(address,uint256,uint256,uint256,uint256,uint256)` — 6-arg
    (token_amount, coin_index, coin_amount, approx_fee, packed_price_scale).
  - `ClaimAdminFee(address,uint256)` — frequent (24x/13x); claim_admin_fees can
    update D/price_scale (the crypto-pool read-set), so route it → resync.
  - (ERC20 `Transfer`/`Approval` from the LP token — ignored.)

## Design (extends the variant model to 3 variants)
`CurveVariant::CryptoSwapNG`. The `get_dy` index ABI is `uint256` — IDENTICAL to
`CryptoSwap` — so `simulate_swap` and the cold-start discover call **collapse**
CryptoSwap + CryptoSwapNG into one `uint256` arm (no new quote code). The variant
only diverges in **event routing**:
- `curve_event_topics(n, CryptoSwapNG)`: the NG TokenExchange (7-arg) + NG
  AddLiquidity (5-arg, arity-derived `uint256[N]`) + RemoveLiquidity
  (`uint256[N]`, supply) + NG RemoveLiquidityOne (6-arg) + ClaimAdminFee.
- `decode_event`: NG TokenExchange → `Swap` (decode-validated with the NG event);
  NG AddLiquidity → `LiquidityAdded`; RemoveLiquidity / NG RemoveLiquidityOne →
  `LiquidityRemoved`; ClaimAdminFee → `Unknown` (a protocol-internal state update,
  routed conservatively → resync). All → `VerifySlots` resync; batch-robust.

`sol!` event decls for the NG signatures live in a new `CurveTricryptoNgEvents`
interface (namespaced; for the `#[cfg(test)]` cross-check + TokenExchange decode).
The fixed-arity (`uint256[N]`) liquidity hashes are derived from `coins.len()` via
`format!`+`keccak256` (the slice-1/3 pattern), cross-checked against the macro at
N=3.

## Affected files
- `src/adapters/types.rs` — `CurveVariant::CryptoSwapNG`.
- `src/adapters/curve.rs` — collapse uint256 get_dy (CryptoSwap|CryptoSwapNG) in
  simulate_swap + cold-start discover; NG event sol! decls + variant-aware topics
  + decode kind; unit cross-checks.
- `tests/adapter_reactive.rs` — NG TokenExchange + AddLiquidity + RemoveLiquidityOne
  (+ ClaimAdminFee) each route → resync.
- `tests/adapter_swap_sim_rpc.rs` — live tricryptoUSDC parity (CryptoSwapNG).
- `tests/reactive_curve_ws_e2e.rs` — register a Tricrypto-NG pool; subscribe its
  topics; observe NG events flowing live.

## Tests
- unit: NG topic hashes == sol! macro; NG vs v2 TokenExchange differ; NG variant
  routes its own set (and the shared RemoveLiquidity), not v2's TokenExchange.
- offline reactive: NG TokenExchange / AddLiquidity / RemoveLiquidityOne /
  ClaimAdminFee each route → resync.
- RPC parity (manager-run): tricryptoUSDC `simulate_swap(USDC,WBTC,1e6)` ==
  `eth_call get_dy(0,1,1e6)` (gt 1476).
- WS soak (manager-run): add a Tricrypto-NG pool; confirm its 7-arg TokenExchange
  + liquidity events flow + route live.

## Verification (manager runs)
fmt; clippy `--all-targets --all-features`; clippy `--no-default-features`; 5×
isolation builds; tests default + `--no-default-features`; doc; the tricrypto-NG
RPC parity; the WS soak.
