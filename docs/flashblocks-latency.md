# Flashblocks latency benchmark

**Current status: in development, not qualified for live execution or a latency
claim.** The `0.3.0` release includes the opt-in integration for experimentation.
Its canonical AMM validation does not qualify Flashblocks. The September 5
revalidation below did not pass; the historical results describe earlier source
and crate versions.

This benchmark measures when a Base or Optimism Uniswap V3 swap becomes usable
in the EVM cache and AMM simulator through Flashblocks versus an ordinary
canonical WebSocket subscription. It is a focused live acceptance test, not a
synthetic microbenchmark.

## Method

- Provider: one paid HTTP and WebSocket endpoint from the same provider
  generation.
- Pool: the most active native Uniswap V3 USDC/WETH pool across the 0.01%,
  0.05%, 0.30%, and 1.00% fee tiers, ranked by recent canonical Swap logs. This
  avoids treating an inactive fixed pool as a provider failure.
- Window: 100 canonical swaps or five minutes, whichever comes first.
- Flashblock path: Base uses native `newFlashblocks` plus pool-filtered
  `pendingLogs`; Optimism samples the generation-pinned `pending` block, exact
  parent, filtered logs, and bounded exact transaction receipts every 250 ms by
  default.
- Vanilla path: pool-filtered canonical `logs` over WebSocket.
- Pairing key: `(transaction_hash, log_index)`.
- State isolation: two independently cold-started `EvmCache` and
  `AmmSyncEngine` instances. The vanilla cache never consumes speculative
  state. The Flashblock cache consumes canonical logs after measurement so its
  disposable overlay converges normally.
- Cache-ready time: notification receipt through `AmmSyncEngine::ingest_batch`.
- AMM-ready time: cache-ready time plus RPC-disconnected replay of the warmed
  representative quotes in both directions at small and large sizes.
- Warmup gate: the second complete representative-quote warmup must report zero
  provider reads before subscriptions are attached.
- Endpoint gate: chain identity plus Base's two subscription lanes or
  Optimism's pending-state method probes must complete within 15 seconds. The
  measured window then proves advancing pending state and active-pool log
  delivery.
- Build: optimized release profile unless a result explicitly says otherwise.

Run it with:

```bash
FLASHBLOCKS_BENCH_CHAIN=base \
BASE_HTTP_URL=<paid-base-http-endpoint> \
BASE_WS_URL=<paid-base-websocket-endpoint> \
FLASHBLOCKS_BENCH_SECONDS=300 \
FLASHBLOCKS_BENCH_MAX_SWAPS=100 \
cargo run --release --example flashblocks_latency_live --features uniswap-v3
```

The harness spaces the independent setup phases by 1.1 seconds when
`FLASHBLOCKS_PROVIDER_ID` contains `quicknode`, keeping the two cache
bootstraps and two representative warmups below QuickNode's 50 requests/second
plan limit. Override this with `FLASHBLOCKS_SETUP_PAUSE_MS` when validating a
different provider quota; measured event handling begins only after setup, so
the pause does not affect latency samples.

For Optimism, use the matching endpoint variables:

```bash
FLASHBLOCKS_BENCH_CHAIN=optimism \
OPTIMISM_HTTP_URL=<paid-optimism-http-endpoint> \
OPTIMISM_WS_URL=<paid-optimism-websocket-endpoint> \
FLASHBLOCKS_BENCH_SECONDS=300 \
FLASHBLOCKS_BENCH_MAX_SWAPS=100 \
cargo run --release --example flashblocks_latency_live --features uniswap-v3
```

The OP adapter's default 250 ms interval and rolling 40-method/second ceiling
bound request use below a 50-request/second provider plan. Applications remain
free to disable preconfirmations or configure a different interval and budget.

## Stable-release harness migration

The September harness starts both caches at a verified block hash and supplies
the complete EVM block environment. Canonical progress is certified by fetching
all logs for this pool at each exact intervening block hash; a subscription
notification alone does not prove log completeness. Empty blocks also advance
the baseline. A speculative view is accepted only after its exact parent is
covered. Reorgs or contradictory proof responses stop the probe.

A bounded collector polls the subscriber independently from canonical proof
RPC work and timestamps each batch when the subscriber yields it. Queue overflow
rejects the run. The two caches share immutable canonical RPC evidence, while
applying it independently. The canonical path advances after its own subscription notice;
it never consumes speculative state. `canonical_reconcile_and_apply` includes
any canonical proof fetch, so it must not be compared with the old cache-only
apply interval. `canonical_proof_rpc` reports these additional requests
separately from native Flashblocks subscription metrics. Canonical read-set
hydration refreshes the dependencies invalidated by a changed block pin; its
account and slot counts are reported separately. Warm quote validation still
runs with provider reads disconnected.

## September 5 revalidation: not qualified

The paid QuickNode Base source did not pass the current native Flashblocks gate.
A 120-second diagnostic produced 19 canonical swaps and 11 paired previews,
below the required 20-swap activity floor and 95% pairing coverage. Subsequent
runs rejected incomplete V3 transition sequences; no latency qualification is
claimed from these attempts.

At Base block `50919224`, the canonical parent slot0 exactly matched the live
cache, but the preview delivered pool log `806` without earlier pool logs `254`
and `447`. The adapter rejected the derived output mismatch and purged the
speculative pool state. At an earlier failure, block `50918833`, independently
replaying both canonical swaps (`58` and `525`) from its verified parent applied
both exactly, with zero purges and passing offline quotes.

A separate direct `pendingLogs` subscription and the crate subscriber on the
same connection also lacked a canonical pool swap at block `50919481` during
the 120-second capture, while both delivered a later preview at `50919493`.
This small diagnostic does not establish a general provider failure rate or
exclude all subscriber issues; it does show that the missing event was not
observed on the direct subscription either. The configured source remains
unqualified. Do not substitute the historical tables below for a passing check
on the current source and crate versions.

Set `FLASHBLOCKS_TRACE_TRANSITIONS=1` for local transition diagnostics. It prints
public log data and cached slot0 values and should remain off for timing runs.

## Historical result

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

## July 29 adapter acceptance

The revised adapter was exercised against the configured paid Alchemy
endpoints after fixing placeholder-hash identity, pending-log buffering,
cumulative sequencing, paired-stream reconnects, complete pending EVM context,
provider pinning, and fail-closed AMM publication.

Base started at block `49,266,792` and reached the 100-canonical-swap stop
condition after 253.936 seconds. It observed 102 Flashblock swaps, 100 canonical
swaps, and 100 exact pairs, with no quote-read retries.

| Base metric | Samples | Mean | p50 | p95 | Max |
| --- | ---: | ---: | ---: | ---: | ---: |
| Flashblock notification lead | 100 | 1,598.348 ms | 1,816.980 ms | 2,238.504 ms | 2,379.267 ms |
| Cache-ready lead | 100 | 1,597.594 ms | 1,816.489 ms | 2,237.564 ms | 2,379.880 ms |
| AMM-quote-ready lead | 100 | 1,309.959 ms | 1,603.105 ms | 1,875.643 ms | 1,993.921 ms |

This run confirms that all-zero provider placeholders no longer alias previews:
pending logs wait until `(block number, transaction membership)` identifies the
matching cumulative preview, while the adapter's noncanonical content
commitment remains unique to the provider generation and cumulative payload.
No identity, cache-publication, or AMM-readiness failure occurred.

An initial, now-superseded native-subscription experiment on the paid Alchemy
Optimism endpoint started at block
`154,862,215`, observed 24 canonical swaps, and observed no matching
`pendingLogs`, so it produced zero paired samples. Both subscriptions were
accepted and there were no parser or quota errors, but this endpoint did not
deliver a usable pool-filtered Flashblocks stream. That result motivated the
generation-pinned pending-state adapter measured below; it is not the current OP
release result.

The configured QuickNode Base and Optimism hosts were unavailable during that
acceptance pass: even a credential-free TLS connection terminated before an
HTTP response. Later paid-provider results below supersede that connectivity
observation.

The local apply and quote timings from the July 29 run used an unoptimized debug
build to maximize assertion coverage and are intentionally omitted as
performance claims. Notification, cache-ready, and AMM-ready lead are still
valid wall-clock comparisons between paths in that same process.

## Alpha.2 crate acceptance

The release-candidate path was rerun in optimized builds after lazy quote
read-set warming and exact provider-generation identity were complete.

The paid QuickNode Base gate used the active WETH/USDC 0.01% Uniswap V3 pool
`0xb4CB800910B228ED3d0834cF79D697127BBB00e5`. It reached 100 canonical swaps
after 241.524 seconds, observed 101 Flashblock swaps, and paired all 100 canonical
swaps. Repeated warmup and both measured paths used zero provider-read retries.

| QuickNode Base metric | Samples | Mean | p95 |
| --- | ---: | ---: | ---: |
| Flashblock notification lead | 100 | 1,456.331 ms | 2,011.315 ms |
| Cache-ready lead | 100 | 1,455.919 ms | — |
| AMM-quote-ready lead | 100 | 1,455.489 ms | 2,017.190 ms |
| Flashblock cache apply | 100 | — | 3.637 ms |
| Flashblock quote | 100 | — | 5.984 ms |
| Canonical quote | 100 | — | 5.184 ms |

### August 4 release-candidate revalidation

The Rust 1.90 release candidate was revalidated against the configured paid
Base endpoints. QuickNode selected the active WETH/USDC 0.05% Uniswap V3 pool
`0xd0b53D9277642d899DF5C87A3966A349A798F224` and ran for 300.003 seconds. It
observed 63 Flashblock swaps and 62 canonical swaps, pairing all 62 canonical
events with no provider-read retries. The native path issued one optional
capability probe and no pending-block, log, receipt, parent, or canonical-head
polling requests, for an observed average of 0.003 RPC methods per second.

| QuickNode Base metric | Samples | Mean | p50 | p95 | Max |
| --- | ---: | ---: | ---: | ---: | ---: |
| Flashblock notification lead | 62 | 1,418.249 ms | 1,520.199 ms | 1,956.590 ms | 2,015.330 ms |
| Cache-ready lead | 62 | 1,417.954 ms | 1,520.613 ms | 1,957.904 ms | 2,015.746 ms |
| AMM-quote-ready lead | 62 | 1,418.042 ms | 1,520.903 ms | 1,962.121 ms | 2,015.781 ms |
| Flashblock cache apply | 63 | 0.951 ms | 0.575 ms | 3.366 ms | 10.061 ms |
| Flashblock quote | 63 | 1.450 ms | 1.249 ms | 2.137 ms | 6.555 ms |
| Canonical cache apply | 62 | 0.581 ms | 0.331 ms | 1.641 ms | 6.626 ms |
| Canonical quote | 62 | 1.537 ms | 1.283 ms | 2.548 ms | 8.414 ms |

The same candidate passed Base HTTP AMM parity checks against Alchemy, and its
ordinary `newHeads` WebSocket subscription was accepted. However, the
configured Alchemy endpoint rejected both `newFlashblocks` and pool-filtered
`pendingLogs` with JSON-RPC error `-32603` before the timed window began. This
is a current provider/application qualification failure, not a parser or
correlation failure. Alchemy cannot be counted as a qualified Flashblocks
fallback for this release candidate until both native subscription lanes pass
again.

### Optimism pending-state acceptance

The paid Alchemy OP experiment used the active WETH/USDC 0.05% Uniswap V3 pool
`0x1fb3cf6e48F1E7B10213E7b6d87D4c073C7Fdb7b`, starting from block
`154,879,418`. In 300.003 seconds it observed 29 Flashblock swaps, 29 canonical
swaps, and 29 exact pairs. There were no provider failures, raced samples,
unavailable receipts, or quote retries. The sampler issued 6,943 RPC methods
(23.143/second): 923 pending blocks, 923 exact parents, 922 filtered-log calls,
and 4,173 exact receipts, of which 4,172 were available.

| Alchemy OP metric | Samples | Mean | p50 | p95 | Max |
| --- | ---: | ---: | ---: | ---: | ---: |
| Positive Flashblock notification lead | 19 | 64.407 ms | 2.738 ms | 572.513 ms | 572.513 ms |
| Positive cache-ready lead | 19 | 64.078 ms | 2.441 ms | 572.422 ms | 572.422 ms |
| Positive AMM-quote-ready lead | 19 | 63.038 ms | 1.445 ms | 572.326 ms | 572.326 ms |
| Flashblock cache apply | 29 | 0.629 ms | 0.395 ms | 2.319 ms | 4.117 ms |
| Flashblock quote | 29 | 2.059 ms | 1.269 ms | 7.151 ms | 7.151 ms |

The lead table contains only positive durations because the harness
uses a checked subtraction. Ten of the 29 OP Flashblock notices arrived at or
after the canonical WebSocket notice. The run therefore proved complete
correlation and bounded request behavior, but not a uniform OP latency advantage.
Applications should weigh that recurring request cost and inconsistent lead
against their provider plan and latency requirements.

The same revision received a 60-second paid QuickNode OP smoke test
on that pool.
It paired only 1 of 5 canonical swaps, returned 455 null results across 587 exact
receipt requests, and recorded 12 failed sampler requests. One paired preview
did reach an offline-ready quote with no retry, so the wire format and cache path
were compatible, but this endpoint failed liveness/correlation qualification.
These results leave QuickNode OP ineligible for pending-state qualification;
Alchemy OP qualified in the five-minute acceptance above. Both providers remain
independently eligible for ordinary canonical OP traffic.

## Alpha status and production-rollout gate

The crate implementation now learns representative account/code/storage/block
read sets, hydrates them at the exact canonical pin, replays affected quotes
against an RPC-disconnected preview, requires the immediate offline warm-up
quote to equal its canonical result, bounds unexpected dependency growth, and
invalidates manifests on runtime-code changes. Same-payload cumulative previews
retain generation-local fills; every invalidation boundary restores canonical
state. An unexpected speculative cold slot rejects publication and is hydrated
by the hash-pinned worker only after the next canonical point. Proof-shape,
provider, and bytecode-residency failures remain typed in the hydration report.

Each release candidate must still run this benchmark against the selected paid
Base and Optimism endpoints intended for deployment. A timed window must observe
at least 20 canonical swaps and correlate at least 95%; otherwise rerun on the
selected active pool/provider.
Repeated warmup must show zero provider reads, and Flashblock
notification-to-offline-quote p95 must remain at or below 25 ms. Provider
identity errors or semantic inconsistencies fail the run.

## Provider head visibility

An acceptance attempt immediately before the historical measured run received a
canonical WebSocket log before the same QuickNode HTTP endpoint could query that
block by hash. The old lazy quote path returned `block not found`; the hash
became queryable shortly afterward. The revised benchmark does not retry quote
reads: representative quotes must already replay offline, so any missing state
fails the acceptance gate.

Stable provider identity and preferred routing remain useful, but they cannot
alone guarantee that separate HTTP and WebSocket sessions have identical head
visibility at notification time. Consumers must either avoid lazy reads on the
hot path, use a provider/session surface that exposes the announced state, or
handle this bounded lag explicitly.

## Scope

The alpha.2 crate scope includes Base native subscriptions and the opt-in
Optimism pending-state sampler through the speculative cache and V3 quote
pipeline. The August 4 QuickNode Base gate paired 62/62 canonical events with
zero hot-path provider-read retries; an earlier Alchemy Base gate paired
100/100, but the currently configured Alchemy endpoint no longer accepts either
native Flashblocks subscription lane. Alchemy OP completed the five-minute
acceptance with 29/29 pairs under the configured request budget; QuickNode OP
did not qualify on the measured endpoint. Unit and integration tests cover
reconnect recovery; a paid forced reconnect exercise remains a separate rollout
gate.
