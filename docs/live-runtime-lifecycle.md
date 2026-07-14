# Transactional Synchronous Lifecycle

Status: Stage 3 complete
Date: 2026-07-10

Stage 3 makes the existing `AmmSyncEngine` working set mutable without replacing
its `ReactiveRuntime`. It is deliberately synchronous: subscriber ownership,
backfill acknowledgement, cache actors, immutable publication, and the `Live`
transition belong to Stage 4.

## Public lifecycle surface

The typed methods are:

```rust,ignore
engine.add_pools(registrations)?;
engine.remove_pools(&keys)?;
engine.remove_pools_evicting(&keys, &mut cache)?;

engine.add_adapter(adapter)?;
engine.remove_adapter(protocol)?;
engine.remove_adapter_cascade(protocol)?;
engine.remove_adapter_cascade_evicting(protocol, &mut cache)?;
```

They return `AmmLifecycleReport`, including accepted pool/adapter generations,
removed registrations, detached work, exact pending-resync cancellations, and
the explicit cache-eviction result. `register_pools`, `unregister_pools`, and
`unregister_pools_evicting` remain compatibility projections over the same
incremental core.

`replace_registry` remains an intentionally expensive escape hatch. It creates
a fresh runtime and loses journal/pending continuity, but it advances every
retained generation so discarded work cannot alias the replacement.

## Pool transaction

Addition canonicalizes and validates the complete batch before mutation:

1. reject duplicate keys and missing adapters;
2. allocate checked successor generations from a retained high-water ledger;
3. prepare handlers and ownership records without publishing them;
4. register only the new handlers in the existing runtime;
5. incrementally commit registry, routing-context, and ownership entries;
6. commit generation/lifecycle state and return the typed report.

An installation failure removes only handlers and records introduced by that
attempt. Existing hooks, health, journals, freshness state, pending requests,
and unrelated handlers survive.

Removal resolves the exact active generations, preflights every handler and
routing entry, transitions them through `Removing`, unregisters only those
handlers, cancels only their owned `ResyncId`s, detaches ownership/registry
records, and leaves lifecycle tombstones at `Removed`. Unknown logical keys are
clean no-ops. Re-adding a key always receives the next generation.

The synchronous status mapping is conservative:

| `PoolStatus` | `PoolRuntimeState` |
| --- | --- |
| `Pending` | `Discovered` |
| `Cold` | `Hydrating` |
| `Ready` | `Searchable` |
| `Degraded` | `Degraded` |
| `Disabled`, `Unsupported` | `Failed` |

Stage 3 never claims `Live`; that requires subscriber/backfill continuity.

## Adapter transaction

One canonical `AdapterKey` owns all protocols served by a multi-protocol
adapter. Default removal rejects while dependent pools exist. Cascade removal
preflights the whole family, removes every dependent pool through the ordinary
pool path, then commits adapter removal through no-scan, prevalidated detach
primitives in the primary registry, routing registry, and ownership index.

Exclusive eviction is deliberately last: cache state is not purged until pool
and adapter commit has succeeded. Re-adding the family receives the checked
successor adapter generation.

## Indexed routing and cancellation

`evm-fork-cache 0.3.0` adds an optional exhaustive `LogRouteIndex` to generic
reactive handlers. `AmmPoolReactiveHandler` declares exact emitter or indexed
topic keys for direct, indexed-address, and indexed-bytes32 sources. Existing
provider filters and local matchers are still rechecked. Adapter-defined routes
remain on the compatible fallback path because third-party adapters do not yet
promise an exhaustive key.

The upstream registry uses stable monotonic ordering, O(1) handler-ID lookup,
and ordered owner sets. Indexed hit/miss dispatch and handler churn therefore do
not scan unrelated handlers or retain tombstones.

Pool teardown collects every generation-owned `ResyncId` and calls
`ReactiveRuntime::cancel_pending_resyncs_by_id` once per removal transaction.
The upstream runtime scans the pending queue once and preserves queue order. It
never uses address-wide cancellation, so removing one Balancer-style
shared-vault pool cannot cancel another pool's request merely because both
target the vault.

## Cache eviction

Removal defaults to `AmmEvictionPolicy::Retain`. The explicit `Exclusive`
policy purges a whole address only when no remaining pool is associated with
it. At shared addresses it purges only exact slots with no remaining owner.
Unknown/lazy state is retained rather than guessed.

An explicitly evicted generation remains fenced while its handler occurs in
the bounded upstream reorg journal. Ordinary canonical batches only age and
retire fences without touching the cache. A batch that recovers a reorg
re-applies ownership-aware eviction before its result is exposed; an ingest
error enforces conservatively before it is propagated because recovery may
already have mutated the journal and cache. A late reorg therefore cannot
rematerialize purged state, while re-added or co-tenant owners still protect
their current addresses and slots. Re-registering the same logical pool retires
its older fence because the caller has explicitly re-adopted that state; a
later `Retain` removal of the replacement is therefore not overridden by the
older generation's eviction policy.

## Continuity and performance

Tests prove that canonical journal history still rolls back correctly after an
unrelated pool is added and removed, and that same-address pending work survives
exact teardown of another generation.

On the Stage 0 M1 Pro host, the shortened Stage 3 capture measured:

- register one at 320 pools: about `6.82us` (Stage 2: `1.1888ms`);
- unregister one at 320 pools: about `7.62us` (Stage 2: `1.1447ms`);
- indexed route hit: about `0.215-0.250us` from 16 through 4,096 pools;
- indexed route miss upstream: about `27ns` through 4,096 handlers.

See `docs/live-runtime-baselines.md` for commands and comparison policy.

## Verification

Primary suites:

```text
cargo test --lib
cargo test --test runtime_ownership
cargo test --test adapter_sync_manager
cargo test --test adapter_reactive
cargo bench --bench runtime_lifecycle
cargo bench --bench pool_routing
```

Coverage includes atomic add rejection, generation-fenced remove/re-add,
adapter-defined routing mutation, adapter-family isolation/cascade, shared-state
eviction, eviction followed by a retained-history reorg, batched exact pending
cancellation under backlog, and reorg-journal continuity.

## Release boundary

The local workspace binds state and search to sibling `evm-fork-cache 0.3.0`
with both `path` and `version`, preventing duplicate crate identities during
development. Cargo packaging preserves the version requirement and removes the
local path. Release order is therefore load-bearing: publish `evm-fork-cache
0.3.0` before packaging/publishing `evm-amm-state`, then validate search against
the published pair. Until that first publish, the state package gate correctly
fails dependency resolution because crates.io offers only `0.2.1`.
