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
  cargo bench --bench reactive_apply     # fully offline — no env needed
  ```
  (`swap_sim` is env-gated — a no-op without the URL; Solidly uses Base:
  `E2E_BASE_RPC_URL`, or an Alchemy `E2E_RPC_URL` with
  `eth-mainnet`→`base-mainnet`. `reactive_apply` measures the event-sourced
  apply paths against a mock-backed cache with pre-warmed packed words, so it
  runs anywhere, including CI.)

## Results

> Point-in-time medians from a single host and run (July 2026, the v0.1.0
> release candidate, the Methodology host above) — treat them as
> order-of-magnitude, and re-run the reproduce command for numbers on your own
> machine. `cargo bench` prints Criterion's timing estimates
> (`time: [low mid high]` plus outlier counts) to stdout — add `-- --verbose`
> for mean/median/std-dev — and saves baseline data under `target/criterion/`.
> (This crate builds criterion without the `plotters` backend, so HTML reports
> are only generated if `gnuplot` is installed.) The medians below are the
> headline figures from that output.

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

Every event-sourced apply below is one `AdapterDriver::apply_log`: topic
routing, ABI decode, packed-word arithmetic, and the exact cache write(s) —
no RPC. The V2 row is measured in both harnesses (RPC-warmed `swap_sim` and
offline `reactive_apply`) and agrees; the other apply rows come from
[`benches/reactive_apply.rs`](../benches/reactive_apply.rs) (offline, July
2026, same host).

| Operation | Time | Notes |
| --- | ---: | --- |
| Apply one Uniswap V2 `Sync` (exact write) | **~250–285 ns** | one masked reserves-word write; ~3.5–4M events/sec |
| Apply one Balancer V2 `Swap` (TWO_TOKEN, event-sourced) | **~430 ns** | both 112-bit cash fields, one shared-slot write |
| Apply one Balancer V2 `Swap` (GENERAL, event-sourced) | **~520 ns** | two per-token cash-field writes |
| Apply one Uniswap V3 `Mint` (warm ticks, event-sourced) | **~1.4 µs** | packed gross/net on both boundary ticks + in-range global liquidity |
| Apply one Uniswap V3 `Burn` (warm ticks, event-sourced) | **~1.4 µs** | the same three writes, negated |
| Cold-start one pool (Uniswap V3) | **~1.06 s** | **one-time, network-bound** — archive-node latency dominates; amortized over every later offline quote |

The reactive event-sourced paths are effectively free relative to a quote
(hundreds of nanoseconds to ~1.4 µs vs 8–85 µs). Cold-start is a one-time
setup cost gated by RPC latency, not a steady-state cost — the crate's design
pays it once and then quotes offline forever.

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

Measured on July 2, 2026, using a paid Alchemy mainnet endpoint with the
benchmark's gzip-enabled `reqwest` client at block `25_446_111`, seven
iterations per path:

| Pool | Prior median | New median | Relative | Scope |
| --- | ---: | ---: | ---: | --- |
| Uniswap V3 USDC/WETH 0.05% | 150.4 ms | **124.8 ms** | **1.21× faster** | prior warms the active tick window; new loads full pool: 7,670 slots, 1,562 ticks, 723 observations |
| Uniswap V2 USDC/WETH | 74.6 ms | **73.0 ms** | **1.02× faster** | both paths load the same 3 slots |
| Balancer V2 80BAL/20WETH | 333.3 ms | **76.4 ms** | **4.36× faster** | prior discover→verify; new refreshes 5 known vault slots |
| Curve 3pool StableSwap | 361.3 ms | **74.7 ms** | **4.84× faster** | prior discover→verify; new refreshes 6 known pool slots |

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

### Curve cold-start: discovery vs a known read-set

A Curve pool's *first* cold start is a discover→verify run: it fetches the pool's
Vyper runtime and executes `get_dy` in a local revm over a cold cache, lazily
faulting in each slot it SLOADs. That first-discovery cost — not warmed quoting —
is what makes a cold Curve boot lag Uniswap V2/V3, whose hot state is a known slot
set (or tick-bitmap program) hydrated in one bundled `eth_call`.

Once the read-set is known, the gap closes to the one-shot figures above (the
same Curve 3pool row: **~361 ms → ~75 ms**). Two paths reuse a persisted
`CurveMetadata.discovered_slots` (from a prior discovery, a block trace, or a
registry):

- **verify-only `cold_start`** — the planner skips discovery and warms exactly
  the known slots in a single verify round;
- **`cold_start_many`** — the same read-set becomes one bundled storage program,
  the identical fast path Uniswap V2/V3 take.

[`examples/curve_cold_start_phases.rs`](../examples/curve_cold_start_phases.rs)
times all three (discovery vs verify-only vs `cold_start_many`) against a live
pool and prints the breakdown — run it for numbers on your own endpoint:

```bash
E2E_RPC_URL=<archive-url> cargo run --release --example curve_cold_start_phases
```

The optional `CurveMetadata::with_code_seed` removes the one lazy code fetch a
Curve pool otherwise pays on its first quote, matching the fully-offline V2/V3
profile after bootstrap.

### Event-time trace resync

[`examples/trace_resync_latency.rs`](../examples/trace_resync_latency.rs)
measures the live reactive repair path for a real Curve 3pool event:

```bash
E2E_RPC_URL=<https-mainnet-rpc> TRACE_RESYNC_ITERS=7 cargo run --release --example trace_resync_latency
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

Measured on July 2, 2026, using the same paid Alchemy mainnet endpoint with gzip
enabled, seven iterations against a Curve 3pool `TokenExchange` at block
`25_446_103`:

| Path | Median | Min..max | Slots | Resync updates | Failures |
| --- | ---: | ---: | ---: | ---: | ---: |
| Trace-only | 155.7 ms | 146.2..189.1 ms | 6 | 2 | 0 |
| Storage fallback | 26.7 ms | 22.6..28.7 ms | 6 | 6 | 0 |

For a single Curve pool, six direct storage reads are faster than tracing the
whole block. The trace path is still important for live multi-AMM syncing because
`evm-fork-cache` dedupes all stale-slot requests pinned to the same block into
one block trace, so the trace cost amortizes across many pools and slots.
Trace-only may apply fewer updates than the requested slot count because block
traces contain changed slots; unchanged requested slots can remain valid from the
pre-event cache state.

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
