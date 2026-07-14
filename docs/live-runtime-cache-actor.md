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
  newer than that baseline and pairs the exact snapshot/change set.
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

For global canonical completeness, the driver does not trust stream arrival
order. On each header it issues hash-pinned `eth_getLogs` reconciliation,
deduplicates exact log identities, and only then submits the block. Per-pool
filters are safely broadened and combined: addresses are chunked (256 by
default), topic-zero sets are unioned, and indexed-topic constraints are
dropped for reconciliation so filter cross-products cannot create false
negatives. Pool handlers recheck their exact matchers locally.

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
