# Live AMM Runtime Master Plan

Status: Stages 0-10 implementation complete; external release gates remain
Date: 2026-07-11
Owner crate: `evm-amm-state`
Supporting crate: `evm-fork-cache`
Consumer crate: `evm-amm-search`

## Purpose

This is the canonical implementation plan for turning the current batch-shaped
AMM bootstrap and caller-orchestrated live path into a progressively available,
dynamically reconfigurable runtime.

The desired end state is:

- runtime startup returns without waiting for pool discovery or cold-start;
- cold-start and repair run in the background with structured progress;
- each coherent pool becomes available independently of the rest of its batch;
- pools and adapters can be added and removed without rebuilding unrelated
  runtime state;
- search consumes typed, ordered AMM changes rather than parsing raw logs or
  generic reactive reports;
- graph, liquidity, and route-session state update incrementally;
- quotes always identify the block, AMM state version, and graph version they
  were computed against;
- applications can express a pipeline such as
  `input -> state commit -> graph update -> route refresh` without knowing the
  cache, reactive-runtime, or adapter repair internals.

This document consolidates and supersedes the sequencing portions of the
workspace-level progressive cold-start, dynamic registration, incremental graph,
and declarative pipeline plans. Those documents remain useful detailed design
references, but implementation status is tracked here.

## Stage 0 Baseline

Before implementation, the crates already contained these low-level
primitives:

- `AdapterRegistry::cold_start_many` performs bulk one-shot hydration with
  per-pool planner fallback;
- `AmmSyncEngine::ingest_batch` always executes reactive resync requests and
  updates degraded/recovered pool status;
- `AdapterRegistry` can add and remove pools and adapters;
- `evm-fork-cache` provides `ReactiveRuntime`, owner-aware subscriber interests,
  `ReactiveEngine`, reorg journaling, resync execution, and immutable
  `EvmSnapshot` fan-out;
- `evm-amm-search` provides streaming route search, incremental state refresh,
  quote reuse, parallel overlays, and liquidity-ranked heuristics.

The missing capability was a coherent high-level lifecycle. At Stage 0:

- bootstrap returns results only after the whole batch finishes;
- `AmmSyncEngine::register_pools` reconstructs its `ReactiveRuntime`;
- a registry-wide `AmmReactiveHandler` owns a frozen registry clone;
- provider subscription ownership remains with the application;
- applications join raw logs, AMM sync, affected-pool detection, graph rebuilds,
  and route refresh manually;
- cache, graph, and liquidity readiness are exposed as one large startup barrier.

## Fixed Architectural Boundaries

These decisions are load-bearing. Changing one requires updating this document
and the affected acceptance tests before implementation changes.

### Crate ownership

`evm-fork-cache` owns protocol-neutral mechanisms:

- the canonical mutable EVM cache;
- immutable snapshots and speculative overlays;
- generic reactive input ordering, journaling, resync, and subscriber mechanics;
- generic cold-start fetch/apply primitives where they are reusable outside AMMs.

It must not gain AMM pool, protocol, adapter, graph, or route types.

`evm-amm-state` owns AMM truth:

- adapter and factory configuration;
- pool discovery and registration metadata;
- pool lifecycle and generation identity;
- cold-start, catch-up, repair, and degradation policy;
- the only canonical `EvmCache` mutation path;
- typed AMM changes and immutable AMM state publication.

`evm-amm-search` owns search truth:

- live token/pool graph topology;
- graph versions and deltas;
- liquidity ordering sidecars;
- route subscriptions, route sessions, quote scheduling, and search events.

### Single canonical cache writer

Exactly one runtime actor mutates the canonical `EvmCache`.

Discovery, provider reads, storage-program execution, access-set derivation, and
quote computation may run concurrently when they operate on immutable inputs.
Their results become canonical only through a serialized, version-checked commit
on the cache owner.

Search never borrows the live cache actor for the duration of a route search. It
uses an immutable `Arc<EvmSnapshot>` and creates isolated overlays per worker.

### Synchronous compatibility

The actor runtime is additive. Synchronous APIs remain supported for:

- deterministic unit and integration tests;
- small examples;
- callers that explicitly prefer batch bootstrap;
- downstream consumers that own their own transport/runtime loop.

`AmmSyncEngine` remains the synchronous AMM live driver. Its implementation will
be made incremental before the actor runtime is layered over the same lifecycle
core.

`replace_registry` remains an explicitly expensive compatibility operation.
Existing `register_pools` and `unregister_pools` delegate to the incremental
lifecycle core once it exists. `AmmReactiveHandler` remains public as a
compatibility wrapper, while the actor runtime uses pool-scoped handlers.
Subscriber/backfill modes live on the asynchronous runtime handle, not on the
providerless synchronous API. The asynchronous actor dependencies must be
feature-gated; `tokio` does not become an unconditional state-crate dependency.

### Eviction is explicit

Removing a handler or subscription stops future decode and routing. It does not
implicitly purge cache state.

Cache eviction is a separate policy decision because state may be shared by:

- several pools;
- a vault or router;
- a pending simulation snapshot;
- a pool that may be re-registered shortly.

## Runtime Identity And Version Model

### Pool identity

`PoolKey` is logical identity. It is not sufficient asynchronous job identity.

Every registration receives a monotonic generation:

```rust
pub struct PoolGeneration(u64);

pub struct PoolInstanceId {
    pub key: PoolKey,
    pub generation: PoolGeneration,
}
```

Removing and re-adding the same `PoolKey` creates a new `PoolInstanceId`. Late
fetch, cold-start, backfill, repair, and quote results for an older generation
must be rejected before mutation or publication.

Generation counters survive logical removal for the lifetime of the runtime.
Persisted registrations receive fresh runtime generations after restart.
No in-flight job survives a process restart, so generations need not be stable
across processes. Exhausting any generation/version `u64` is a terminal runtime
error: counters never wrap or silently reuse an identity.

Pools are not the only asynchronous owners. Adapter instances and factory
watchers/discovery sources also receive monotonic generations:

```rust
pub struct AdapterInstanceId {
    pub key: AdapterKey,
    pub generation: AdapterGeneration,
}

pub struct DiscoveryOwnerId {
    pub key: DiscoveryOwnerKey,
    pub generation: DiscoveryGeneration,
}

pub enum RuntimeOwnerId {
    Pool(PoolInstanceId),
    Adapter(AdapterInstanceId),
    Discovery(DiscoveryOwnerId),
}

pub struct WorkId(u64);
```

`AdapterKey` identifies one registered adapter family and its declared protocol
IDs; one generation fence therefore covers multi-protocol adapters. Every
provider/discovery/scheduler result carries its owning instance ID plus
the target adapter instance where applicable and a monotonic per-runtime
`WorkId`. Removing and re-adding an adapter or factory watcher allocates a fresh
generation, so late results cannot register pools into the replacement instance.
Retrying work under the same owner always allocates a fresh `WorkId`; commit and
publication require it to equal the owner's currently active attempt. A
cancelled/superseded result is rejected even when owner generation and block pin
still match.

### State versions

```rust
pub struct AmmStateVersion(u64);
pub struct PoolStateRevision(u64);
```

`AmmStateVersion` starts at `0` for the initial published snapshot and advances
exactly once for every subsequent coherent `AmmStateSnapshot`. It is independent
of `EvmCache::snapshot_generation`, which does not advance for every operation
that can make AMM state searchable.

`PoolStateRevision` advances when a specific pool's quote-relevant state,
metadata, shared dependency, or availability changes. The initial safe search
quote-cache key includes `PoolInstanceId`, `PoolStateRevision`, and the complete
`AmmStatePoint`, in addition to request/simulation inputs. A result can therefore
never be relabeled across blocks when timestamp, number, base fee, or another
block-environment field may affect execution. It does not advance merely because
an unrelated pool changes, and same-point unaffected quotes remain reusable
without a whole-cache invalidation scan.

Adapters must map shared/external quote dependencies to every affected pool
revision. A future optimization may replace the whole point with an
adapter-declared, complete dependency fingerprint and block-context sensitivity
mask, but only after equivalence tests prove stale cross-point reuse impossible.
Every cached quote retains the state point/version at which it was produced.

The search crate owns a separate `GraphVersion`. An AMM state update need not
advance graph topology, and a graph mutation must record the AMM state version
that caused it.

### Chain state point

Every canonical snapshot and asynchronous state job carries:

```rust
pub struct AmmStatePoint {
    pub chain_id: u64,
    pub block_number: u64,
    pub block_hash: B256,
    pub position: StatePosition,
}

pub enum StatePosition {
    PostBlock,
}
```

The first runtime version supports post-block state only. The explicit enum
prevents later pre-transaction or pending overlays from silently reusing the
wrong backfill convention. The AMM runtime owns this state point directly;
correctness does not depend on `ReactiveRuntime::last_canonical_block`, which is
unavailable when journaling is disabled. High-level cold-start APIs must expose
an explicit hash pin rather than relying only on the cache's implicit block
number.

## Pool Lifecycle State Machine

`PoolStatus` continues to express adapter/search health (`Pending`, `Ready`,
`Degraded`, and so on). Scheduler/transport state is represented separately:

```text
Discovered
    -> Queued
    -> Hydrating
    -> CatchingUp
    -> Searchable
    -> Live
    -> Degraded

Degraded
    -> CatchingUp (repair from a verified baseline)
    -> Live (exact repair with continuity preserved)

Any nonterminal state
    -> Removing
    -> Removed

Queued / Hydrating / CatchingUp
    -> Failed
    -> Queued (explicit retry policy)
```

### State meanings

| State | Contract |
| --- | --- |
| `Discovered` | Metadata candidate exists; no job commitment has been made. |
| `Queued` | A generation-scoped cold-start job is accepted. |
| `Hydrating` | At least one cold-start unit is in flight or ready to commit. |
| `CatchingUp` | A verified staging baseline is being advanced to the runtime's canonical position; it is not yet in a published canonical snapshot. |
| `Searchable` | A coherent published snapshot can simulate the pool at the runtime state point. |
| `Live` | Handler, subscriber interests, and canonical state are continuous through the runtime position. |
| `Degraded` | The pool is registered but its current-state quality is insufficient for normal search policy. |
| `Removing` | New work is rejected; existing work is being cancelled and routing removed. |
| `Removed` | No handler, subscriber owner, job, or graph registration remains for this generation. |
| `Failed` | The current attempt ended without a searchable registration; retry is explicit. |

The operational state is a sidecar, not a replacement for public `PoolStatus`.
The normative relationship is:

| Operational state | Required/normal `PoolStatus` |
| --- | --- |
| `Discovered`, `Queued` | normally `Pending` |
| `Hydrating`, `CatchingUp` | normally `Cold` |
| `Searchable`, `Live` | `Ready` |
| `Degraded` | `Degraded` |
| `Failed` | preserves the adapter result; it is not automatically `Unsupported` |

`Disabled` and `Unsupported` registrations do not enter normal hydration or the
search graph without an explicit retry/reclassification transition.

### Registration provenance

Every accepted registration records a typed origin. At minimum the model
distinguishes stable manual/configuration identity, state-query discovery, and
factory-log discovery. Factory-log provenance carries factory address, block
number/hash, transaction hash, and log index; watcher ownership carries the
configuration generation that installed it. State-query provenance carries the
queried block number/hash and an explicit finality/revalidation policy; a
latest/nonfinal query is not treated as stable evidence.

### Searchability versus optional warming

A pool becomes `Searchable` when the minimum state and code required by its
adapter can produce a correct quote in a coherent snapshot at the runtime's
published state point. A historical staging baseline never enters the canonical
graph by itself: mixing pools from different chain points would make route
quotes incoherent. It is caught up off to the side, then committed and published
as soon as it reaches an accepted actor point.

`Searchable` pools enter the graph immediately after that publication. `Live`
is stronger: catch-up has been acknowledged through the actor's current
canonical point and subsequent live delivery is continuous.

The following must not delay searchability when the adapter can safely operate
without them:

- wider V3 tick windows;
- full liquidity ranking data;
- nonessential quote-target prefetch;
- optional read-set discovery refinements;
- low-priority persistence writes.

Such work is reported as deferred background work.

## Backfill And Catch-Up Semantics

### Post-block baseline rule

A cold-start at post-block state `N` already includes every transaction and log
from block `N`. Its catch-up range begins at `N + 1`.

Examples:

| Baseline | First log to apply |
| --- | ---: |
| Post-block 100 | 101 |
| Post-block pool-creation block 500 | 501 |

Factory discovery alone is not a post-block hydration. A pool discovered from a
creation log in block `N` and not yet hydrated may replay from `N`. Once provider
reads establish a post-block `N` baseline, catch-up begins at `N + 1`.

Backfilling block `N` on top of post-block state `N` can double-apply event
deltas and is forbidden.

`evm-fork-cache` 0.2.1 exposes a `ReactiveReport::BlockCommitted` variant, but
the current reactive source does not construct that report. Stage 1 must not
infer block-final publication from the enum's existence. It must either derive
the canonical batch boundary from the accepted `ReactiveInputBatch` plus the
runtime journal head, or first add and test upstream `BlockCommitted` emission.
In either case, a newly registered post-block `N` pool must explicitly request
catch-up from `N + 1`; the existing inclusive handler backfill default is not a
safe registration anchor for this lifecycle.

If a future workflow cold-starts a pre-block/pre-transaction state, it must add
a distinct `StatePosition` and its own tested range rule.

### Registration continuity

Progressive registration uses subscriber-owned buffering plus a staging state,
then makes canonical routing visible as one transaction:

1. validate pool generation and adapter;
2. prepare the pool-scoped handler and interests;
3. install subscriber ownership and begin buffering owner-matched input;
4. hydrate a generation- and hash-pinned staging state at post-block `N`;
5. replay `N + 1` toward a recent actor point in staging;
6. enter a serialized actor batch boundary and establish an exact commit fence;
7. replay every owner input through that fence into staging while the actor does
   not advance to the next canonical batch;
8. validate generation, hash lineage, state point, and the complete pool-owned
   write set;
9. commit the caught-up pool state, registry record, indexes, and canonical
   runtime routing together at that exact actor point;
10. publish `Searchable`, its snapshot/change set, and graph eligibility;
11. resume canonical input; only inputs strictly after the published fence may
    remain buffered, and they use normal canonical routing;
12. mark `Live` only after catch-up reaches the current canonical position and
    continuous delivery is acknowledged.

If the fenced replay would exceed the actor's bounded commit budget, the install
yields without publication, catches up in staging again, and retries at a later
batch boundary. It never publishes a pool behind the snapshot's global point.

Any failure before canonical commit removes subscriber ownership and discards
staging state. Any failure during canonical installation rolls back routing,
indexes, and registry visibility before publication. The generic subscriber
contract must either guarantee atomic interest updates or the first runtime
implementation is explicitly constrained to `AlloySubscriber`.

The runtime must not process a queued subscriber batch against a half-installed
registration. Subscriber-interest acknowledgement means buffering is installed;
canonical registration acknowledgement follows step 10. Neither means the
post-commit buffer has drained. The subscriber driver must add a
protocol-neutral owner progress acknowledgement through a specific canonical
block/hash. `Live` is emitted only after the actor observes that acknowledgement
and has committed every preceding input.

### Reorgs during lifecycle work

Every cold-start, catch-up, repair, and quote result carries its block baseline.

On reorg:

- a result pinned to a dropped hash is rejected;
- a pool in `CatchingUp` restarts from a canonical baseline;
- a previously published quote is marked stale when its state point is dropped;
- old-generation and stale-baseline results remain harmless even if their
  provider calls complete successfully;
- a pool whose factory-log creation provenance is orphaned transitions through
  `Removing`, cancels generation-owned work, loses graph/runtime/subscriber
  ownership, and publishes a removal change set;
- an orphaned factory watcher/configuration generation is removed with the same
  owner-scoped rollback, while stable manual configuration remains installed;
- a registration supported only by a state query at an orphaned/nonfinal hash is
  revalidated against the new canonical state and removed if that evidence no
  longer exists; finalized query evidence remains valid;
- canonical rediscovery of the same logical `PoolKey` is a new accepted
  registration and receives a fresh generation.

Rollback is provenance-driven, not address-driven: a pool supported by stable
manual configuration or finalized query evidence is not removed merely because
a later duplicate discovery log was orphaned. Factory/query discovery and reorg
tests must cover create, orphan, revalidation, replacement, and duplicate-
evidence cases.

## Event And Snapshot Contract

### Critical change stream

`AmmChangeSet` is the ordered, correctness-critical input to search. It contains:

- `AmmStateVersion` and `AmmStatePoint`;
- affected `PoolInstanceId`s and their revisions;
- added, updated, degraded, recovered, and removed registrations;
- state quality and reorg/gap information;
- whether quoteability or topology metadata changed.

Delivery uses bounded reliable `mpsc` to the canonical search consumer. A
consumer may apply backpressure or recover from the latest snapshot plus
version, but it may not silently skip a change.

The runtime exposes an atomic subscribe/recover operation returning a current
`watch<Arc<AmmStateSnapshot>>` value and a change receiver whose first item is
strictly newer than that snapshot version. This closes the snapshot/change race.
Additional correctness-critical consumers need their own bounded delivery and
recovery contract rather than sharing the lossy observer stream.

### Observer stream

`AmmRuntimeEvent` is the richer lifecycle/progress/diagnostic vocabulary for
TUIs, services, logs, and metrics. It includes:

- batch/pool queued;
- classification and cold-start path;
- round start/completion;
- pool searchable/catching-up/live;
- repair and deferred work;
- cancellation/failure;
- registration/removal;
- state commit, reorg, gap, and runtime health.

Observer delivery uses lossy `broadcast`. Lag must be counted and surfaced to
the observer; it must never corrupt the critical search graph. Existing
inline `ReactiveHook` callbacks are diagnostic integration points, not the
state-to-search correctness channel.

Progress is also queryable and published through a latest-value watch so a
late or lagged TUI can recover without replaying observer events:

```rust
pub struct AmmRuntimeStatusSnapshot {
    pub sequence: u64,
    pub state_version: AmmStateVersion,
    pub lifecycle_by_owner: Arc<RuntimeLifecycleMap>,
    pub active_work: Arc<WorkProgressMap>, // keyed by RuntimeOwnerId + WorkId
    pub queued_by_class: Arc<QueueDepths>,
    pub health: AmmRuntimeHealth,
}
```

Every progress-affecting transition updates this snapshot before its observer
delta is emitted. Batch progress carries known completed/total units and bytes
or rounds where meaningful; unknown totals remain explicit rather than being
presented as percentages.

### Ordering

For a canonical reactive input batch:

1. accept, deduplicate, and order input;
2. decode adapter events;
3. apply direct effects;
4. execute required resyncs;
5. update degradation/recovery and pool revisions;
6. align and verify block context;
7. allocate the next `AmmStateVersion` and construct `AmmStateSnapshot`;
8. publish that snapshot and make the version current atomically;
9. emit `AmmChangeSet`;
10. emit observer events.

Consumers may rely on every change event referencing an already-published
snapshot of the same version.

### Immutable snapshot

```rust
pub struct AmmStateSnapshot {
    pub state_version: AmmStateVersion,
    pub point: AmmStatePoint,
    pub cache: Arc<EvmSnapshot>,
    pub registry: Arc<AdapterRegistrySnapshot>,
    pub pool_revisions: Arc<PoolRevisionMap>,
}
```

Registry snapshots change only when topology/metadata changes and should be
shared across state-only snapshots. `EvmCache::snapshot()` supplies the
copy-on-write EVM view for parallel search overlays. The wrapper carries the
block hash because `EvmSnapshot` does not expose one as public provenance.
Hydrating/catching-up staging state is never included until it is coherent with
the snapshot's `AmmStatePoint`.

### Batch failure coherence

Current upstream direct ingestion can mutate earlier records before a later
record returns an error. Until ingestion is transactional, the actor treats any
mid-batch error as canonical trust loss: it stops snapshot/change publication,
marks runtime health degraded, and rebuilds or resynchronizes from a verified
state point before resuming. It must never publish a new version over a partially
applied failed batch. A later upstream transactional API may replace this
recovery path without changing the public contract.

## Runtime Task Model

The intended live implementation has two cooperating components:

### Subscriber driver

- owns `AlloySubscriber` and provider streams;
- owns reconnect/backfill/dedup delivery state;
- accepts owner-scoped interest commands with acknowledgements;
- installs a block-header interest so the canonical EVM block context advances;
- emits ordered `ReactiveInputBatch` values to the cache actor.

### AMM runtime actor

- owns `EvmCache` and `ReactiveRuntime`;
- owns registry, lifecycle table, generations, revisions, and ownership indexes;
- owns the cold-start/repair scheduler;
- serializes canonical commits;
- publishes snapshots and events.

This split prevents a long-lived subscriber `next_batch().await` from starving
control commands. It reuses the owner-aware lifecycle semantics provided by
`ReactiveEngine` without requiring the application to update subscriber and
runtime state separately. The actor and driver must not be collapsed into a
single task loop around monolithic `ReactiveEngine` ownership.

## Cold-Start Scheduling Contract

### Priority

The scheduler services work in this order:

1. reorg and canonical live input;
2. registration catch-up;
3. required repair for already-tracked pools;
4. focused/user-requested pools;
5. normal bootstrap;
6. optional deferred warming.

Classes 2-6 use bounded work quanta plus weighted aging. Higher priority changes
latency preference, not eventual service: queued bootstrap and deferred work
cannot starve indefinitely, while class 1 always preempts before the next
canonical commit boundary.

### Cooperative units

- fast-eligible pools run in bounded micro-batches;
- fallback planners yield between rounds;
- one pool failure does not fail unrelated jobs;
- progress reports completed work and known totals without inventing percentages;
- cancellation is checked before fetch, before commit, and before publication.
- every job carries a generation-scoped `RuntimeOwnerId`, unique `WorkId`, target
  adapter generation where applicable, and explicit block/hash pin.

### Fetch versus commit

The final responsive design separates:

1. pure plan preparation;
2. provider work on bounded I/O workers;
3. serialized version-checked cache commit.

If reusable support is needed in `evm-fork-cache`, it must be expressed as
protocol-neutral fetch/apply cold-start primitives. AMM job types remain here.

## Incremental Registration Contract

Registration and unregistration must not construct a new `ReactiveRuntime`.

Registration is atomic across:

- generation allocation;
- handler routing;
- subscriber interest ownership;
- lifecycle/index insertion;
- catch-up anchor;
- publication.

Unregistration performs:

1. transition to `Removing`;
2. cancel generation-scoped cold-start, backfill, repair, and deferred work;
3. remove subscriber interests before future runtime routing;
4. remove runtime handler and request-ID/pool-owned pending resync work;
5. remove ownership indexes and the registry record;
6. publish removal;
7. optionally run explicit exclusive-state eviction;
8. transition to `Removed`.

Runtime-global health, hooks, journals, and unrelated pending work survive.
Address-wide resync cancellation is forbidden for pool teardown because shared
emitters such as vaults can serve unrelated pools.

Adapter removal rejects while dependent pools exist by default, matching the
current registry contract. An explicit cascading operation is transactional: it
marks the entire adapter-family generation `Removing` and rejects new dependent
work, prepares removal of every family-owned discovery watcher and pool through
their normal lifecycles, then removes the adapter and publishes one coherent
change set. A prepare/ack failure restores the active adapter generation and
owner interests before any canonical removal is published. Re-adding the same
`AdapterKey` is rejected until cascade acknowledgement reaches `Removed`; the
subsequent add always creates a fresh adapter generation.

## Implementation Stages

### Stage 0 — Contract and baseline

- [x] Freeze ownership, lifecycle, version, backfill, event, snapshot, and
  cancellation semantics in this document.
- [x] Define the reproducible baseline contract in
  `docs/live-runtime-baselines.md`.
- [x] Add an offline dynamic lifecycle benchmark.
- [x] Capture a fresh paid-RPC baseline after validation.

### Stage 1 — Runtime domain model and typed AMM changes

- [x] Add runtime identity, state point, lifecycle, version, change, and event
  types.
- [x] Add affected pools directly to `AmmSyncBatchReport`.
- [x] Derive typed changes only after direct effects and resync completion.
- [x] Publish deterministic event-order/version tests.

### Stage 2 — Per-pool reactive handlers and ownership indexes

- [x] Replace the live registry-wide handler with per-pool handlers.
- [x] Add address/slot/pool/adapter/job ownership indexes.
- [x] Preserve shared-emitter and adapter-defined routing behavior.
- [x] Prove routing/removal isolation.

### Stage 3 — Transactional synchronous lifecycle

- [x] Incrementally add/remove pools in `AmmSyncEngine` without runtime rebuild.
- [x] Add or require indexed upstream log routing so pool-scoped dispatch does
  not scan every unrelated handler.
- [x] Add generation-scoped rollback and typed lifecycle reports.
- [x] Add adapter add/remove lifecycle and explicit eviction policy.
- [x] Prove journal and pending-work continuity.

### Stage 4 — Cache-owner runtime

- [x] Add subscriber driver, cache actor, handle, commands, and shutdown.
- [x] Add reliable change, observer, and latest-snapshot channels.
- [x] Publish immutable cache/registry snapshots.
- [x] Prove command fairness and transactional subscriber/runtime updates.

### Stage 5 — Progressive cold-start scheduler

- [x] Refactor bulk/fallback cold-start into resumable jobs.
- [x] Stream pool and round progress.
- [x] Publish pools independently.
- [x] Split provider fetch from canonical commit.
- [x] Prove cancellation, stale-baseline rejection, and batch equivalence.

### Stage 6 — Discovery, repair, and adapter orchestration

- [x] Queue connector-focused and factory-event discovery.
- [x] Route `RepairAction::ColdStart` and `DeferredWork` through the scheduler.
- [x] Dynamically add/remove adapters and factory watchers.
- [x] Preserve multi-source provenance and durable owner-keyed revalidation.
- [x] Fence topology mutation behind subscriber transaction acknowledgement.
- [x] Prove the complete discover-to-remove lifecycle.

### Stage 7 — Incremental search universe

- [x] Add `LiveAmmGraph`, `GraphVersion`, and `GraphDelta`.
- [x] Incrementally update graph and liquidity targets.
- [x] Replace graph signatures with versions and topology impact.
- [x] Add revision-aware quote-cache keys.

Stage 7 verification: mixed incremental mutations are compared semantically
against full rebuilds; reliable commits prove state-only stability,
out-of-order/cross-runtime atomic rejection, and newer-snapshot recovery; live
simulations are bound to the snapshot's immutable cache, runtime lineage, and
pool generation/revision/point. Liquidity hydration accepts only the exact live
immutable snapshot.
The offline lifecycle benchmark measured incremental two-token additions at
`1.37%`, `0.44%`, and `0.04%` of full rebuild time for 16, 64, and 320 existing
pools, clearing the `<10%` gate at every size. Paid-RPC end-to-end measurements
remain a Stage 10 release gate.

### Stage 8 — Live route engine and typed pipeline

- [x] Add route subscriptions and typed route runtime events.
- [x] Consume reliable AMM changes and immutable snapshots.
- [x] Add reusable worker pool and external cancellation.
- [x] Reject stale state/graph results.

Stage 8 verification: the search-owned actor consumes the sole reliable AMM
stream, advances an incremental graph and immutable worker view before route
invalidation, and multiplexes recoverable route subscriptions plus lossy typed
pipeline events. A bounded persistent worker pool, actor-aware cancellation,
newest-only coalescing, and complete runtime/state-point/graph/subscription/job
fences prevent stale publication without blocking commit intake. The offline
integration suite covers commit priority, state-only and same-point graph
changes, observer lag/closure, panic isolation, global concurrency bounds,
external/drop cancellation, and critical-stream release. Paid-RPC and extended
soak measurements remain Stage 10 gates.

### Stage 9 — Consumer migration and persistence

- [x] Migrate the route TUI from manual orchestration.
- [x] Render and accept input while startup work continues.
- [x] Add progressive/dynamic examples.
- [x] Persist immutable registrations and read-set hints for warm resume.

Stage 9 verification: the route TUI now opens before provider/cache work, owns
partial-bootstrap cancellation, and composes `AmmRuntime`, the bounded cold-start
worker, the canonical subscriber, factory watchers, and `LiveRouteRuntime`
without manual graph rebuilding or direct log ingestion in its live loop. Exact
immutable route views, request/discovery generation fences, quiescent shutdown,
and propagated archive failures keep interactive state coherent. Providerless
examples prove pool-by-pool availability plus dynamic add/remove and fallback
without rebuilding the runtime or adapter. `AmmRegistrationArchive` provides a
bounded, deterministic, chain-scoped, atomically replaced local sidecar for all
built-in registration metadata and reusable read-set hints; restored queueable
records remain pending until current-baseline verification. The all-feature
test/clippy/rustdoc suites, minimal-feature checks, and both examples pass.
Phase 10 completed the remaining fault/reorg/cancellation hardening, hot-path
indexing, search-path optimization, offline acceptance matrix, paid-mainnet
acceptance matrix, and independent lifecycle/release review. Base-chain parity
still requires an external `E2E_BASE_RPC_URL`, and registry publication remains
dependency-ordered and explicitly outside automated validation.

### Stage 10 — Hardening and release

- [x] Complete fault injection and reorg/cancellation soak tests.
- [x] Remove remaining registry-wide hot-path scans.
- [x] Optimize search scheduler/reachability/path allocation after lifecycle work.
- [x] Publish paid-RPC and offline performance results.
- [x] Run independent lifecycle review and release gates.

Stage 10 acceptance is recorded in `live-runtime-baselines.md`. The final review
found and closed two release blockers: orphaned factory work after reorg and
uncancellable dense exhaustive route materialization. It also caught a package-
only example dependency defect and an incomplete pinned-header timestamp, both
of which now have regression coverage. Remaining work is operational rather
than implementation: run the explicit Base live gate when credentials are
available, then publish `alloy-transport-balancer 0.2.0`, `evm-fork-cache 0.3.0`, `evm-amm-state 0.2.0`, and
`evm-amm-search 0.1.0` in that order after each version becomes visible.

## Cross-Stage Test Matrix

| Capability | Unit | Property/model | Offline integration | Live gated | Benchmark |
| --- | ---: | ---: | ---: | ---: | ---: |
| Identity/version transitions | yes | yes | yes | no | no |
| Event ordering | yes | yes | yes | no | no |
| Add/remove rollback | yes | yes | yes | optional | yes |
| Shared-emitter routing | yes | no | yes | yes | yes |
| Cold-start progress/equivalence | yes | yes | yes | yes | yes |
| Cancellation/late result | yes | yes | yes | optional | no |
| Backfill anchor/catch-up | yes | yes | yes | yes | yes |
| Reorg during lifecycle | yes | yes | yes | yes | no |
| Snapshot read equivalence | yes | yes | yes | optional | yes |
| Incremental/full graph equivalence | yes | yes | yes | no | yes |
| Stale quote rejection | yes | yes | yes | optional | no |

Tests should verify public behavior rather than internal task placement. Each
implementation stage proceeds through small red/green vertical slices.

## Performance Gates

Stage 0 freezes these initial gates. Hardware- and provider-sensitive values are
reported as medians, p95, and maxima rather than single samples.

| Metric | Gate |
| --- | ---: |
| Runtime handle creation after provider/cache supply | `<10ms` offline |
| Control command enqueue | `<1ms p99` offline |
| UI/control return after queued discovery | `<10ms` offline |
| Live batch queue delay during background bootstrap | `<50ms p99` offline fixture, provider time excluded |
| Time to first connected route | `<50%` of full-bootstrap median |
| Full bootstrap throughput | no regression greater than `10%` from the frozen baseline |
| One two-token graph delta | `<10%` of full graph rebuild latency |
| Existing warm search | no regression greater than `10%` |
| Full runtime/graph rebuilds during add/remove | exactly `0` |
| Old-generation or stale-baseline canonical commits | exactly `0` |
| Quotes without state/block/graph provenance | exactly `0` |

## Stage Review Gate

Each stage is complete only when:

1. its focused tests pass;
2. the full relevant crate validation passes;
3. docs and examples match the public behavior;
4. performance gates are rerun where applicable;
5. the diff is reviewed specifically for lifecycle rollback, cancellation,
   ordering, and stale-result behavior;
6. downstream crates compile against the resulting public surface;
7. the stage is reported for alignment before the next stage begins.

Lifecycle-heavy stages 3, 4, 5, 6, and 8 require an independent review pass
before they are declared complete.
