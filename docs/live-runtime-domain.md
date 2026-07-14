# Live Runtime Domain Model

Status: Stage 1 complete
Date: 2026-07-10

This document describes the protocol-neutral vocabulary introduced before the
background AMM runtime itself. The actor, scheduler, immutable snapshots, and
critical delivery channels are implemented in later stages; these types freeze
the identities and correctness boundaries they must use.

## Identity and stale-work fences

`PoolKey` remains a logical protocol identity. Every accepted registration is
identified at runtime by `PoolInstanceId { key, generation }`. Adapter families
and discovery owners have the same generation fence through
`AdapterInstanceId` and `DiscoveryOwnerId`. `RuntimeWorkId` adds a fresh
`WorkId` for every attempt under one owner.

Generation, work, state-version, pool-revision, and observer-event counters use
checked increments. Exhaustion is an error; no counter wraps or silently
reuses an identity. An adapter family key canonically contains every protocol
served by that adapter, so a multi-protocol V3 adapter has one generation
domain rather than one per primary-protocol lookup.

## State provenance

`AmmStatePoint` currently represents post-block state only:

```rust,ignore
let point = AmmStatePoint::post_block(chain_id, block_number, block_hash);
let first_backfill_block = point.first_unapplied_block()?; // N + 1
```

`PoolStateRef` combines the pool instance, its quote-relevant
`PoolStateRevision`, and the complete state point. A future quote cache must use
that full provenance; it must not relabel a result across blocks merely because
the pool revision is unchanged.

`AmmStateVersion` identifies a coherent published snapshot. Version zero is the
initial snapshot. Later actor work advances it once per newly published
snapshot—not once per input log, provider response, or cache mutation.

## Lifecycle and provenance

`PoolLifecycle` enforces the operational transitions frozen in the master
plan. It is a sidecar to `PoolStatus`: scheduler/transport state lives in
`PoolRuntimeState`, while adapter/search health remains in `PoolStatus`.

Registration evidence is typed and canonicalized:

- stable manual/configuration evidence;
- state-query evidence with finalized or revalidate-on-reorg policy;
- factory-log evidence owned by a generation-scoped watcher.

`RegistrationEvidenceSet::reorg_action` removes a registration only when all
supporting evidence is orphaned, requests revalidation when nonfinal query
evidence remains, and preserves registrations backed by stable/finalized or
unaffected evidence.

## Critical changes versus observer events

`AmmChangeSet` is the deterministic, correctness-critical search input. It
requires an explicit `AmmStateVersion` and `AmmStatePoint`, contains at most one
final `AmmPoolChange` per `PoolInstanceId`, and canonicalizes both pool changes
and incidents. `AmmChangeImpact` independently describes state, quoteability,
and topology consequences.

`AmmRuntimeEvent` is the typed, monotonically sequenced observer vocabulary for
lifecycle, work, progress, registration, state commits, continuity incidents,
and health. `AmmRuntimeStatusSnapshot` is the recoverable latest-value view for
observers that start late or miss lossy event-stream entries. Unknown progress
totals remain `None`; a known total smaller than completed work is rejected.

Later stages will place `AmmChangeSet` on a bounded reliable channel and
`AmmRuntimeEvent` on a lossy observer channel. Reactive hooks remain diagnostic
integration points, not the state-to-search correctness channel.

## Compatibility sync report

`AmmSyncEngine::ingest_batch` now returns deterministic logical
`affected_pools`, typed `pool_changes`, and typed continuity `incidents` after
direct effects, authoritative resyncs, and lifecycle transitions finish.
Attribution follows actual applied diffs, including shared slot/account owners
and exact slot-scoped purges; an unchanged authoritative write or no-op purge
does not become a false state change. A partially effective multi-slot purge is
reported as unknown because the upstream diff exposes only an aggregate removal
count. Decode failures, failed/unexecutable repairs, and coverage gaps degrade
the relevant pools with unknown impact. Reorgs and missed block ranges
conservatively require a full refresh.

Those trust failures are persisted beyond the individual batch report. Failed
slot repairs recover only when their exact targets refresh; an untracked
Curve/Balancer pool requires its complete enumerable read set. Unknown impact,
account-target failure, reorg, gap, and coverage loss install an explicit-refresh
barrier, so an unrelated later slot repair cannot mark stale state ready. The
compatibility clearing path is an authoritative cold-start followed by registry
replacement with a non-degraded registration; later actor stages make that
transaction typed and generation-fenced.

The compatibility report deliberately uses logical `PoolKey`, not
`PoolInstanceId`, and does **not** contain an `AmmStateVersion` or
`AmmStatePoint`. A caller-provided reactive batch is not by itself proof of a
complete post-block commit boundary, and `evm-fork-cache` currently exposes but
does not emit `ReactiveReport::BlockCommitted`. The Stage 4 actor must establish
that fence before constructing an `AmmChangeSet`; compatibility ingest must not
fabricate one.

Successful and failed AMM hook payloads are available as the typed
`AmmReactiveSignal`, so integrations can route exact pool identities without
parsing debug labels.

## Stage 1 verification

The acceptance suites are:

```text
cargo test --test runtime_domain
cargo test --test adapter_sync_manager
cargo test --test adapter_reactive
```

They cover generation/retry fencing and overflow, post-block `N + 1` semantics,
checked lifecycle transitions, provenance-driven reorg actions, deterministic
change/event ordering, progress validation, direct-versus-resync attribution,
shared-emitter and shared-dependency behavior, degradation/recovery, gaps,
reorg rollback, and the absence of fabricated post-block commits.
