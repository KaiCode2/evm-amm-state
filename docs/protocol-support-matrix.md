# Protocol support matrix (v0.1)

What each protocol adapter actually guarantees, so you can tell at a glance where
a pool is fully offline, where the reactive path avoids RPC, and where a live
backend or extra setup is still needed. This complements the summary table in the
[README](../README.md#supported-protocols).

## Capabilities

| Protocol (feature) | Cold-start | Offline quote after cold-start | Reactive event → state | Factory discovery | Known limitations |
| --- | --- | --- | --- | --- | --- |
| **Uniswap V2** (`uniswap-v2`) | named slots (token0/1, reserves) | ✅ fully offline | `Sync` → **exact** masked write, no RPC | ✅ `getPair[t0][t1]` | — |
| **Uniswap V3** (`uniswap-v3`) | slot0 + liquidity + a bounded multi-word tick window (all four `Tick.Info` words), or the one-shot full-range program | ✅ offline within the warmed tick window; a swap crossing beyond it lazily fetches (or use the one-shot full sync for zero-lazy) | `Swap` → **exact** slot0/liquidity; `Mint`/`Burn` → bounded tick-range **resync** | ✅ fee-keyed `getPool[t0][t1][fee]` | — |
| **PancakeSwap V3** (`pancake-v3`) | as Uniswap V3 (Pancake slot layout) | ✅ as Uniswap V3 | as Uniswap V3 (Pancake `Swap` topic) | ✅ fee-keyed | one-shot full sync uses the layout-only `core` spec (Pancake's fee-growth/observation slots are unverified) |
| **Slipstream / Aerodrome CL** (`slipstream`) | as Uniswap V3 (Slipstream slot layout) | ⚠️ **discovery + cold-start only** | as Uniswap V3 | ✅ tickSpacing-keyed `getPool[t0][t1][spacing]` | discovered `fee` is left unset (its quoter takes a different ABI); `simulate_swap` returns `MissingMetadata` unless the caller supplies a compatible quoter + fee |
| **Balancer V2** (`balancer-v2`) | discover→verify (`getPoolTokens` read-set) | ✅ (the vault's code is lazily fetched on the first quote) | `Swap` → balance-slot **resync** | ❌ not shipped (no on-chain token→pool index; needs an async log scan) | register pools explicitly |
| **Solidly V2** (`solidly-v2`) | named slots (config layout: reserves + tokens) | ⚠️ `getAmountOut` also reads the pool's `stable` flag + token `decimals` and STATICCALLs `factory.getFee()`, so the first offline quote lazily fetches those (and the factory code) unless a backend is attached or they are pre-warmed | `Sync` → **exact** two-slot write, no RPC | ✅ `getPool[t0][t1][bool stable]` (Aerodrome preset verified on Base) | Velodrome/Optimism reuses Aerodrome's constants — unverified on Optimism |
| **Curve** — StableSwap, StableSwap-NG, CryptoSwap v2, Tricrypto-NG (`curve`) | discover→verify (`get_dy` read-set) | ✅ fully offline | `TokenExchange` + liquidity events → discovered-slot **resync** | ❌ not shipped (needs the Vyper MetaRegistry view call) | metapools / lending pools out of scope (their `get_dy` makes external calls a pool-only capture misses) |

Legend: **exact** = the event carries absolute state, applied with no RPC;
**resync** = the event carries only deltas, so the affected slots are re-verified
via a bounded, hash-pinned request (block trace → bulk-storage / point-read
fallback), executed by [`AmmSyncEngine`](../src/adapters/sync_manager.rs). A
"resync" is not RPC in the quote hot path — it is a targeted refresh triggered by
a liquidity/swap event, resolved off the block's own trace where possible.

## Notes

- **First-quote lazy fetch.** A warmed pool quotes offline, but a contract's own
  runtime *code* is fetched lazily on first use unless it was bytecode-seeded
  (Uniswap V2/V3 are; Balancer/Curve/Solidly fetch code lazily). With a
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
