# Protocol support matrix (v0.3.0-alpha.7)

What each protocol adapter actually guarantees, so you can tell at a glance where
a pool is fully offline, where the reactive path avoids RPC, and where a live
backend or extra setup is still needed. This complements the summary table in the
[README](../README.md#supported-protocols).

## Capabilities

| Protocol (feature) | Cold-start | Offline quote after cold-start | Reactive event → state | Factory discovery | Known limitations |
| --- | --- | --- | --- | --- | --- |
| **Uniswap V2** (`uniswap-v2`) | named slots (token0/1, reserves) | ✅ fully offline | `Sync` → **exact** masked write, no RPC | ✅ `getPair[t0][t1]` | — |
| **Canonical Uniswap V3** (`uniswap-v3`) | complete canonical globals + observation head + bounded multi-word tick window (all four `Tick.Info` words), or the one-shot full-range program | ✅ offline within the warmed tick window; use one-shot full sync for zero-lazy quotes | ordered `Swap` + complete chain/block/transaction context and parent → **exact** slot0, active liquidity, input fee growth, protocol fees, observation ring, and every crossed tick; any missing/mismatch purges stale state and requests repair. Every standard non-`Swap` mutation is routed and whole-storage purged pending exact rebuild | ✅ fee-keyed `getPool[t0][t1][fee]` | exact transition has a deterministic 4,096-step ceiling; complete canonical batches enforce strict position order and the live actor rejects duplicates, while the direct driver remains stateless |
| **PancakeSwap V3** (`pancake-v3`) | Pancake layout warm-up | ✅ as Uniswap V3 | `Swap` exactness **unsupported**: routed events invalidate and repair rather than applying canonical semantics; routed canonical non-`Swap` topics also purge rather than reusing canonical layout semantics | ✅ fee-keyed | independent deployed-bytecode fee/oracle/tick parity is required before exact capability |
| **Reviewed Slipstream / Aerodrome CL** (`slipstream`) | full-range quote surface; resumable cold-start verifies extended globals and all six initialized-tick words | ✅ with a compatible Slipstream quoter and its runtime read-set resident | Base BIFI and Optimism mooBIFI: ordered `Swap` → exact quote/search replay with inferred effective fee; `Mint`/`Burn` → exact geometry, bitmap, tick, active-liquidity, and oracle transition; `Collect` → exact search no-op. No event transition requests provider reconstruction | ✅ tickSpacing-keyed `getPool[t0][t1][spacing]` | capability is address/chain/layout scoped. Optional runtime-bound evidence adds byte-exact swap accounting. Position/token accounting, silent gauge stake/unstake, administrative mutations, and other Slipstream deployments remain out of scope and fail closed |
| **Balancer V2** (`balancer-v2`) | discover→verify (`getPoolTokens` read-set); verify-only fast path once the read-set is known | ✅ (the vault's code is lazily fetched on the first quote) | `Swap` → **exact** 112-bit `cash`-field writes where the probed cash locations are warm (TWO_TOKEN + GENERAL specializations), **resync** fallback; `PoolBalanceChanged` → **resync** | ❌ not shipped (no on-chain token→pool index; needs an async log scan) | register pools explicitly |
| **Solidly V2** (`solidly-v2`) | named slots (config layout: reserves + tokens) | ⚠️ `getAmountOut` also reads the pool's `stable` flag + token `decimals` and STATICCALLs `factory.getFee()`, so the first offline quote lazily fetches those (and the factory code) unless a backend is attached or they are pre-warmed | `Sync` → **exact** two-slot write, no RPC | ✅ `getPool[t0][t1][bool stable]` (Aerodrome preset verified on Base) | Velodrome/Optimism reuses Aerodrome's constants — unverified on Optimism |
| **Curve** — StableSwap, StableSwap-NG, CryptoSwap v2, Tricrypto-NG (`curve`) | discover→verify (`get_dy` read-set); verify-only fast path once the read-set is known | ✅ (the pool's code is lazily fetched on the first quote) | `TokenExchange` + liquidity events → discovered-slot **resync** | ❌ not shipped (needs the Vyper MetaRegistry view call) | metapools / lending pools out of scope (their `get_dy` makes external calls a pool-only capture misses) |

Legend: **exact** = the event plus its declared exact parent/context reproduce
the adapter's complete declared transition surface with no RPC. For canonical
Uniswap V3 that surface is the complete swap-mutated pool storage; for reviewed
Slipstream it is the complete state that can affect subsequent quote/search
evaluation, with stronger accounting parity available when runtime-bound fee
evidence is supplied;
**resync** = the event carries only deltas, so the affected slots are re-verified
via a bounded, hash-pinned request (block trace → bulk-storage / point-read
fallback), executed by [`AmmSyncEngine`](../src/adapters/sync_manager.rs). A
"resync" is not RPC in the quote hot path — it is a targeted refresh triggered by
a liquidity/swap event, resolved off the block's own trace where possible.

## Notes

- **First-quote lazy fetch.** A warmed pool quotes offline, but a contract's own
  runtime *code* is fetched lazily on first use unless it was bytecode-seeded
  (Uniswap V2/V3 are; Curve accepts a caller-supplied seed via
  `CurveMetadata::with_code_seed`; Balancer/Solidly fetch code lazily). With a
  live-backed [`EvmCache`](https://github.com/KaiCode2/evm-fork-cache) this is a
  one-time cost; against a pinned/offline backend, seed or pre-warm what a quote
  reads. See the README's *Solidly offline caveat* for the one protocol whose
  quote read-set exceeds what its cold-start warms.
- **Extending coverage.** A protocol without a shipped adapter or discovery
  mechanism is not a fork: `register_adapter(Arc<dyn AmmAdapter>)` adds a novel
  simulation engine and `PoolDiscovery::with_factory(Box<dyn PoolFactory>)` adds a
  novel discovery mechanism. See [`docs/writing-an-adapter.md`](writing-an-adapter.md)
  and [`docs/pool-discovery.md`](pool-discovery.md).
- **Verification.** The factory-preset storage constants are confirmed on-chain by
  the gated `discovery_cl_rpc` / `discovery_solidly_rpc` parity tests, and each
  protocol's `simulate_swap` is checked against the on-chain `eth_call` quote at a
  pinned block by `adapter_swap_sim_rpc` (see [`docs/benchmarks.md`](benchmarks.md)
  for the reproduce commands).
