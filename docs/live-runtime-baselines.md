# Live Runtime Baseline Contract

Status: Stage 10 implementation acceptance complete; Base live parity and
dependency-ordered registry publication remain external release gates.
Date: 2026-07-10

## Purpose

This document defines how the progressive AMM runtime work is measured. It
separates stable offline regression benches from provider-sensitive live
benchmarks and names metrics before the new runtime exists, so later stages
cannot improve a headline number by quietly changing what it means.

The baseline spans both crates:

- `evm-amm-state`: discovery, bootstrap, registration, live ingest, and dynamic
  lifecycle;
- `evm-amm-search`: graph construction, liquidity priming, first quote, eventual
  best, full search, and incremental refresh.

## Measurement Rules

- Use release builds for live network and route-search measurements.
- Use the paid `E2E_RPC_URL` endpoint for published network claims.
- Preserve gzip on HTTP benchmark transports.
- Pin a block where the benchmark supports it.
- Record hardware, commit, feature set, pool/token counts, block, and effective
  concurrency/chunk configuration.
- Report median, p95, and maximum for repeated latency measurements.
- Redact RPC URLs in output and documentation.
- Keep provider time separate from local scheduling/commit time.
- Never compare a warm run with a cold run without labeling the difference.
- A missing metric is reported as unavailable, not estimated.

## Offline Baselines

### Dynamic lifecycle

```text
cargo bench --bench runtime_lifecycle
```

`benches/runtime_lifecycle.rs` measures the current public
`AmmSyncEngine::register_pools` and `unregister_pools` behavior at several
existing-registry sizes.

Metric names:

```text
runtime_lifecycle/register_one/<existing_pools>
runtime_lifecycle/unregister_one/<existing_pools>
```

The stable names captured Stage 0/2's full-runtime reconstruction and now
measure Stage 3's incremental compatibility wrappers directly.

### Reactive apply

```text
cargo bench --bench reactive_apply
```

This is the existing no-network hot-path regression suite for V2, V3, and
Balancer event application. Progressive loading work must not regress it.

### Pool-scoped routing

```text
cargo bench --bench pool_routing
```

`benches/pool_routing.rs` compares compatibility and generation-scoped
per-pool handlers at 16, 64, 320, and 4,096 pools. The matching pool is placed
at the end of the relevant interest/handler order. Compatibility stays on the
fallback path; Stage 3 pool handlers declare exhaustive exact route indexes.

### Cache actor

```text
cargo bench --features live-runtime --bench live_runtime_actor
```

On the 2026-07-10 M1 Pro offline fixture, after provider/cache supply:

| Metric | Criterion interval | Gate |
| --- | ---: | ---: |
| `live_runtime/handle_creation_after_cache_supply` | `4.507us..4.548us` | `<10ms` |
| `live_runtime/control_command_enqueue` | `96.5ns..97.2ns`; debug-test p99 `667ns` | `<1ms p99` |
| `live_runtime/cold_start_queue_return` | `4.224us..4.402us` | `<10ms` |
| `live_runtime/live_batch_round_trip_during_blocked_bootstrap` | `3.113us..3.186us`; debug-test p99 `75.25us` | `<50ms p99` |

The enqueue benchmark times bounded non-blocking submission only. It awaits
the accepted empty lifecycle command outside the measured interval so the
queue drains between samples.

The Stage 5 queue-return benchmark includes actor admission, exact generation
reservation, work/lifecycle/status publication, and bounded worker submission.
Provider hydration is deliberately excluded and the runtime is torn down after
the measured return. These intervals are the 2026-07-10 rerun after exact-hash
cache pinning and progressive scheduling landed.

The blocked-bootstrap metric holds a cold-start `eth_call` behind a deterministic
transport gate while timing only empty canonical-batch submission through actor
commit. Header construction and provider time are outside the measured region.
The matching public regression warms up 32 commits, samples 1,000 sequential
round-trips, and asserts the nearest-rank p99 remains below 50ms.

### Graph lifecycle

From the sibling `evm-amm-search` crate:

```text
cargo bench --bench graph_lifecycle
```

`benches/graph_lifecycle.rs` records the existing full build, in-place full
rebuild, add-one-via-full-rebuild, and already-incremental remove-one paths at
16, 64, and 320 pools. The workload names remain stable through Stage 7:

```text
graph_lifecycle/full_build/<existing_pools>
graph_lifecycle/rebuild_existing/<existing_pools>
graph_lifecycle/add_one_via_full_rebuild/<existing_pools>
graph_lifecycle/remove_one/<existing_pools>
```

## Live `evm-amm-state` Baselines

Load the local env without printing it, then run:

```text
set -a; source .env; set +a
FACTORY_DISCOVERY_SECONDS=0 \
FACTORY_DISCOVERY_MAX_EVENTS=0 \
cargo run --release --example factory_discovery_live
```

The example reports:

- cache/provider setup;
- factory discovery;
- full `cold_start_many` barrier;
- `AmmSyncEngine` registration;
- WebSocket subscription;
- first log and first applied event when an event window is enabled;
- ingest total, average, and maximum.

For an event-bearing sample, use a nonzero window:

```text
FACTORY_DISCOVERY_SECONDS=60 \
FACTORY_DISCOVERY_MAX_EVENTS=12 \
cargo run --release --example factory_discovery_live
```

Additional existing measurements:

```text
E2E_RPC_URL=<paid-url> SYNC_BENCH_ITERS=7 \
cargo run --release --example sync_latency

E2E_RPC_URL=<paid-url> TRACE_RESYNC_ITERS=7 \
cargo run --release --example trace_resync_latency

E2E_RPC_URL=<paid-url> \
cargo run --release --example curve_cold_start_phases
```

## Live `evm-amm-search` Baselines

From the sibling `evm-amm-search` crate:

```text
set -a; source .env; set +a
cargo run --release --example docs_route_benchmark
```

This reports focused cold start, optional liquidity refresh, first quote,
eventual best, exhaustive completion, and incremental route refresh.

The larger workload is:

```text
PROD_BASKET_PRIME_CACHE=1 \
PROD_BASKET_SEARCH_MODE=heuristic \
PROD_BASKET_LIQUIDITY_PRIMING=1 \
PROD_BASKET_INCREMENTAL_COMPARE=1 \
cargo run --release --example production_basket_search
```

The current published 320-pool workload records approximately:

- factory discovery: `360.9ms` to `490.3ms`;
- `cold_start_many`: `1.326s` to `1.607s`;
- graph build: `1.31ms` to `1.74ms`;
- broad liquidity refresh: about `11.2s`;
- warm measured search: `35.6ms` to `38.6ms` for the documented batch.

Treat these as the existing published snapshot, not a substitute for the fresh
Stage 0 capture. Provider behavior and the in-progress cold-start chunking work
can change the current values.

## Metric Definitions

### Metrics measurable now

| Metric | Start | Stop |
| --- | --- | --- |
| `provider_setup` | before provider/cache construction | usable pinned cache |
| `factory_discovery` | before `PoolDiscovery::find*` | candidate registrations returned |
| `full_cold_start` | before `cold_start_many` | complete outcome vector returned |
| `engine_registration` | before `register_pools` | rebuilt engine ready |
| `ws_subscription` | before WS connect/subscribe | stream established |
| `first_log` | event loop starts | first subscribed log delivered |
| `first_applied_event` | event loop starts | first AMM state effect/resync applies |
| `reactive_ingest` | before `ingest_batch` | coherent report returned |
| `full_graph_build` | before `AmmGraph::from_registry` | report returned |
| `liquidity_refresh` | before balance refresh | refresh report returned |
| `first_quote` | search starts | first viable streamed quote |
| `eventual_best` | search starts | final winning quote first appears |
| `search_complete` | search starts | configured completion reached |
| `incremental_refresh` | before `refresh_affected` | update report returned |
| `cold_start_queue_return` | before `queue_cold_start` | generation-owned work returned |
| `time_to_first_searchable_pool` | batch accepted | first independently published pool snapshot |
| `time_to_n_searchable_pools` | batch accepted | snapshot containing the requested ready count |
| `cold_start_fetch_time` | worker quantum begins provider work | prepared artifact ready to commit |
| `cold_start_commit_time` | actor accepts prepared artifact | coherent snapshot/change publication |

### Progressive metrics unlocked by later stages

| Metric | Unlocked by | Current measurement surface |
| --- | --- | --- |
| `time_to_first_connected_route` | Stages 5 and 7 | headless progressive TUI benchmark |
| `graph_delta` | Stage 7 | `graph_lifecycle/add_one_incremental` |
| `route_recompute_from_amm_change` | Stage 8 | `live_route_scheduler/*` |

Each surface preserves the original milestone definition in typed runtime state
or structured benchmark output. Stage 10 captures both the offline scheduler
cost and provider-backed progressive bootstrap separately.

## Frozen Performance Gates

| Metric | Initial gate |
| --- | ---: |
| Runtime handle creation after provider/cache supply | `<10ms` offline |
| Control command enqueue | `<1ms p99` offline |
| Control return after queued discovery | `<10ms` offline |
| Live batch queue delay during bootstrap | `<50ms p99` fixture time, provider excluded |
| Time to first connected route | `<50%` of full-bootstrap median |
| Full bootstrap throughput | no regression greater than `10%` |
| One two-token graph delta | `<10%` of full graph build |
| Existing warm search | no regression greater than `10%` |
| Runtime or graph rebuild during add/remove | `0` |
| Stale/old-generation commits | `0` |

These are provisional release gates. Stage-specific evidence may tighten them,
but weakening one requires a documented design tradeoff.

## Capture Template

Every published run should record:

```text
date:
crate commits:
dirty files (names only):
hardware:
rustc:
profile:
RPC provider class (redacted):
gzip:
chain/block:
tokens/pools/edges/queries:
cold-start chunk/concurrency:
benchmark command (without secret values):
median/p95/max:
failures/retries:
notes:
```

## Stage Comparison Policy

Each completed stage updates this document with:

1. the new metric availability;
2. the exact command used;
3. before/after results on the same workload;
4. any changed provider or hardware conditions;
5. correctness failures, retries, or dropped work;
6. whether every applicable frozen gate passed.

## Stage 0 Capture — 2026-07-10

Environment:

```text
hardware: Apple M1 Pro, 10 cores, 16 GiB
rustc: 1.96.0-nightly (900485642 2026-04-08)
evm-amm-state commit: 19af7c555cd2
evm-amm-search commit: 6f1f90f8f7f5
profile: Criterion bench / optimized; live release
RPC provider class: paid Alchemy mainnet (URL redacted)
gzip: transport default; not independently verified by this example
live blocks: latest 25501824-25501827, pinned 25501816-25501819
live workload: USDC/WETH, 5 pools, 30,047 verified slots on the first run
cold-start diff SHA-256: 1bb94053e40ea581f496ffbd7b5d4c0c1af7c180635ef2da7933867fa4a457ce
cold-start overrides: EVM_AMM_COLD_START_CHUNK unset; EVM_AMM_COLD_START_CONCURRENCY unset
cold-start dirty defaults: min derived chunk 32; concurrency ceiling 8
```

Dirty files at capture time were the Stage 0 files plus pre-existing user work
in `src/adapters/cold_start.rs` and the sibling
`src/bin/amm_route_tui.rs`. Those pre-existing files were not edited by this
stage.

### Offline state lifecycle

Command: `cargo bench --bench runtime_lifecycle`. Criterion used its default
100-sample collection. Values below are the estimate and 95% confidence
interval reported by the final serial run.

| Operation | Existing pools | Estimate | 95% confidence interval |
| --- | ---: | ---: | ---: |
| register one | 0 | `1.068us` | `1.046-1.092us` |
| register one | 16 | `10.962us` | `10.356-11.865us` |
| register one | 64 | `36.356us` | `35.133-37.971us` |
| register one | 320 | `164.38us` | `157.32-173.54us` |
| unregister one | 16 | `9.523us` | `9.248-9.828us` |
| unregister one | 64 | `35.751us` | `33.704-38.800us` |
| unregister one | 320 | `162.46us` | `159.30-167.24us` |

Both lifecycle operations scale with registry size because the current public
methods reconstruct runtime-shaped state. Stage 3 should make their cost
approximately independent of unrelated pool count.

### Offline graph lifecycle

Command: `cargo bench --bench graph_lifecycle`. This was run serially after the
state suite; outputs are dropped outside the timed interval.

| Operation | Existing pools | Estimate | 95% confidence interval |
| --- | ---: | ---: | ---: |
| full build | 16 | `32.268us` | `31.330-33.224us` |
| full build | 64 | `170.35us` | `165.72-176.67us` |
| full build | 320 | `1.1043ms` | `1.0941-1.1164ms` |
| rebuild existing | 16 | `33.033us` | `31.472-35.002us` |
| rebuild existing | 64 | `171.60us` | `165.13-182.26us` |
| rebuild existing | 320 | `1.0905ms` | `1.0821-1.1012ms` |
| add one via full rebuild | 16 | `36.516us` | `36.186-36.942us` |
| add one via full rebuild | 64 | `171.76us` | `169.92-174.70us` |
| add one via full rebuild | 320 | `1.1414ms` | `1.1292-1.1588ms` |
| remove one | 16 | `185.99ns` | `172.14-204.82ns` |
| remove one | 64 | `244.38ns` | `220.73-269.15ns` |
| remove one | 320 | `305.92ns` | `279.69-331.51ns` |

Removal already uses a pool-to-edge index. Addition has no public graph delta
and therefore costs essentially a full build; Stage 7 preserves the removal
shape and makes addition incremental.

### Paid-RPC focused cold start

Command:

```text
FACTORY_DISCOVERY_SECONDS=0 \
FACTORY_DISCOVERY_MAX_EVENTS=0 \
cargo run --release --example factory_discovery_live
```

Five successful runs were captured. With only five observations, nearest-rank
p95 is the maximum; the small sample is an honest startup snapshot rather than
a distribution claim.

| Phase | Median | p95 / max |
| --- | ---: | ---: |
| provider/cache setup | `284.70ms` | `513.22ms` |
| factory discovery | `113.36ms` | `118.98ms` |
| full cold start | `965.33ms` | `3,601.27ms` |
| engine registration | `0.11ms` | `0.61ms` |
| WebSocket subscription | `309.05ms` | `522.20ms` |
| end-to-end | `1,732.13ms` | `4,713.28ms` |

All runs found and readied five pools. The first run's cold-start spike is kept
in the tail result. The zero-second event window intentionally produced no log
or reactive-ingest latency sample.

### Paid-RPC focused search

Command from `evm-amm-search`:

```text
DOCS_BENCH_PERSIST_CACHE=0 \
cargo run --release --example docs_route_benchmark
```

This was a cold local-cache run at pinned Ethereum block `25501909` with the
same cold-start diff recorded above: 30 runs, 10 workers, gzip transport, 117
ready pools, 8 tokens, and 234 directed edges. Setup results were:

| Phase | Result |
| --- | ---: |
| cache build | `82.34ms` |
| factory discovery | `65.68ms` |
| full cold start | `1.1656s` |
| graph build | `3.08ms` |
| liquidity refresh | `1.9843s` |
| cold start plus liquidity | `3.1498s` |

Recommended balanced search with a fresh liquidity index and finalist
simulation enabled:

| Route | First quote p50 / p95 / max | Final winner p50 / p95 / max | Exhaustive p50 / p95 / max |
| --- | ---: | ---: | ---: |
| `10 WETH -> USDC` | `0.261 / 0.311 / 0.316ms` | `0.261 / 0.311 / 0.316ms` | `94.47 / 99.37 / 99.58ms` |
| `100 LINK -> AAVE` | `0.392 / 0.420 / 0.440ms` | `1.616 / 1.675 / 1.735ms` | `204.59 / 227.19 / 258.44ms` |
| `1000 DAI -> UNI` | `0.339 / 0.398 / 0.425ms` | `7.414 / 7.775 / 8.463ms` | `447.61 / 554.58 / 717.89ms` |

All 90 route runs completed, and no heuristic/exhaustive winner divergence was
observed. Against the checked-in July 9 focused baseline, final-winner p50 moved
from `0.284ms` to `0.261ms`, `1.59ms` to `1.616ms`, and `7.61ms` to `7.414ms`.
The largest regression is about `1.6%`, so the frozen `<=10%` warm-search
regression gate passes.

The current incremental session path also completed 90/90 refreshes with zero
divergence from full heuristic recompute:

| Metric | p50 | p95 | max |
| --- | ---: | ---: | ---: |
| session start | `262.77ms` | `471.91ms` | `753.98ms` |
| affected refresh | `4.174ms` | `6.155ms` | `10.008ms` |
| full recompute | `7.303ms` | `10.978ms` | `30.947ms` |

## Stage 2 Capture — 2026-07-10

The implementation and Stage 0 capture used the same M1 Pro host. The routing
suite was run with 30 samples, two seconds of measurement, and one second of
warm-up; lifecycle used Criterion's default 100-sample collection.

### Pool-scoped routing

Command:

```text
cargo bench --bench pool_routing -- \
  --noplot --sample-size 30 --measurement-time 2 --warm-up-time 1
```

| Handler model | Pools | Estimate interval |
| --- | ---: | ---: |
| compatibility worst case | 16 | `0.242-0.256us` |
| compatibility worst case | 64 | `0.671-0.688us` |
| compatibility worst case | 320 | `3.07-3.20us` |
| pool-scoped worst case | 16 | `0.271-0.278us` |
| pool-scoped worst case | 64 | `0.858-0.914us` |
| pool-scoped worst case | 320 | `3.99-4.17us` |

The pool-scoped ownership boundary costs less than `1us` at 320 pools in this
fixture. Both models still scale linearly because upstream
`ReactiveRegistry::route_log` scans every registered handler and interest; an
indexed router is now a Stage 3 prerequisite rather than an undocumented
scaling limitation.

### Rebuild-shaped compatibility lifecycle

The Stage 2 per-pool handler and ownership construction makes the still-
rebuilding compatibility methods more expensive:

| Operation | Existing pools | Stage 0 estimate | Stage 2 estimate |
| --- | ---: | ---: | ---: |
| register one | 0 | `1.068us` | `3.553us` |
| register one | 16 | `10.962us` | `47.933us` |
| register one | 64 | `36.356us` | `204.69us` |
| register one | 320 | `164.38us` | `1.1888ms` |
| unregister one | 16 | `9.523us` | `47.042us` |
| unregister one | 64 | `35.751us` | `191.79us` |
| unregister one | 320 | `162.46us` | `1.1447ms` |

This is not accepted as the final lifecycle shape. Stage 3 retains the stable
benchmark names and must make add/remove incremental with zero runtime rebuild.
The largest absolute Stage 2 operation remains below `1.3ms`; provider-bound
cold start and search behavior were unchanged by this local routing/index
stage, so no new paid-RPC claim is made here.

## Stage 3 Capture — 2026-07-10

The same M1 Pro host ran a shortened 10-sample validation capture after binding
to local `evm-fork-cache 0.2.2`. Commands:

```text
cargo bench --bench runtime_lifecycle -- \
  --warm-up-time 0.05 --measurement-time 0.1 --sample-size 10
cargo bench --bench pool_routing -- \
  --warm-up-time 0.05 --measurement-time 0.1 --sample-size 10
```

Short collections are regression evidence, not stable distribution claims.

### Incremental lifecycle

| Operation | Existing pools | Estimate | Stage 2 estimate |
| --- | ---: | ---: | ---: |
| register one | 0 | `3.32us` | `3.55us` |
| register one | 16 | `3.58us` | `47.93us` |
| register one | 64 | `4.54us` | `204.69us` |
| register one | 320 | `6.82us` | `1.1888ms` |
| unregister one | 16 | `2.50us` | `47.04us` |
| unregister one | 64 | `5.98us` | `191.79us` |
| unregister one | 320 | `7.62us` | `1.1447ms` |

At 320 pools, register improved about `174x` and unregister about `150x`.
Growth is now ordered-index/tree cost rather than full runtime reconstruction;
the runtime-rebuild count during ordinary add/remove is zero.

### Indexed pool routing

| Handler model | Pools | Estimate |
| --- | ---: | ---: |
| compatibility fallback | 16 | `0.279us` |
| compatibility fallback | 64 | `0.736us` |
| compatibility fallback | 320 | `3.09us` |
| compatibility fallback | 4,096 | `39.42us` |
| indexed pool scoped | 16 | `0.215us` |
| indexed pool scoped | 64 | `0.221us` |
| indexed pool scoped | 320 | `0.247us` |
| indexed pool scoped | 4,096 | `0.250us` |

Pool-scoped routing is effectively independent of unrelated handler count.
The upstream companion benchmark additionally measured indexed misses at about
`27ns` through 4,096 handlers and remove/re-register churn below `0.8us` for
fallback, distinct-index, and shared-index populations.

Stage 3 therefore passes the applicable frozen gates: zero runtime rebuilds,
zero stale-generation reuse in the test matrix, and no routing scan across
unrelated indexed pool handlers. Provider-bound startup metrics are unchanged;
background cache ownership and progressive cold start begin in Stages 4 and 5.

## Stage 4 Capture — 2026-07-10

The feature-gated cache actor now has a dedicated offline creation benchmark:

```text
cargo bench --features live-runtime --bench live_runtime_actor -- \
  --sample-size 10 --measurement-time 1 --warm-up-time 1
```

On the same M1 Pro host, handle creation after the provider-backed cache and
sealed full-header baseline were already supplied measured `2.90-3.04us`.
This excludes provider/cache construction by design and is comfortably below
the frozen `<10ms` gate. The benchmark shuts each actor down outside the timed
region and runs entirely against Alloy's mock transport.

Stage 4 canonical/control responsiveness is additionally enforced by public
behavior tests: canonical delivery receives priority, while one control command
must run after at most 16 continuously-ready canonical batches. Both channels
are bounded; nonblocking submission returns typed `Full`/`Closed` outcomes.

## Stage 10 Offline Capture — 2026-07-11

This release-gate capture used an Apple M1 Pro on macOS 26.5.1 with
`rustc 1.96.0-nightly (900485642 2026-04-08)`. The worktrees contained the
uncommitted Stage 1-10 implementation. Criterion used ten samples, a one-second
warm-up, and a one-second measurement window:

```text
cargo bench --bench runtime_lifecycle -- --sample-size 10 --measurement-time 1 --warm-up-time 1
cargo bench --bench pool_routing -- --sample-size 10 --measurement-time 1 --warm-up-time 1
cargo bench --all-features --bench live_runtime_actor -- --sample-size 10 --measurement-time 1 --warm-up-time 1
cargo bench --bench graph_lifecycle -- --sample-size 10 --measurement-time 1 --warm-up-time 1
cargo bench --all-features --bench live_search_runtime -- --sample-size 10 --measurement-time 1 --warm-up-time 1
```

### Runtime responsiveness

| Metric | Median estimate | Range |
| --- | ---: | ---: |
| handle creation after cache supply | `4.218us` | `4.196-4.255us` |
| control enqueue | `90.13ns` | `89.62-90.76ns` |
| cold-start queue return | `3.559us` | `3.535-3.579us` |
| live batch round-trip during blocked bootstrap | `3.127us` | `3.113-3.186us` |

All four clear their `<10ms`, `<1ms`, `<10ms`, and `<50ms p99` gates by several
orders of magnitude. The blocked-bootstrap acceptance test independently
measured `75.25us` p99 over 1,000 debug-profile actor round-trips while the
provider request remained stalled. Register-one measured `4.158us`, `4.995us`, `5.755us`, and `6.901us`
at 0, 16, 64, and 320 existing pools; unregister-one measured `4.685us`,
`5.183us`, and `7.389us` at 16, 64, and 320 pools.

### Indexed routing and incremental graph

Pool-scoped handler dispatch remained nearly flat from `206.8ns` at 16 pools to
`248.3ns` at 4,096. The compatibility fallback grew from `268.6ns` to
`37.95us`, which is retained only for adapters that cannot declare an exact
route.

| Pools | Full graph build | Direct add delta | Delta / full build |
| ---: | ---: | ---: | ---: |
| 16 | `36.02us` | `678.2ns` | `1.88%` |
| 64 | `178.0us` | `759.3ns` | `0.43%` |
| 320 | `1.123ms` | `498.9ns` | `0.04%` |

The direct graph delta clears the `<10%` gate at every size. A complete
production topology publication, which also constructs immutable state and
liquidity views, measured `4.518us`, `20.74us`, and `79.28us`; that path remains
linear because it clones the current graph/liquidity sidecars before mutation.

### Search views, reachability, and scheduling

Live search view creation is population-independent at `23.9-24.0ns` for 16,
64, and 320 pools because it retains immutable registry/revision Arcs and
resolves quote provenance only for requested pools. Exact dead-end reachability
measured `949ns`, `4.73us`, and `258us`; the in-place DFS removes per-branch set
clones, while dense cyclic topologies still expose combinatorial traversal.

| Subscriptions | One commit to all ready | Eight replacements to latest ready |
| ---: | ---: | ---: |
| 1 | `36.50us` | `124.6us` |
| 8 | `123.2us` | `1.167ms` |
| 32 | `293.8us` | `4.553ms` |

The scheduler rejects cancelled queued jobs before constructing search state,
coalesces each subscription to its newest epoch, and publishes only results
whose runtime/state/graph/request/job fences still match. Fanout is currently
linear because every AMM commit invalidates every subscription; dependency-
indexed invalidation is the next scale optimization rather than a correctness
or responsiveness release blocker at the measured populations.

## Stage 10 Paid-RPC Capture — 2026-07-11

The mainnet release runner used a paid archive HTTP/WebSocket provider whose
endpoint is redacted and whose account was limited to 50 requests/second. Tests
were serialized to respect that limit. At current head it passed:

- six pinned swap-parity cases across Balancer V2, Curve StableSwap,
  Curve CryptoSwap, Curve Tricrypto-NG, Uniswap V2, and Uniswap V3;
- all three V3 full/partial-sync checks (1,316 ticks, 723 observations, 6,686
  materialized slots for the full one-call sync; 677 ticks in the active
  partial window);
- both V3 mint/burn event-sourcing checks, both Balancer cash-delta checks, and
  all three pinned mainnet Pancake discovery checks;
- a final 45-second broad V2 WebSocket health soak that observed 76 `Sync`
  events.

The Curve WebSocket gate keeps market activity distinct from correctness. A
180-second diagnostic observed one ecosystem-wide liquidity event but no event
from the three registered pools; this is reported as inactive-pool evidence,
not an adapter failure. The subscription remained open, and a subsequent
post-soak check matched `simulate_swap` exactly to `eth_call` for StableSwap,
CryptoSwap, and CryptoSwapNG. Manual activity soaks can opt into a required
matching event with `E2E_REQUIRE_CURVE_ACTIVITY=1`.

The progressive TUI benchmark then ran the complete provider-backed
`AmmRuntime` -> cold worker -> canonical subscriber -> incremental graph ->
`LiveRouteRuntime` pipeline at mainnet block `25,510,819` with a persisted cache:

| Milestone | Elapsed |
| --- | ---: |
| cache/runtime/subscriber/route handles usable | `2.791s` |
| first usable streamed route | `4.638s` |
| all background AMM work idle | `184.549s` |

The first route was published at state version 3 with one ready pool while 44+
pool/discovery jobs were still progressing. It arrived at `2.51%` of full-idle
time, clearing the `<50%` progressive-loading gate by about `20x`; handle
availability was `1.51%` of full-idle time. No endpoint value is recorded.

This live run also exposed and drove a fail-safe fix before the successful
capture: `EvmCacheBuilder` retained NUMBER/BASEFEE/COINBASE/PREVRANDAO/GASLIMIT
from the pinned header but discarded TIMESTAMP. Strict runtime startup rejected
the incomplete baseline. The cache now retains the fetched timestamp, while
missing/failed header fetches remain unset and continue to fail strict startup.
Base-chain adapter parity was not claimed because no `E2E_BASE_RPC_URL` was
available; it remains a separate chain-specific release gate.

### Paired full-bootstrap throughput

To separate provider drift from code changes, five current/frozen pairs ran
back-to-back against the same paid Alchemy endpoint and the same five-pool
USDC/WETH workload. The frozen side used state commit
`19af7c555cd2f231c29a38ed0bdc6a7965303bdc`; the current side used the dirty
Stage 1-10 worktree based on that commit, local `evm-fork-cache 0.2.2`, and no
`EVM_AMM_COLD_START_CHUNK` or `EVM_AMM_COLD_START_CONCURRENCY` overrides.
Blocks advanced from `25,510,943` to `25,510,961`; every run readied five pools
and 30,055 verified slots with zero failures or retries.

| Metric | Current median | Current p95/max | Frozen median | Frozen p95/max | Change |
| --- | ---: | ---: | ---: | ---: | ---: |
| full cold start | `2.128s` | `2.515s` | `2.058s` | `2.530s` | `+3.4%` |
| end-to-end bootstrap | `4.491s` | `5.738s` | `4.150s` | `4.665s` | `+8.2%` |

The paired end-to-end median clears the frozen `<=10%` regression gate. A
non-paired run against a different 50-requests/second provider was substantially
slower across every network phase and is intentionally excluded from the code
comparison.

### Warm-search regression

The final provider-backed search shape ran 30 samples per route at block
`25,510,998`, with 117 ready pools, 234 directed graph edges, ten workers, and
cache persistence disabled. The liquidity-ranked, simulation-winner variant is
the like-for-like Stage 0 winner comparison:

| Route | Stage 0 p50 time-to-best | Stage 10 p50 | Change |
| --- | ---: | ---: | ---: |
| 10 WETH -> USDC | `0.261ms` | `0.278ms` | `+6.6%` |
| 100 LINK -> AAVE | `1.616ms` | `1.687ms` | `+4.4%` |
| 1,000 DAI -> UNI | `7.414ms` | `7.888ms` | `+6.4%` |

All 90 route runs completed without error or heuristic divergence. Each median
clears the frozen `<=10%` warm-search regression gate.

### Repeated progressive first-route capture

Five additional paid-provider starts used a 120-second first-route timeout and
stopped immediately after the first usable route so the full-idle reference
remained the complete `184.549s` capture above. Runtime handles were usable in
all five samples with a `3.546s` median and `5.434s` p95/maximum. Four samples
streamed a route successfully: median `8.341s`, p95/maximum `10.341s`. Every
successful sample was far below half of the full-idle reference (`92.275s`).

One sample timed out without a usable route after its runtime handles became
usable in `2.982s`; no result was discarded or counted as success, and no retry
was substituted into the five-sample distribution. This provider-sensitive
failure is recorded separately from the deterministic offline responsiveness
gate and the successful complete progressive capture.

The anomaly did not reproduce in the final three consecutive live-matrix/TUI
reruns: all three produced a usable route in `7.398s`, `5.298s`, and `12.126s`.
Two of those reruns also waited up to 240 seconds for background idle. Combined
with the original complete capture, the matched three-start cohort is:

| Metric | Samples | Median | p95/maximum |
| --- | --- | ---: | ---: |
| runtime handles usable | `2.791s`, `3.076s`, `3.929s` | `3.076s` | `3.929s` |
| first usable route | `4.638s`, `5.298s`, `12.126s` | `5.298s` | `12.126s` |
| all background work idle | `184.549s`, `>240s`, `>240s` | `>240s` | `>240s` |

The full-bootstrap median is therefore right-censored above 240 seconds, while
the matched first-route median is `5.298s`: less than `2.21%` of the full-idle
median's lower bound and well inside the `<50%` gate. The two idle timeouts are
reported rather than converted into synthetic durations; they reinforce that
interactive availability is decoupled from prolonged provider-backed discovery
and hydration. The isolated first-route timeout remains visible as a transient,
non-reproducing provider-sensitive anomaly rather than being silently retried
out of the earlier five-start distribution.
