# High-performance bootstrap defaults

Status: partially delivered. Bundled multi-pool hydration ships as
`AdapterRegistry::cold_start_many` (+ `supports_one_shot_hydration`); a
discovery-integrated `Bootstrapper` with strategy knobs and a per-phase
timing/request-count report is still future work.

This document records what we learned while wiring factory discovery into a live
Uniswap V2/V3 bootstrap demo, why the high-performance paths were not selected
automatically, and what the crate should expose so users naturally get the fast
route instead of assembling it from lower-level pieces.

## Delivered: `cold_start_many` (fast multi-pool bootstrap default)

`AdapterRegistry::cold_start_many` is now the fast default for bootstrapping
many pools at once, with the conservative per-pool `cold_start` as its fallback:

```rust
let outcomes = registry
    .cold_start_many(&mut ready, &mut cache, provider, ColdStartPolicy::Eager)
    .await?;
engine.register_pools(ready)?;
```

Given a slice of `PoolRegistration`s (typically straight from
`PoolDiscovery::find`, which is already a batched read), it:

1. **Classifies** each pool. A pool is *fast* when its adapter is registered and
   `supports_one_shot_hydration(&pool)` holds — i.e. a flat read-set
   (`storage_sync`) or a V3-family layout (`v3_sync` full sync) resolves.
   Everything else is *fallback*.
2. **Batches code-seed verification.** Every fast pool's `AdapterCodeSeed`s are
   seeded together (same skip rules as the single-pool path — `Verified`
   same-hash / `Etched` / identical `Pending` are skipped, seed conflicts and
   empties are safe skips) and, when any land `Pending`, verified in **one**
   account-fields call; unverifiable seeds are purged so no address is left
   `Pending`.
3. **Bundles hydration.** One `evm_fork_cache::bulk_storage::run_storage_programs`
   call runs every fast pool's one-shot program; distinct targets bundle into a
   single multicall `eth_call`. A program that errors (offline/gas/transport) or
   whose output fails to decode moves that pool to the fallback set.
4. **Falls back** per pool through the normal multi-round `cold_start` for every
   fallback pool. Because step 2 already marked shared code `Verified`, the
   fallback's own seeding is a no-op.

Outcomes are returned one-per-pool in **input order**; an empty slice is a no-op
that never touches the provider. Request count now scales with bootstrap
*phases* (seed-verify + one bundled hydration + whatever the fallbacks need),
not with pool count, fee-tier count, or warmed-slot count.

`supports_one_shot_hydration(pool)` is exactly `hydration_kind(pool).is_some()`,
so the classification a caller can inspect never disagrees with what
`cold_start_many` actually attempts.

### Still future: a discovery-integrated `Bootstrapper`

`cold_start_many` covers the hydration + finalization half of the "Desired
default" below. Not yet built: a single high-level entry point that also owns
*discovery* and returns a structured report — strategy knobs (`FastestSafe` /
`Conservative` / `FastOnly`), per-phase timings, request counts, loaded-slot and
verified-seed tallies, explicit block pinning, and provider-capability
detection. Until then the example wires discovery → `cold_start_many` →
`register_pools` directly.

## Live exercise

The example path was:

1. Query the Ethereum Uniswap V2 and V3 factories for USDC/WETH pools.
2. Cold-start every discovered pool through `AdapterRegistry::cold_start`.
3. Register those pools with `AmmSyncEngine`.
4. Subscribe to the discovered pool addresses and feed logs back into
   `AmmSyncEngine::ingest_batch`.

The instrumented run used a public Ethereum RPC/WS endpoint, so the absolute
latencies should not be treated as stable benchmark numbers. The shape of the
work is still useful:

```text
benchmark: setup=631.24ms, factory_discovery=456.20ms, cold_start=1304.44ms, engine_register=0.14ms, ws_subscribe=745.67ms, event_loop=30001.49ms, total=33139.59ms
benchmark_detail: pools=5, verified_slots=3737, changed_slots=3737, first_log=10744.55ms, first_applied=10745.07ms, ingest_total=0.51ms, ingest_avg=0.51ms, ingest_max=0.51ms
```

The relevant bootstrap work before waiting for live events was roughly:

| Phase | Time |
| --- | ---: |
| Provider/cache setup | 631 ms |
| Factory discovery | 456 ms |
| Cold-start five pools | 1,304 ms |
| Engine registration | 0.14 ms |
| WS subscription setup | 746 ms |

The run discovered one V2 pool and four V3 pools, then warmed 3,737 storage
slots. Event ingestion itself was cheap: the first V3 `Swap` applied in 0.51 ms.
The long total runtime was dominated by waiting for live logs, not by reactive
application.

## What happened

The demo used the obvious high-level lifecycle API:

```rust
let discovered = discovery.find(...)?;
let mut registration = discovered.registration;
registry.cold_start(&mut registration, &mut cache, ColdStartPolicy::Eager)?;
engine.register_pools([registration])?;
```

That API is correct and conservative, but it is not the fastest available
route.

At the time of this analysis, factory discovery resolved derived mapping slots
one at a time:

- V2 read one `getPair[token0][token1]` slot.
- V3 looped fee tiers and read one `getPool[token0][token1][fee]` slot per tier.
- For every found V3 pool it also read `feeAmountTickSpacing[fee]`.

For USDC/WETH this was nine derived-slot reads — cheap, but not planned as one
bulk watchlist. **Shipped since:** `PoolDiscovery::find` / `find_many` now
gather every factory's candidate slots (`PoolFactory::candidate_reads`) into a
single batched `read_storage_slots` call.

Cold-start, at the time of this analysis, ran one pool at a time through the
generic cold-start planner (the shipped `cold_start_many` now bundles
one-shot-eligible pools; the per-pool planner remains the fallback). V3 is necessarily dependent in this planner:

1. read `slot0` + global liquidity;
2. derive the current bitmap word window from `slot0`;
3. read bitmap words;
4. derive initialized ticks;
5. read tick info slots.

Even when the underlying `EvmCache` storage fetcher uses bulk extraction inside a
round, the high-level call shape still prevented collapsing all pools and all
known work into a single bootstrap plan, and V3 cold-start did not yet use the
one-shot V3 sync program (both are addressed by `cold_start_many`; a unified
discovery-to-ready `Bootstrapper` remains future work, below).

## Existing fast primitives

The crate already has the building blocks:

- `storage_sync` can build flat-slot storage programs for protocols with known
  slots. `run_storage_syncs` executes many distinct targets through
  `evm-fork-cache::run_storage_programs`, which can bundle them into one
  Multicall3-dispatched `eth_call`.
- `v3_sync` can generate V3 one-shot sync programs that walk pool bitmap/tick
  state in-program and return a full or partial pool snapshot.
- `evm-fork-cache` has a default bulk storage fetch strategy and a bulk account
  fields fetcher for code-seed verification.
- `AmmSyncEngine` already provides the intended live path after bootstrap,
  because it uses the resync-capable runtime ingestion path.

Those primitives are not hidden in private modules, but they are not surfaced as
one lifecycle. A user must know:

- which protocols use `storage_sync` versus `v3_sync`;
- which metadata must already exist before one-shot sync is valid;
- how to inject returned storage into `EvmCache`;
- how to seed and verify bytecode claims;
- how to decide the registration can be marked `Ready`;
- when to fall back to generic cold-start.

That is too much policy for application authors.

## Why the fast path was not automatic

There is no strategy layer between discovery and cold-start.

The current abstractions answer local questions:

- `PoolDiscovery`: given a protocol and token query, find registrations.
- `AdapterRegistry::cold_start`: given one registration, warm it conservatively.
- `storage_sync`: given an already-known flat read-set, load those slots quickly.
- `v3_sync`: given V3 metadata, run a one-shot V3 loader.
- `AmmSyncEngine`: given ready registrations, keep them live.

No public API currently answers the user-level question:

> Given these factory configs and pool queries, discover pools, hydrate the cache
> as fast as safely possible, verify bytecodes, mark registrations ready, and
> return something I can register with the live engine.

Because that orchestration does not exist, the obvious example naturally reached
for `PoolDiscovery::find` plus `AdapterRegistry::cold_start`.

## Desired default

The fast route should become the default happy path, with generic cold-start as
fallback rather than as the primary route.

Proposed shape:

```rust
let report = AmmBootstrapper::new(registry, factory_config)
    .with_strategy(BootstrapStrategy::FastestSafe)
    .discover_many(&mut cache, [
        BootstrapQuery::uniswap_v2_pair(USDC, WETH),
        BootstrapQuery::uniswap_v3_pair(USDC, WETH).with_fee_tiers([100, 500, 3_000, 10_000]),
    ])
    .await?;

engine.register_pools(report.ready_pools)?;
```

The default should choose:

1. **Bulk factory resolution.**
   Compute all derived factory mapping slots and tick-spacing slots, then resolve
   them as a bulk read-set. For multiple factories, use distinct target storage
   programs bundled through the `run_storage_programs` route.

2. **Bytecode seed planning.**
   Ask every discovered registration for `AdapterCodeSeed`s, write those to
   `EvmCache`, and verify all pending code seeds through the account-fields
   fetcher in one batch.

3. **Protocol-specific bulk hydration.**
   Use flat-slot storage programs for V2/Solidly/Balancer/Curve where a fixed or
   discovered read-set exists. Use V3 one-shot programs for Uniswap V3-family
   pools with complete immutable metadata.

4. **Lifecycle finalization.**
   Mark registrations `Ready` only after the metadata contract, code verification
   contract, and storage hydration contract all pass. Return structured fallback
   reasons for any pool that cannot use the fast path.

5. **Conservative fallback.**
   Fall back to `AdapterRegistry::cold_start` per pool when a protocol has no
   fast planner, when provider capabilities are missing, or when a fast planner
   reports an incomplete result.

## Expected request shape

It is probably not realistic to make the whole process one RPC request in the
general case because pool state targets depend on factory outputs. A practical
target is a small fixed number of calls:

1. one call for block pin / chain context if not already known;
2. one bundled factory-resolution call;
3. one bundled pool-hydration call;
4. one account-fields call for bytecode verification, unless it can be combined
   with a broader account bootstrap phase.

The important goal is not literally "one request in all cases"; it is that
request count should scale with bootstrap phases, not with number of pools,
number of V3 fee tiers, or number of warmed slots.

## Protocol notes

| Protocol | Fast discovery | Fast hydration | Fallback |
| --- | --- | --- | --- |
| Uniswap V2 | derived `getPair` slot, batchable across pairs | flat slot loader for token0/token1/reserves | generic V2 cold-start |
| Uniswap V3 | derived `getPool` + `feeAmountTickSpacing`, batchable across tiers/pairs | V3 one-shot full or partial sync program | generic V3 multi-round cold-start |
| Pancake V3 / Slipstream | same family, but factory/deployer/layout config must be explicit | V3-family one-shot sync when metadata/layout is complete | generic V3-family cold-start |
| Solidly V2 | factory support still needs protocol-specific config | flat slot loader once layout is known | generic Solidly cold-start |
| Balancer V2 | factory/discovery work still protocol-specific | flat loader once vault balance slots are discovered/persisted | generic Balancer discover-verify cold-start |
| Curve | registry/factory discovery varies by deployment | flat loader once discovered slots are persisted | generic Curve discover-verify cold-start |

Balancer and Curve need the same treatment before this work is complete: once
their pool discovery and read-set metadata are available, their default bootstrap
path should use flat storage programs rather than forcing users through
discover-verify every time.

## API requirements

The high-level bootstrap API should provide:

- a report with per-phase timings, request counts, loaded slots, verified code
  seeds, ready pools, and fallback pools;
- explicit block pinning in the report so discovered metadata and warmed storage
  are known to correspond to the same block;
- provider capability detection for state overrides, Multicall dispatch, account
  fields extraction, and trace/debug support where relevant;
- strategy knobs for `FastestSafe`, `Conservative`, and `FastOnly`;
- per-protocol feature gating matching the existing adapter feature flags;
- a single registration handoff into `AmmSyncEngine`.

The report should make performance regressions obvious. If a user asks to
bootstrap 500 pools and the implementation falls back to per-pool cold-start,
that must be visible in the returned plan/report, not hidden behind a slow call.

## Migration checklist

1. **Done.** `PoolDiscovery::find`/`find_many` turn `PoolQuery`s into derived
   candidate reads resolved in one batched `read_storage_slots` call
   (`PoolFactory::candidate_reads` + `assemble_pairs`).
2. Add a `BootstrapPlan` or `AdapterBootstrapPlanner` trait for protocol
   adapters to contribute code seeds, metadata requirements, storage programs,
   and fallback reasons.
3. **Done.** V2/flat sync and V3 one-shot sync are integrated into lifecycle
   finalization via `AdapterRegistry::cold_start_many`: they now produce `Ready`
   registrations (bundled hydration + batched seed-verify + graceful per-pool
   fallback), not just cache mutations.
4. Add a high-level discovery-integrated `Bootstrapper` (see "Still future"
   above) that pairs bulk discovery with `cold_start_many` behind strategy knobs
   and returns a structured per-phase report of every fallback.
5. **Done.** `examples/factory_discovery_live.rs` now bootstraps through
   `cold_start_many` (discover → `cold_start_many` → `register_pools`),
   demonstrating the fast path with per-pool fallback rather than the
   per-pool conservative loop.
6. Add benchmark coverage that times discovery, hydration, registration, and
   live event ingestion separately.

The user-facing benchmark should prove that adding more pools primarily grows
returned bytes and EVM work inside storage programs, not JSON-RPC request count.
