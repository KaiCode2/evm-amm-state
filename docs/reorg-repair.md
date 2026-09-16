# Canonical reorg repair

The live runtime separates canonical identity replacement from pool-state repair.
A complete canonical batch still updates the block environment and emits its
`Reorg` incident. Consumers must continue to invalidate orphaned fees, references,
inventory, nonce, receipt, and execution evidence using that incident.

## Event-free replacements

The actor retains at most 64 published block identities and whether each complete
batch contained tracked logs. It can avoid a full pool refresh only when:

- the incoming block connects exactly to a retained common ancestor;
- every displaced block and the incoming replacement contain no tracked logs;
- the reactive rollback reports exactly those displaced identities, only header
  inputs, no storage rollback/purge, and no canceled resync;
- no imported state or noncanonical publication has invalidated this proof.

Cold-start/hydration installation and lifecycle publications reset the proof at
their exact block. Dropping that anchor, unknown/deep ancestry, coverage gaps,
events on either branch, or actual state effects retains conservative repair.
Restart begins with only its authoritative baseline. The synchronous compatibility
engine has no actor proof and retains conservative reorg behavior.

This optimization performs no provider read. It does not infer event absence
from a short log response: input must already satisfy complete canonical delivery.

## Superseded repair artifacts

Already-degraded, active required repairs may be superseded only by typed stale
baseline evidence or a subscriber reconciliation response with a different hash
at the exact requested height. No conflicting provider hash is installed as state.
The runtime discards the prepared artifact, emits `WorkSuperseded`, retains the
repair intent, and keeps the pool degraded. If the runtime still owns the old
target, retry waits for a different normal canonical publication. If canonical
delivery already advanced, replacement work can be queued immediately.

Consumers should retire the superseded work identifier without treating it as
`WorkFailed`. Existing degradation/liveness deadlines must still bound recovery;
supersession must not reset those deadlines. Other provider, ownership, malformed
identity, decode, and cold-start errors remain failures. Successful fresh repair
still requires the normal exact-generation, exact-block, storage and account checks.

## Subscription identity

Pool refresh compares typed event emitters, topic sets, and routing rules under
the same validated pool/adapter owner. Debug representations of Alloy filters
contain set ordering and internal bloom caches; they are not subscription identity.
Unchanged source definitions do not start an unnecessary subscriber replacement.

## Ownership

- `live_runtime/reorg.rs`: bounded publication-history proof, owned by the actor.
- `sync_manager/reorg.rs`: validates that proof against actual reactive rollback.
- `sync_manager/subscription.rs`: pure typed pool event-source comparison.
- `live_runtime/repair_supersession.rs`: typed obsolete-artifact disposition and
  retained repair intent; cannot adopt a provider block or publish prepared state.
- `tests/live_runtime/reorg.rs`: canonical actor and real background worker
  regressions; no network credentials or broadcast.

## Validation scope

The focused library/runtime suite covers event-free V2 and Ethereum-shaped V3
replacements, events on either branch, multi-block rollback, displaced imported
state, bounded history, and required repair overtaken by canonical progress.
The optimized CI regression prints actor round-trip p50/p95/p99/max across 256
empty replacements and asserts no repair jobs are queued. These timings cover
canonical AMM publication only, not quote-to-proposal latency or provider RTT.
No new provider call or fallback is introduced by the proof or disposition.
