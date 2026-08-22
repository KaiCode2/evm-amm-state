# Cache-Owner Runtime

Status: Stage 4 complete
Date: 2026-07-10

The `live-runtime` feature adds a single-writer actor around `EvmCache` and
`AmmSyncEngine`. It publishes immutable, versioned snapshots while callers and
search workers hold only cheap `Send + Sync` handles and `Arc` publications.

## Startup boundary

`EvmCache` is thread-local, so `AmmRuntime::spawn` must run inside a Tokio
`LocalSet`. Startup requires `AmmRuntimeBaseline`, constructed from a hash-sealed
full RPC header. Spawn verifies chain ID, block number, base fee, beneficiary,
prevrandao, gas limit, and timestamp before publishing version zero.

```rust,ignore
let baseline = AmmRuntimeBaseline::from_verified_header(chain_id, header)?;
let runtime = LocalSet::new()
    .run_until(async move {
        AmmRuntime::spawn(cache, registry, baseline, AmmRuntimeConfig::default())
    })
    .await?;
```

The returned `AmmRuntimeHandle`, `AmmStateSnapshot`, and `AmmStateCommit` are
`Send + Sync`. A snapshot carries the state version, post-block number/hash,
subscriber interest revision, immutable registry/ownership view, pool
revisions, and an `Arc<EvmSnapshot>` suitable for independent worker overlays.

## Delivery and recovery surfaces

- `subscribe_changes()` atomically returns a current snapshot plus the one
  correctness-critical bounded commit receiver. Every first commit is strictly
  newer than that baseline and pairs the exact snapshot/change set. Canonical
  event commits also expose `AmmCommitTiming`: process-local monotonic source
  ingress plus decode, order, transition, and commit offsets. These are not
  wall-clock timestamps. Subscriber ingress survives provider-backed
  reconciliation, while topology/cold-start/repair commits return `None`
  instead of inventing event provenance.
- `subscribe_snapshots()` and `subscribe_status()` are recoverable latest-value
  watches for late consumers.
- `subscribe_events()` is a lossy observer stream. Lag is explicit; actor exit
  closes observers even while a cloned runtime handle remains alive.
- `try_ingest_batch`, `try_install_prepared_pools`, and `try_remove_pool` return
  independent command tickets or immediate typed backpressure.

Canonical input and control use separate bounded queues. Canonical work is
preferred, but after 16 continuously-ready canonical batches one ready control
command must run. In attached mode only the driver can submit canonical input.
During a lifecycle handshake the actor continues servicing the driver's
already-in-flight canonical delivery until the driver acknowledges its pause;
that acknowledgement is the topology transaction's delivery fence.

## Flashblock previews

`AmmRuntimeHandle::ingest_preconfirmation` accepts only batches whose records
carry `ChainStatus::Preconfirmed` and `DeliveryScope::Preconfirmed`. An attached
`AlloySubscriber` forwards those batches through the same owner driver and AMM
handlers used for canonical logs. The runtime publishes the result separately as
`AmmPreconfirmedSnapshot` via `latest_preconfirmation()` and
`subscribe_preconfirmations()`.

A preview names its canonical base version/point, subscriber interest revision,
exact `FlashblockRef` (including provider provenance), immutable cache state,
registry topology, and quote-relevant pool changes. It is deliberately not an
`AmmStateCommit`: canonical version, pool revisions, lifecycle, health, repair
ownership, and the reliable canonical change stream do not advance. Consumers
must treat the outer `Arc` as short-lived simulation input rather than confirmed
state. The runtime installs its verified full-header startup point as the
reactive canonical baseline, and accepts only a preview whose block number and
parent hash identify that baseline's exact child. Canonical advancement moves
the baseline before another preview can be accepted.

When the subscriber batch carries typed source provenance, the published
snapshot also exposes `AmmPreconfirmationTiming`: the exact process-local
monotonic ingress and elapsed time through preview publication. This is
optional, provider-free metadata. Missing timing remains `None` rather than
being inferred at the runtime watch boundary, and the value has no ordering,
identity, canonical-state, or execution authority.

Each cumulative Flashblock replaces the previous overlay. Canonical progress
first restores the saved canonical cache and clears the preview watch; subscriber
trust loss, coupled-stream termination, explicit invalidation, and shutdown do
the same. The actor continues to defer provider I/O, so it publishes a preview
only when the existing event-sourced handlers leave every affected pool ready
for immediate simulation. A required full refresh, pending or failed repair,
unresolved reactive resync, unknown pool impact, or non-ready applied quality
rejects the complete speculative branch. Rejection restores canonical state,
clears the preview watch, and cannot degrade canonical registrations or acquire
canonical repair ownership. Callers that need pending-tag repair may use the
synchronous `AmmSyncEngine` path, but that repaired state is not silently mixed
into the actor's no-I/O fast path.

Representative quote manifests add a second readiness gate. They are learned
and proven offline against the canonical cache before subscriber attachment.
Every preview replays affected manifests against its immutable snapshot; a
missing account, code body, slot, or block hash rejects the branch and extends
the bounded manifest. The actor performs no provider read. At the next exact
canonical state point, missing quote-only slots are queued through the existing
hash-pinned cold-start worker and installed only if the pool generation and
baseline still match. A runtime-code hash change invalidates every dependent
manifest instead of assuming the old storage layout remains valid.

## Complete canonical blocks

`AmmCanonicalBatch` is not an arbitrary event vector. It owns a sealed full
header and reconciled logs for exactly that block and interest revision. Its
constructor rejects cross-chain/context records, removed/non-log input,
intrinsic block mismatch, incomplete transaction/log identity, duplicates, and
an invalid header seal.

The actor prepends the full header before synchronous ingest. Consequently a
zero-log block still advances the reactive journal and complete EVM block
context. Same-height replacement headers and parent discontinuities are handed
to the upstream reorg journal; coherent replacements publish a new state
version and typed reorg incident instead of requiring process restart.

Any synchronous error after mutation stops publication and moves health to
`Untrusted`. The last coherent snapshot remains available. Stage 5's
authoritative scheduler owns reconstruction after partial failed mutation.

## Alloy subscriber driver

`attach_alloy_subscriber` installs an Alloy-specific driver in `Auto` or
`PubSub` mode. It always installs a header interest and uses upstream monotonic
owner epochs:

1. pause canonical delivery;
2. stage exact generation-scoped handler interests at post-block `N`;
3. subscribe before backfill and reconcile through the actor's exact hash;
4. verify the previewed and committed pool generations still match;
5. commit actor routing/state;
6. activate subscriber owners and advance the shared interest revision;
7. publish snapshot/change/events and resume delivery.

Removal is prepare, pause/fence, actor removal, exact-epoch finalization, then
publication/resume. Before publication, failure aborts the subscriber stage.
After actor mutation, an impossible or disconnected acknowledgement fences the
runtime as `Untrusted` rather than publishing a half-transaction. A driver that
fails, reaches end-of-stream, or is stopped independently likewise marks the
attached runtime untrusted; runtime shutdown itself does not wait on a wedged
driver task.

### Global canonical completeness

The driver used to re-fetch every block's logs with a hash-pinned `eth_getLogs`,
on the reasoning that it could not trust stream arrival order. That conflated two
concerns. *Ordering* never needed a network call: subscription logs carry their
block identity, transaction index, and global log index, so ordering is restored
locally. *Completeness* was the real gap — and the concrete mechanism was that
`alloy-pubsub` discarded a lagged or undecodable notification silently, leaving a
punctured stream indistinguishable from a whole one.

`evm-fork-cache` now surfaces that loss and repairs it, and attests the result
through `ChainControl::LogCoverage`: no notification loss went unhealed at or
below the named block. The attestation is a *negative* guarantee by construction,
because no source can prove from its own log stream that every matching log
arrived — a filter that matched nothing for a hundred blocks looks exactly like
one whose notifications were dropped.

The driver combines that attestation with its own ordering evidence. A block is
submitted from the subscription once three things hold:

1. the subscriber has attested log coverage at or above it,
2. its header has arrived, and
3. its log set is *provably* closed, by one of exactly two proofs.

The two proofs, and why only these two:

- **A log delivered for a strictly later block.** A single log subscription
  delivers in canonical order, so a log for block N+1 puts block N behind the
  write head and nothing further can arrive for it.
- **The block's `logsBloom` excludes every registered interest.** A bloom admits
  false positives and never false negatives, so exclusion is a positive proof
  that no matching log exists to wait for. This is the common case by a wide
  margin — a pool trades on a small fraction of blocks — and it is what keeps a
  quiet pool off the provider entirely.

Anything else reconciles. In particular a **header** for a later block is not a
proof: `newHeads` and `logs` are independent subscriptions, so a header
establishes only that the header stream advanced, never that an earlier block's
logs have been delivered. Nor is a timer. An earlier revision of this driver
accepted both, which published blocks carrying incomplete log sets and dropped
the late-arriving logs with no counter moving — the same silent loss this work
exists to remove, one layer up. The straggler window remains, but it now only
bounds how long the driver waits for a proof before falling back to a hash-pinned
reconciliation.

The successor rule is the steady-state path; the grace window covers a quiet
tail. `AmmSubscriberDriverStats` reports which rule sealed each block, so a
deployment can tell whether the window is doing work the successor rule should be
doing.

**Correctness does not depend on the stream having been whole.** Any block that
is not attested still takes the reconciliation path unchanged: a detected gap, a
reconnect, or a block the parent-lineage walk had to fill during a reorg. A reorg
additionally discards buffered logs and the attestation outright, because the
watermark described the branch that was canonical when it was issued. The fetch
stopped being unconditional and became exact.

Reconciliation itself is unchanged when it runs. Per-pool filters are safely
broadened and combined: addresses are chunked (256 by default), topic-zero sets
are unioned, and indexed-topic constraints are dropped so filter cross-products
cannot create false negatives. Pool handlers recheck their exact matchers
locally.

`CanonicalLogSource::Reconcile` restores the previous behaviour in one line.
Against an `evm-fork-cache` that does not attest log coverage, no block is ever
attested, so every block reconciles and behaviour is unchanged rather than
unsound.

Stage 4's dynamic installation seam accepts only a pool already coherent at the
actor's current point. `Ready` metadata alone is insufficient: all declared
exact slots must exist in the cache, and whole-account dependencies require the
stronger prepared-state proof introduced by the progressive Stage 5 scheduler.

`AmmRuntimeHandle::shutdown` becomes ready only after the actor and its
actor-owned `EvmCache` have been dropped. This is the durability boundary for
applications that flush the cache and then seal an immutable warm generation;
returning before the cache's drop flush completes would make a manifest commit
racy.

## Verification

Primary gates:

```text
cargo test --features live-runtime --test live_runtime --test snapshot
cargo test --test adapter_sync_manager
cargo clippy --features live-runtime --all-targets -- -D warnings
cargo check --all-targets --no-default-features
cargo check --all-targets --all-features
cargo bench --features live-runtime --bench live_runtime_actor
```

Focused coverage includes immutable overlay fan-out, complete zero-log blocks,
snapshot/change ordering, degradation coherence, explicit observer lag/closure,
bidirectional queue fairness, shutdown under critical backpressure, exact
add/remove generations, missing prepared state, subscriber source failure,
same-height reorg replacement, and failed-batch publication fencing.
