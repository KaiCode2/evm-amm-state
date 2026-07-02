# Benchmarks & performance

This documents how `evm-amm-state` performs on its **offline hot path** — the
steady state after a pool is cold-started — and how that profile compares,
qualitatively, to the other ways people price AMM swaps.

> **Numbers below are real**, produced by [`benches/swap_sim.rs`](../benches/swap_sim.rs)
> (criterion, `bench` profile). The cross-tool comparison is **qualitative** —
> it positions our measured numbers against the *known characteristics* of the
> other approaches; the competitor figures are order-of-magnitude expectations
> from each approach's algorithm class, **not** benchmarks run here.

## Methodology

- **Host:** Apple M1 Pro (10 cores), `aarch64-apple-darwin`, `cargo bench`
  (optimized `bench` profile).
- **Setup (one-time, RPC):** fork Ethereum mainnet at block `20_000_000`
  (Base block `47_700_000` for Solidly), cold-start one real pool per protocol
  into an `EvmCache`. This is the only network step.
- **Measured (offline):** each `simulate_swap` runs the pool's own on-chain
  quote entrypoint in revm against the warmed cache — **no RPC, no reimplemented
  math**. Reactive apply decodes + routes + applies one event.
- **Reproduce:**
  ```bash
  E2E_RPC_URL=<archive-url> cargo bench --bench swap_sim
  ```
  (Env-gated — a no-op without the URL. Solidly uses Base: `E2E_BASE_RPC_URL`,
  or an Alchemy `E2E_RPC_URL` with `eth-mainnet`→`base-mainnet`.)

## Results

### `simulate_swap` — one offline quote (the repeated hot path)

| Protocol | Quote entrypoint | Median / quote | ≈ Quotes/sec |
| --- | --- | ---: | ---: |
| Solidly V2 | pool `getAmountOut` | **8.0 µs** | ~125,000 |
| Uniswap V2 | `Router02.getAmountsOut` | **9.3 µs** | ~107,000 |
| Curve StableSwap | pool `get_dy` (int128) | **43 µs** | ~23,000 |
| Balancer V2 | `Vault.queryBatchSwap` | **68 µs** | ~14,700 |
| Curve CryptoSwap | pool `get_dy` (uint256) | **80 µs** | ~12,500 |
| Uniswap V3 | `QuoterV2.quoteExactInputSingle` | **85 µs** | ~11,800 |

**Why the spread?** It tracks how much bytecode each quote executes in revm:

- **Constant-product pools (V2, Solidly) are cheapest** (~8–9 µs) — a quote is
  a couple of reserve reads + one multiply/divide.
- **Curve StableSwap (~43 µs)** runs the StableSwap invariant (a bounded
  Newton iteration); **CryptoSwap (~80 µs)** runs the heavier cryptoswap
  invariant.
- **Balancer (~68 µs)** executes the full Vault `queryBatchSwap` path.
- **Uniswap V3 (~85 µs)** is the heaviest because the quote runs the entire
  `QuoterV2` contract — it simulates the swap through tick-crossing logic and
  reverts to read back the amount. (A V3 quote is intrinsically more work than a
  V2 one; this is the cost of exactness.)

### Reactive event apply & cold-start

| Operation | Time | Notes |
| --- | ---: | --- |
| Apply one `Sync` (Uniswap V2 exact write) | **~249 ns** | decode + route + masked slot write; ~4M events/sec |
| Cold-start one pool (Uniswap V3) | **~1.06 s** | **one-time, network-bound** — archive-node latency dominates; amortized over every later offline quote |

The reactive exact-write path is effectively free relative to a quote. Cold-start
is a one-time setup cost gated by RPC latency, not a steady-state cost — the
crate's design pays it once and then quotes offline forever.

### One-shot sync latency — network-bound state loading

The storage-program loaders added for full/known-read-set syncing are measured
with [`examples/sync_latency.rs`](../examples/sync_latency.rs). This benchmark is
provider-bound: it times live `eth_call` state overrides plus cache injection,
not local revm quote execution.

Reproduce:

```bash
E2E_RPC_URL=<https-mainnet-rpc> SYNC_BENCH_ITERS=7 cargo run --release --example sync_latency
```

If `E2E_RPC_URL` is unset, the example falls back to
`https://ethereum.publicnode.com`; use a paid/archive endpoint for lower jitter.

Measured on July 2, 2026, using `https://ethereum.publicnode.com` at block
`25_445_700`, seven iterations per path:

| Pool | Prior median | New median | Relative | Scope |
| --- | ---: | ---: | ---: | --- |
| Uniswap V3 USDC/WETH 0.05% | 185.8 ms | **142.5 ms** | **1.30× faster** | prior warms the active tick window; new loads full pool: 7,670 slots, 1,562 ticks, 723 observations |
| Uniswap V2 USDC/WETH | 96.5 ms | **96.8 ms** | same | both paths load the same 3 slots |
| Balancer V2 80BAL/20WETH | 331.9 ms | **93.5 ms** | **3.55× faster** | prior discover→verify; new refreshes 5 known vault slots |
| Curve 3pool StableSwap | 352.6 ms | **82.7 ms** | **4.27× faster** | prior discover→verify; new refreshes 6 known pool slots |

Interpretation:

- **V3:** the comparison is conservative for the new path. The prior path is not
  a full-pool sync; it warms only the active tick window. The new path is faster
  while loading roughly 3.5× more slots and leaves the full tick range +
  observation ring resident.
- **V2:** no material improvement is expected because the old path already knows
  exactly three slots. The new loader mainly unifies the transport shape.
- **Balancer/Curve:** the new numbers assume the read-set metadata already
  exists. Today that metadata can come from discover→verify cold-start; the next
  `debug_traceBlockByNumber` integration should populate it from traces, avoiding
  the view-call discover round and keeping the one-shot refresh path.

### Event-time trace resync

[`examples/trace_resync_latency.rs`](../examples/trace_resync_latency.rs)
measures the live reactive repair path for a real Curve 3pool event:

```bash
E2E_RPC_URL=<https-mainnet-rpc> TRACE_RESYNC_ITERS=3 cargo run --release --example trace_resync_latency
```

The example finds a recent `TokenExchange`, cold-starts the pool at the previous
block to establish `discovered_slots`, then ingests that historical log through
`AmmSyncEngine` in two modes:

- **trace-only:** storage fallback returns errors, so success proves the block
  trace supplied all changed requested slots;
- **storage-fallback:** trace is disabled, so the configured storage batch
  fetcher refreshes the known read-set at the event block.

Provider support for `debug_traceBlockByHash` varies. If the trace-only row
reports failures, that endpoint did not supply enough trace data for the chosen
event and the production path would rely on the storage fallback instead.

## How this compares to other tools

Three broad approaches exist for "what does this pool quote?" They trade
differently along **per-quote speed**, **correctness/drift**, and **offline
capability**.

| Approach | Per-quote | Correctness | Offline? | Per-protocol cost |
| --- | --- | --- | --- | --- |
| **Reimplemented math** (amms-rs, Uniswap/Curve SDKs) | sub-µs–low-µs (faster) | drifts unless math is kept in lockstep with each contract upgrade | yes | must hand-write & maintain each protocol's exact math |
| **`evm-amm-state` (this crate)** | **8–85 µs (measured)** | exact — runs the real bytecode | **yes** | zero — any pool with a quote entrypoint works |
| **Node `eth_call`** (geth/reth/erigon over RPC) | ~10–100 ms+ (network round-trip, rate-limited) | exact — runs the real bytecode | no | zero |

Reading across:

- **vs reimplemented-math libraries** (e.g. `amms-rs`, protocol SDKs): closed-form
  arithmetic in pure Rust is *faster per quote* than executing bytecode in revm —
  typically by one to two orders of magnitude. The cost is **drift and coverage**:
  someone has to re-derive and maintain each protocol's exact math (fees,
  integer rounding, Curve's NG variants, V3 tick math, fee-on-transfer, hooks),
  and a quote is only as correct as that reimplementation. This crate trades raw
  per-quote speed for **zero-drift correctness** (it runs the deployed contract,
  so it cannot disagree with chain) and **uniform coverage** (any pool whose
  quote entrypoint exists works, with no new math to write).
- **vs a node's `eth_call`**: identical correctness — both execute the real
  bytecode — but `eth_call` is a **network round-trip per quote** (commonly tens
  to hundreds of milliseconds, and rate-limited), and it is not offline. We do
  the *same computation* locally in **microseconds**: roughly **3–4 orders of
  magnitude faster per quote**, with no rate limit, and fully offline once
  warmed. For repeated work — arbitrage scanning across many pools and sizes —
  this is the decisive difference.
- **vs hand-rolled local revm**: this is the same engine we use, so per-quote
  speed is comparable — but you would have to build cold-start, storage-slot
  discovery, and event-driven cache synchronization yourself. That machinery is
  exactly what this crate provides.

**Where we sit:** between reimplemented-math (faster per quote, but reimplements
and can drift) and `eth_call` (same correctness, but ~1000× slower and online).
We deliver `eth_call`-grade correctness at near-local-compute latency, fully
offline, with no per-protocol math to maintain — which is the right point on the
curve for an always-current, multi-protocol state engine driving simulation or
arbitrage search.

## Caveats

- Single machine (M1 Pro), one representative pool and input size per protocol,
  one historical block. Treat the numbers as **relative** (the shape across
  protocols) more than absolute; your hardware, pool, and input size will shift
  them.
- `simulate_swap` measures a **warm** quote. The first quote after cold-start may
  fault in a slot the cold-start did not pre-warm (a one-time fetch); the benches
  warm fully before measuring.
- The competitor rows are **qualitative** order-of-magnitude expectations from
  each approach's algorithm class, not benchmarks run here (see the note at the
  top).
