# Base Flashblocks latency benchmark

This benchmark measures when a Base Uniswap V3 swap becomes usable in the EVM
cache and AMM simulator through Flashblocks versus an ordinary canonical
WebSocket subscription. It is a focused live acceptance test, not a synthetic
microbenchmark.

## Method

- Provider: paid QuickNode Base HTTP and WebSocket endpoints.
- Pool: Uniswap V3 USDC/WETH 0.05%,
  `0xd0b53D9277642d899DF5C87A3966A349A798F224`.
- Window: 100 canonical swaps or five minutes, whichever comes first.
- Flashblock path: `newFlashblocks` plus pool-filtered `pendingLogs`.
- Vanilla path: pool-filtered canonical `logs` over WebSocket.
- Pairing key: `(transaction_hash, log_index)`.
- State isolation: two independently cold-started `EvmCache` and
  `AmmSyncEngine` instances. The vanilla cache never consumes speculative
  state. The Flashblock cache consumes canonical logs after measurement so its
  disposable overlay converges normally.
- Cache-ready time: notification receipt through `AmmSyncEngine::ingest_batch`.
- AMM-ready time: cache-ready time plus a successful local
  `simulate_swap(1 USDC -> WETH)` against the updated state.
- Build: optimized release profile.

Run it with:

```bash
BASE_HTTP_URL=<paid-base-http-endpoint> \
BASE_WS_URL=<paid-base-websocket-endpoint> \
FLASHBLOCKS_BENCH_SECONDS=300 \
FLASHBLOCKS_BENCH_MAX_SWAPS=100 \
cargo run --release --example flashblocks_latency_live --features uniswap-v3
```

## Result

Measured July 28, 2026, starting from Base block `49,234,041`:

- Stop condition: five-minute time limit.
- Flashblock swaps: 73.
- Canonical swaps: 73.
- Exact paired swaps: 73.
- Quote-read head-lag retries during the measured run: zero on both paths.

| Metric | Samples | Mean | p50 | p95 | Max |
| --- | ---: | ---: | ---: | ---: | ---: |
| Flashblock notification lead | 73 | 1,272.770 ms | 1,248.957 ms | 1,981.938 ms | 2,111.066 ms |
| Cache-ready lead | 73 | 1,272.231 ms | 1,247.927 ms | 1,982.043 ms | 2,111.011 ms |
| AMM-quote-ready lead | 73 | 1,166.368 ms | 1,157.888 ms | 1,858.450 ms | 2,053.348 ms |
| Flashblock cache apply | 73 | 0.934 ms | 0.425 ms | 3.529 ms | 12.264 ms |
| Canonical cache apply | 73 | 0.398 ms | 0.250 ms | 1.519 ms | 2.024 ms |
| Flashblock quote | 73 | 108.950 ms | 173.250 ms | 201.050 ms | 266.987 ms |
| Canonical quote | 73 | 3.087 ms | 0.134 ms | 1.171 ms | 205.759 ms |

Flashblocks moved the dominant event-arrival boundary forward by about 1.27
seconds on average. Cache application itself remained sub-millisecond at the
median. A usable AMM quote was ready about 1.17 seconds earlier on average even
though speculative quotes were materially slower than canonical quotes in this
sample.

The speculative quote distribution is an integration concern, not a reason to
discard the notification gain. The roughly 170--200 ms mode is consistent with
lazy quote reads being repeated as cumulative Flashblock overlays are replaced,
but this run did not isolate the exact cache-miss source. It should be explained
and then fixed or explicitly budgeted before treating the path as
production-ready.

## Alpha status and production-rollout gate

The first Flashblocks alpha may ship with the repeated lazy-read cost above:
speculative state remains correct and disposable, and the measured AMM-ready
path still led canonical delivery by about 1.17 seconds on average. Production
rollout is gated on both layers of read-set retention:

1. Prime representative AMM quote calls before attaching the subscriber so the
   canonical baseline already contains their account and storage read sets.
2. Preserve unrelated lazy cache fills across preconfirmed branch replacement
   while restoring every speculative account, slot, balance, resync, and purge
   effect exactly.

Acceptance requires provider-read instrumentation proving that an unchanged
quote path does not repeatedly fault to RPC across Flashblocks, followed by the
same paid-provider benchmark showing that the recurring provider-round-trip
latency mode is gone. This work belongs in `evm-fork-cache` and
`evm-amm-state`; consumers should not need a parallel Flashblocks-specific
warming path.

## Provider head visibility

An acceptance attempt immediately before the measured run received a canonical
WebSocket log before the same QuickNode HTTP endpoint could query that block by
hash. The first lazy quote read returned `block not found`; the hash became
queryable shortly afterward. The benchmark therefore retries that specific
transient response for up to three seconds and includes any wait in AMM-ready
latency. The final measured run required zero retries.

Stable provider identity and preferred routing remain useful, but they cannot
alone guarantee that separate HTTP and WebSocket sessions have identical head
visibility at notification time. Consumers must either avoid lazy reads on the
hot path, use a provider/session surface that exposes the announced state, or
handle this bounded lag explicitly.

## Scope

This result proves the paid QuickNode Base subscription, pending-log
correlation, speculative cache application, canonical convergence, and V3 quote
path for one active pool and one five-minute window. It does not prove OP
Flashblocks, other providers, reconnect recovery, or long-duration reliability.
