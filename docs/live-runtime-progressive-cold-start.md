# Progressive Cold-Start Runtime

Status: Stage 6 complete
Date: 2026-07-11

The `live-runtime` feature can hydrate pools in bounded background work without
blocking canonical input or waiting for an entire bootstrap batch. Each pool is
generation-reserved when its work is accepted and becomes visible in an
immutable `AmmStateSnapshot` as soon as its own verified state commits.

## Public workflow

```rust,ignore
let worker = runtime
    .attach_cold_start_worker(
        provider,
        AmmColdStartWorkerConfig::default()
            .with_max_concurrency(32)
            .with_storage_batch_config(StorageBatchConfig::new(150, 8))
            .with_storage_fetch_strategy(StorageFetchStrategy::BulkCall(
                BulkCallConfig {
                    max_slots_per_call: 25_000,
                    max_slots_per_request: 25_000,
                    max_request_bytes: 2_400_000,
                    max_concurrent_calls: 4,
                    ..BulkCallConfig::default()
                },
            )),
    )
    .await?;

let scheduled = runtime
    .queue_cold_start(registrations, AmmColdStartOptions::default())
    .await?;

// `scheduled` returns before provider hydration finishes. Consumers recover
// current progress from `subscribe_status()` and observe ordered deltas through
// `subscribe_events()` / `subscribe_changes()`.
```

The point-read batch config and storage fetch strategy are propagated through
cold start, connector/factory discovery, repair, and deferred warming. Defaults
remain conservative; applications should tune pool concurrency and serialized
request bytes from measurements of their provider set rather than treating a
slot count as a universal RPC limit.

Discovery and dynamic ownership use the same actor and worker:

```rust,ignore
let owner = runtime
    .add_factory_watcher(AmmFactoryWatcherRegistration::new(
        DiscoveryOwnerKey::new("mainnet-v2"),
        adapter_instance,
        discovery,
    ))
    .await?;

runtime
    .queue_token_discovery(
        owner.clone(),
        TokenEdgeDiscoveryRequest::new(token, connectors),
        AmmDiscoveryOptions::default(),
    )
    .await?;

// Remove pools first when this watcher is their only registration evidence.
runtime.remove_factory_watcher(owner).await?;
```

`queue_deferred` accepts `ColdStart`, `VerifySlots`, and their mixed form; a
mixed request refreshes first and then applies the exact slot patch as a second
same-generation revision. Scheduler-owned `Repair(ColdStart)` and
`Repair(VerifySlots)` are also supported. Account, trace-discovery, and probe
root planner phases remain typed rejections until they gain a verifiable
prepared-artifact representation.

Worker queue capacity is measured in individual pool jobs, not submitted
batches. Admission is atomic: a batch that does not fit reserves no pool
generation. Provider quanta are capped by `max_concurrency`. Work classes are
priority ordered, FIFO within a class, and use a bounded priority burst so a
continuous high-priority stream cannot permanently starve background work.
A rotating aging cursor shares those forced quanta across every waiting lower
class rather than repeatedly favoring the nearest class.

Provider-backed workers require a multi-thread Tokio runtime. The cache actor
may still run on its required `LocalSet`; synchronous fetch bridges use
`block_in_place` so the Tokio scheduler can replace the blocked worker thread
while the RPC future is driven.

Dropping the worker handle does not stop actor-owned work. `worker.shutdown()`
stops it explicitly; `runtime.shutdown()` also wakes the dispatcher, cancels or
drains queued/in-flight quanta, and reports `Stopped` only after child tasks have
finished.

## Provenance and commit boundary

Every worker request is pinned with EIP-1898
`{ blockHash, requireCanonical: true }`.

- one-shot storage programs and resumable verify/probe rounds run outside the
  cache actor;
- adapter code-seed claims are checked with root-only `eth_getProof` at the same
  hash;
- workers retain immutable queue-time state plus a private storage overlay;
- later planner rounds see earlier verified values without mutating canonical
  state;
- the final artifact carries the exact pool generation, work ID, block hash,
  declared storage values, verified account fields, and runtime code;
- the actor rejects stale work, stale generations, stale hashes, missing or
  unexpected declared slots/accounts, duplicate identities, code mismatches,
  and deliberate local-code conflicts before publication.

The account patch installs RPC-returned balance/nonce and verified runtime code
directly into both cache layers. A snapshot therefore never exposes a pool whose
template code is still marked pending verification.

This is a trusted-RPC boundary, not local Merkle proof verification. The worker
pins `eth_getProof` to the exact canonical hash, validates response identities,
hashes returned runtime bytecode, and compares that hash with the RPC-returned
`codeHash`; it does not retain or verify Merkle proof nodes against the block
header state root. Applications that require Byzantine-provider resistance must
place a proof-verifying provider in front of this API or add local state-root
verification before accepting the prepared account patch.

`AmmPreparedPoolState::new` and `commit_prepared_pool` remain a trusted
compatibility seam for external fetch systems. They validate shape,
dependencies, and current baseline, but cannot authenticate an arbitrary
caller's RPC response. Normal applications should use `queue_cold_start`, whose
work/generation claim is created internally.

## Lifecycle and progress

Accepted work follows this observable sequence:

```text
Discovered -> Queued -> Hydrating -> Searchable [-> Live with a subscriber]
```

Each quantum emits sequenced `ColdStartRoundStarted`,
`ColdStartRoundCompleted`, and `WorkProgress` events. `AmmRuntimeStatusSnapshot`
is updated before the corresponding observer event and contains the recoverable
latest lifecycle, active progress, and per-class queue depths. While the runtime
remains open, every resolved work attempt emits exactly one of `WorkCompleted`,
`WorkFailed`, or `WorkCancelled`. Runtime shutdown closes observers after
draining the worker; it does not synthesize terminal events for work abandoned
as part of that teardown.

Cancellation is exact-work and exact-generation scoped. A late result cannot
mutate the replacement registration. Cancelling a reserved pool tombstones its
generation; retry receives the checked successor generation.

If canonical state advances while a worker is fetching, the old-hash result is
rejected without mutation and the attempt becomes `Failed`. Discovery-owned
cold starts, discovery requests, repairs, and deferred warmups retain their
exact-generation intent and are re-admitted at the new canonical point. An
explicit caller-owned cold start remains caller-retryable.

## Discovery, repair, and dynamic ownership

Stage 6 runs connector queries and factory-event discoveries through the same
bounded worker as cold starts. Discovery results enter a bounded, deduplicated
handoff queue and each pool publishes independently; terminal jobs release
their admission lease before actor handoff, so a capacity-one worker can admit
the successor without deadlocking itself.

The live canonical path applies direct effects without synchronous provider I/O.
Required repairs fence the exact pool generation degraded, commit the canonical
block, then schedule hash-pinned repair work in the background. Lazy cold starts
publish with explicitly deferred slots and immediately stream a same-generation
revision when those slots arrive. Required repair supersedes optional warming.

Adapters and factory watchers are generation-scoped dynamic owners. Removing an
owner cancels its queued or in-flight work promptly; removing and re-adding the
same adapter family cannot allow an old result to publish under the replacement
generation. Pending and published registrations retain a deduplicated evidence
set plus owner-keyed query revalidation recipes. Removing a watcher is rejected
when it is the final evidence source for an active pool; otherwise only that
owner's evidence and recipes are revoked. On a reorg, a query-backed pool stays
degraded but usable while its hash-pinned revalidation is queued or in flight,
and is removed only after every supporting canonical query rejects it.

Interest-changing lifecycle operations stage and reconcile subscriber epochs,
reserve the critical publication slot, and require subscriber commit
acknowledgement before mutating actor topology. The public lifecycle is proven
from connector discovery through pool publication, exact pool removal, watcher
removal, and adapter removal.

## Resumable fallback

Metadata-complete pools use the existing one-shot storage program as one
quantum. Other supported pools use their `AdapterColdStartPlanner` one round at
a time. The worker keeps a compact transcript and private overlay, then
reconstructs the non-`Send` pure planner for each quantum. This lets long jobs
yield to canonical, repair, or focused work while retaining planner-equivalent
results.

The prepared fallback supports exact slot `verify` and `probe` phases. Planner
`accounts`, call-trace `discover`, and `probe_roots` phases fail with a typed
unsupported-phase error rather than silently omitting work; factory and token
edge discovery plus repair/deferred slot verification use dedicated scheduled
job types.

## Verification

```text
cargo test --all-features --test live_runtime --test cold_start_worker
cargo test --all-features --test cold_start_priority
cargo test --all-features --test adapter_sync_manager
cargo clippy --all-targets --all-features -- -D warnings
cargo check --all-targets --no-default-features
cargo check --all-targets --all-features
cargo bench --features live-runtime --bench live_runtime_actor
```

Focused coverage includes non-blocking queue return, independent pool
publication, exact hash capture for storage and account proof requests,
metadata merge parity, stale-baseline rejection, cancellation and late-result
rejection, retry generation fencing, canonical responsiveness during blocked
RPC, job-level capacity, priority/fairness, runtime-owned shutdown, and
multi-round overlay/probe equivalence. Stage 6 coverage additionally proves
capacity-one discovery-to-hydration handoff, mixed deferred refresh/slot
chaining, automatic repair, factory-log orphan removal, and durable query
revalidation while the provider response is blocked.

The 2026-07-11 M1 Pro offline Criterion run measured actor queue return at
`5.176us..5.890us`, well below the `<10ms` control-return gate. The same run
measured actor creation at `4.994us..5.166us` and non-blocking control enqueue
at `101.5ns..104.8ns`. Provider time is excluded from all three measurements.
