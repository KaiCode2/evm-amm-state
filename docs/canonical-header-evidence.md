# Canonical full-header evidence (unreleased)

`AmmStateCommit::canonical_header()` exposes `Option<&Arc<AmmCanonicalHeader>>`
through the existing reliable change subscription. `AmmCanonicalHeader::point()`
is the exact chain/number/hash and `header()` borrows the complete immutable Alloy
header, including its parent hash and logs bloom. This safety evidence is
independent of the optional `ingress-timing` diagnostic feature.

Only canonical ingestion supplies this evidence. `AmmCanonicalBatch` already
checks the header's advertised hash against its encoded contents; the runtime
checks the chain and applies that batch before publication. Commit construction
retains evidence only when its point matches both the snapshot and change set,
including in release builds. Lifecycle, prepared-state and repair publications
carry `None`, even when their state point equals the preceding block. A baseline
snapshot or latest-value watch cannot manufacture missed header history.

This preserves existing trust assumptions: a verified header hash is not an
independent proof of consensus, finality, the provider's honesty or the
completeness of all logs. A bloom can prove absence only for a correctly specified
predicate; a positive result is not proof of an event. No contract-specific
control topics or economic decisions are part of this API.

A consumer extending a sequence of negative-bloom proofs must use the reliable
commit stream and verify runtime identity, consecutive commit versions and block
numbers, exact chain/hash/state pairing and parent-hash linkage. It must stop or
rebuild that sequence on missing evidence, receiver loss/replacement, a version
or block gap, reorg/full-refresh incident, or a new runtime/baseline. In particular,
an event-free reorg may preserve coherent AMM state without requesting a full
refresh; its reorg incident still invalidates the consumer's prior branch proof.
A consumer may subsequently establish a new chain from a newly validated anchor.
No generic absence proof is granted automatically by the header accessor.

The existing bounded reliable queue reserves capacity before canonical state
mutation. The header is retained by the same immutable commit, so backpressure
cannot publish state while silently dropping its corresponding header. Closing
the receiver ends this continuity guarantee; resubscribing captures a new
baseline rather than replaying history. Consumers retaining old commits also
retain their headers, just as they already retain snapshots.

## Ownership and cost

`src/adapters/snapshot/canonical_header.rs` owns the data-only type and private
verified-parts constructor. `snapshot.rs` pairs it with commits;
`live_runtime.rs` constructs it after existing batch validation and propagates
it on canonical publication. Callers use the normal `adapters` facade. The leaf
has only Alloy header and state-point dependencies, with no provider or protocol
policy dependency.

This reuses the existing subscription, header validation, canonical batch and
reliable queue. It adds one shared header allocation per canonical batch and one
header clone for the existing reactive input. It adds no RPC, subscription,
background task, database/filesystem operation, timer or durable schema.
Allocation/copy overhead remains real; no latency improvement is claimed for
this transport change.

`tests/live_runtime/canonical_header.rs` covers exact full-header/state pairing,
empty blocks, altered-bloom hash rejection, capacity-one queue continuity,
lifecycle/prepared publications, replacement subscriptions and reorg identity.
A private constructor regression also rejects otherwise valid headers with the
wrong chain, height or hash. Existing canonical-envelope and runtime tests remain
applicable. The new API
has not been published in the alpha.6 crate.
