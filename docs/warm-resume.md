# Registration warm resume

Stage 9 adds a state-owned sidecar for immutable pool configuration and reusable
read-set hints. Generic account/storage state remains owned by
`evm-fork-cache`; the sidecar never serializes runtime lineage, generations,
state versions, work queues, subscriber state, or canonical EVM snapshots.

## Capture and restore

```rust,ignore
let snapshot = runtime.latest_snapshot();
let archive = AmmRegistrationArchive::capture(
    snapshot.point().chain_id(),
    snapshot.registry(),
)?;
archive.save(".cache/amm-registrations.bin")?;

let restored = AmmRegistrationArchive::load(
    ".cache/amm-registrations.bin",
    current_chain_id,
)?;
runtime
    .queue_cold_start(
        restored.into_registrations(),
        AmmColdStartOptions::default(),
    )
    .await?;
```

All queueable registrations load as `PoolStatus::Pending`. Persisted Curve and
Balancer read sets, V3/Solidly layouts, token metadata, event sources, and code
seeds are hints that make current-baseline verification cheaper; they never make
a pool searchable by themselves. The runtime cold-start worker re-verifies them
at its current hash-pinned baseline and publishes each pool independently.

Disabled and unsupported registrations retain those statuses. Opaque custom
metadata is rejected explicitly because the crate cannot serialize an
`Arc<dyn Any>` without a caller-supplied codec.

## File contract

The file uses an AMM-specific magic header, an explicit little-endian schema
version, and a private deterministic JSON payload. Records and unordered hint
sets are normalized before encoding while semantic token/coin order is
preserved. Loads validate the chain namespace, schema, pool uniqueness,
protocol/metadata pairing, and defensive size limits.

Saving encodes fully in memory, writes a unique temporary file in the target
directory, syncs it, atomically renames it over the destination, and syncs the
parent directory on a best-effort basis. A failed pre-rename write leaves the
previous archive intact. Applications should run save/load filesystem work on a
blocking worker rather than the AMM cache actor or UI task.

## Application-level crash consistency

`AmmRegistrationArchive::save` is atomic for the registration file; it does not
atomically coordinate the separate `evm-fork-cache` files or a caller-owned
canonical checkpoint. Applications that restore pools as ready must treat those
artifacts as one generation:

1. create a private staging generation and seed it from the last committed one;
2. point the mutable `EvmCache` at that staging directory;
3. stop canonical and cold-start producers, flush the actor-owned cache, and
   save the archive captured from the same runtime snapshot;
4. shut down the runtime and wait until actor-owned cache resources have dropped;
5. sync every generation file and directory, rename the staging directory to an
   immutable generation, then sync the generations directory;
6. atomically replace and sync a small manifest containing the generation name
   and `{ chain_id, block_number, block_hash }` checkpoint **last**.

Startup trusts only the manifest-selected complete generation and still verifies
the checkpoint hash against the canonical provider. Orphan staging directories
and unreferenced sealed generations are not resume candidates. If verification
fails, discard persisted EVM state and cold-start at a new verified baseline;
registration metadata may remain only as pending hydration hints.

This ordering makes the manifest the commit record. A process death before its
replacement leaves the previous generation selected, even if the new cache or
archive had already been fully written. Use one writer per chain namespace;
multi-process locking is an application concern.

The envelope checksum detects accidental corruption and torn/manual edits; it
does not authenticate the archive. Store it only in a caller-controlled local
cache directory. An attacker who can replace the file can recompute the checksum
and substitute token identities, factories, layouts, or read-set hints.

Persisted immutable metadata also relies on the adapter's immutable-contract
assumption. If a tracked deployment can upgrade any persisted identity or layout,
the application must invalidate its sidecar when that upgrade happens.
Current-baseline cold start refreshes state and verifies readiness, but it cannot
prove that caller-classified immutable metadata stayed fixed.

## Compatibility boundary

The first schema supports every shipped built-in pool/metadata form. Unknown
magic, newer versions, corrupt/truncated payloads, chain mismatches, duplicate
pools, and inconsistent key/metadata combinations are typed failures. A source
block is deliberately not restored by the archive API itself: read-set hints may
survive blocks, but readiness is established only at the new runtime point. An
application may separately restore a hash-certified cache generation at its
verified checkpoint and replay canonical blocks before publishing readiness, as
described above.
