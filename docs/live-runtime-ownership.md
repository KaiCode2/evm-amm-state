# Live Runtime Routing And Ownership

Status: Stage 2 ownership complete; Stage 3 lifecycle integrated
Date: 2026-07-10

Stage 2 replaces registry-wide live dispatch inside `AmmSyncEngine` with one
`AmmPoolReactiveHandler` per concrete pool instance and makes runtime ownership
explicit. `AmmReactiveHandler` remains available as the synchronous/public
compatibility wrapper; later lifecycle stages build on the pool-scoped path.

## Pool-scoped routing

Every pool-handler API is identified from `PoolInstanceId`, so removing
generation `G` cannot unregister or alias a separately allocated replacement at
generation `G + 1`. Its interests contain only that pool's event sources. Each
interest installs an exact local matcher in addition to the provider filter
because upstream `RouteKeySpec` extracts metadata but does not select a handler.

`AmmSyncEngine` now assigns generation zero only to a key's first accepted
registration and retains per-key high-water marks after removal. Ordinary
add/remove installs or removes only the exact handler generation in the existing
runtime. The explicit `replace_registry` compatibility escape hatch still
rebuilds, but allocates fresh generations rather than aliasing discarded work.

The matcher preserves all supported routing shapes:

- direct emitter-to-pool routing;
- indexed address and bytes32 routing for shared emitters;
- adapter-defined routing through `AmmAdapter::route_log`.

Adapter-defined routes use `AmmReactiveRoutingContext`, a shared copy-on-write
registry view. Generic direct/indexed matches are self-contained and do not
take its lock. Stage 3 incrementally mutates the routing view in the same pool
transaction; engine-level tests prove add/remove changes adapter-defined routing
without reconstructing the runtime.

Successful, failed, and actionable repair hooks carry the exact
`PoolInstanceId` through `AmmReactiveSignal::{PoolEvent, PoolDecodeError,
PoolRepair}`. Compatibility-handler hooks retain their existing logical-key or
payload-less format. Pool-handler repair IDs include the pool generation and
input position; compatibility IDs retain their pre-Stage-2 string format.

## Ownership model

`AmmOwnershipIndex` is the bidirectional source of truth for:

- logical pool key to active pool generation;
- pool instance to handler and adapter-family instance;
- adapter instance to dependent pools;
- associated state address to pools;
- whole-account storage ownership to pools;
- exact `(address, slot)` ownership to pools;
- event emitter to pools;
- runtime owner to generation-scoped jobs;
- resync ID to pool instance.

Address association, whole-account ownership, and exact-slot ownership are
separate concepts. This is essential for shared vaults: a coverage failure or
account purge affects every associated pool, while an ordinary slot write or
slot-scoped purge affects only the pools that declared that exact slot (plus
whole-account owners).

Adapters declare these relationships through the object-safe
`AmmAdapter::state_dependencies` seam. The conservative default treats a pool's
own address and configured state addresses as whole-account dependencies.
Balancer V2 and Curve override it with their discovered exact read sets so one
pool can be removed or degraded without contaminating another pool sharing the
same contract.

Index mutations validate all invariants before touching reverse maps. Failed
unknown-adapter, protocol-mismatch, duplicate-generation, duplicate-handler,
job, or resync insertions therefore leave the index unchanged. Adapter families
are canonicalized once across all protocols they serve.

## Compatibility sync integration

`AmmSyncEngine` now uses ownership lookups—not registry-wide protocol-specific
scans—for applied slot/account diffs, purge attribution, failed resync targets,
coverage gaps, recovery read sets, and exclusive-state eviction. Explicit
eviction snapshots the removed ownership before registry replacement, purges a
whole address only when no remaining pool is associated with it, and otherwise
purges only exact slots with no remaining owner.

Applied handler reports register each `ResyncId` against the exact requesting
pool before failure attribution. Failure handling consults that request owner
before target-based fallback, so two pools owning the same shared slot are not
both degraded for one pool's failed repair. Synchronous completion then removes
the IDs through `untrack_resync`; later schedulers can use the same operation
for completion, cancellation, or supersession.

When a multi-slot purge removes every requested slot, attribution remains
exact. Upstream reports only an aggregate removal count, so a partially
effective multi-slot purge cannot reveal which requested slots existed; that
case is deliberately published as `UnknownImpact` for the candidate owners
instead of fabricating exact per-pool updates.

`register_pools` and `unregister_pools` now project from typed transactional
Stage 3 methods and preserve runtime-global journals and pending work.
`replace_registry` alone remains explicitly rebuilding and generation-fenced.

## Performance evidence after indexed routing

The Stage 2 offline routing benchmark is:

```text
cargo bench --bench pool_routing
```

`evm-fork-cache 0.3.0` indexes exact emitter/topic/data keys and retains an
explicit fallback set. AMM direct/indexed handlers supply exhaustive keys;
adapter-defined handlers remain compatible fallbacks. A shortened Stage 3 run
measured pool-scoped routing at about `0.215-0.250us` from 16 through 4,096
pools. The compatibility handler remained linear, reaching about `39.42us` at
4,096 pools.

At 320 pools, incremental register/remove measured about `6.82us`/`7.62us`,
down from Stage 2's rebuilding `1.1888ms`/`1.1447ms`. See
`docs/live-runtime-baselines.md` and `docs/live-runtime-lifecycle.md`.

## Verification

The primary Stage 2 suites are:

```text
cargo test --test runtime_ownership
cargo test --test adapter_sync_manager
cargo test --test adapter_reactive
cargo check --all-targets --no-default-features
cargo check --all-targets --all-features
```

They cover typed/atomic construction failures, deterministic indexes,
adapter-family ownership, exact versus whole-account dependencies,
shared-emitter isolation, adapter-defined routing refresh, generation-safe
handler and repair IDs, job/resync partitioning, removal isolation, recovery,
and conservative shared-state eviction.
