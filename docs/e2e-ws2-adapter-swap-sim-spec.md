# E2E WS2 — Finalize adapters (V2 / V3 / Balancer V2) + swap simulation

Manager-owned spec for the keystone workstream. Implementation agent builds to it;
manager authors the acceptance tests and reviews.

## Objective

Make Uniswap V2, Uniswap V3, and Balancer V2 "fully finished" on the adapters
path: (1) reactive cache mutation in response to events, and (2) a **swap
simulation** that executes the swap in revm against the cold-start snapshot —
**no `amm-math` / `LocalAMM` / hand-rolled AMM math**. The deployed contract
bytecode does the math; we only build calldata, run it via `AdapterCache::call_raw`,
and decode the output.

## Scope

1. **Swap-sim surface** — a new `AmmAdapter` method:
   ```rust
   fn simulate_swap(
       &self,
       pool: &PoolRegistration,
       cache: &mut dyn AdapterCache,
       token_in: Address,
       token_out: Address,
       amount_in: U256,
   ) -> Result<SwapQuote, SimError>;   // default: Err(SimError::Unsupported(self.protocol()))
   ```
   `SwapQuote { amount_out: U256 }` (extensible). Per-protocol it builds the
   protocol's canonical quote calldata and runs `cache.call_raw(from = ZERO, to =
   <quote target>, calldata, commit = false)`, then decodes `amount_out` from the
   `ExecutionResult` output. A revert/halt → `Err(SimError::Reverted)`.
   - **Uniswap V3 (+ family):** `QuoterV2.quoteExactInputSingle((tokenIn, tokenOut,
     amountIn, fee, sqrtPriceLimitX96=0))`. Target = the QuoterV2 contract.
   - **Uniswap V2:** `UniswapV2Router02.getAmountsOut(amountIn, [tokenIn, tokenOut])`.
     Target = the router. (This executes the on-chain `UniswapV2Library` math
     against the warmed pair reserves — chain code, not ours.)
   - **Balancer V2:** `Vault.queryBatchSwap(GIVEN_IN, [swap], assets, funds)`.
     Target = the vault (from `BalancerV2Metadata.vault`).
2. **Quote-target addresses** — resolved from a `SimConfig`/registry default
   (mainnet QuoterV2, Router02) with per-pool/chain override; Balancer uses the
   pool's vault. The quote contract's bytecode must be in the cache: lazily
   fetched against a live backend; **installed as a fixture** for offline tests.
3. **Balancer V2 reactive mutation** — currently routing-only
   (`balancer_v2.rs:103-112`, `updates: Vec::new()`). On a `Swap` log for a
   tracked pool, keep the pool's cached vault balances fresh so a subsequent
   `simulate_swap` reflects the new state. **Preferred mechanism:** refresh
   (re-verify) the cold-start-discovered vault balance slots rather than
   reverse-engineering the vault's balance-mapping layout or doing lossy
   event-delta arithmetic — this stays consistent with the discover-based cold
   start. This requires the discovered balance slots to be reachable from
   `decode_event`/`after_apply`; persist them (e.g. on `BalancerV2Metadata` or a
   side field set in `finish`). The acceptance test asserts the **outcome** (a
   post-`Swap` sim reflects refreshed balances), not the mechanism.

## Non-goals

- Curve, Solidly, Balancer V3, ERC4626, Uniswap V4 (scaffold-only — leave as is).
- Pre-warming all V3 tick/bitmap slots in cold-start (deferred A4 work). The sim
  stays **correct** for tick-crossing swaps because `EvmCache` lazily fetches any
  cold slot from the backend; full offline pre-warming is a follow-up.
- No `amm-math`/`LocalAMM`/`stableswap_math`/`cryptoswap_math` use.
- No new production crate dependencies (quote ABIs are local `sol!`).

## Acceptance criteria

1. `simulate_swap` returns a correct `amount_out` for V2, V3, and Balancer V2.
2. **Offline harness test** (manager-authored, per protocol): a pool is
   cold-started into an `EvmCache`, a **mock quote contract** (installed at the
   quote target, returning a deterministic value derived from the warmed pool
   slots — same fixture style as `MockBalancerVault`) is used, `simulate_swap`
   returns that value, fully offline (`asserter.read_q().is_empty()`). A reverting
   quote target → `Err(SimError::Reverted)`.
3. **Balancer reactive test** (manager-authored): after cold-start, ingesting a
   `Swap` log refreshes the discovered balance slots (the offline fetcher returns
   new balances), and a subsequent `simulate_swap` reflects the change. Before
   this work the `Swap` produces no cache mutation (red).
4. **RPC parity test** (manager-authored, `#[ignore]`/env-gated on `E2E_RPC_URL`):
   fork at a pinned block, cold-start a known mainnet pool (e.g. a USDC/WETH V3
   0.05% pool, a V2 pair, a Balancer weighted pool), run `simulate_swap`, and
   assert it equals the **same quote executed via the provider's `eth_call` at the
   same block** (on-chain ground truth). Exact match expected (identical bytecode
   + state). Document the pinned block + pool addresses in the test.

## Affected files

- `src/adapters/traits.rs` (new method + default), `src/adapters/types.rs`
  (`SwapQuote`, `SimError`), `uniswap_v2.rs`, `uniswap_v3.rs`, `balancer_v2.rs`
  (impls + Balancer reactive), maybe `cache.rs`/a new `sim.rs` for shared
  calldata/decoding helpers + quote-target config. New `sol!` quote ABIs.
- `tests/`: a new `adapter_swap_sim.rs` (offline harness + Balancer reactive) and
  `adapter_swap_sim_rpc.rs` (gated parity); new mock-quote fixtures under
  `tests/fixtures/`.

## Verification

```
CARGO_NET_GIT_FETCH_WITH_CLI=true cargo test --test adapter_swap_sim --test adapter_reactive --test cold_start_adoption
cargo fmt --all --check
cargo clippy --all-targets --no-deps -- -D warnings
cargo clippy --no-default-features --features adapters,uniswap-v2,uniswap-v3,balancer-v2 --all-targets --no-deps -- -D warnings
cargo clippy --no-default-features --all-targets --no-deps -- -D warnings
cargo test && cargo test --no-default-features
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps
# Manual (manager runs): E2E_RPC_URL=<archive> cargo test --test adapter_swap_sim_rpc -- --ignored
```

## Open questions / assumptions

- Assumption: quote via canonical Quoter/Router/Vault entrypoints (above) is
  acceptable as "swap tx" — it runs chain bytecode, not reimplemented math.
- Assumption: V3 family quote uses `fee`/`tick_spacing` from `V3Metadata`.
- `simulate_swap` takes `&mut dyn AdapterCache` (call_raw needs `&mut`); confirm no
  borrow conflicts with the registry's adapter dispatch and adjust the call site.
