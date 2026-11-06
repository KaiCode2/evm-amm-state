# Optional event-only canonical delivery

`CanonicalLogSource::SubscriptionOnly` is an opt-in strict mode of the existing
Alloy subscriber driver. The compatibility default remains unchanged. This mode
does not require a new full-block feed and does not make incomplete logs complete.

Attachment requires `SubscriberMode::PubSub`,
`SubscriberGapPolicy::FailClosed`, disabled subscriber reconnect, disabled log
block verification, disabled pending-transaction hydration and disabled previews.
The provider must also disable its own transport reconnect/subscription replay;
that lower-layer setting cannot be inspected through an opaque `Provider` value.
The normal-loop zero-request contract is conditional on that transport setup,
not something the driver can attest for arbitrary providers.
Pinned Alloy 1.6.3 attempts one reconnect even with `with_max_retries(0)`;
zero retries alone is insufficient. The connector must override `try_reconnect`
to reject locally before calling its transport, as well as prevent another
initial `connect` on the same capability. A new connection needs an explicit
cold restart. Count the local blocked callback separately from outbound requests.

Cold adoption builds one bounded address-superset subscription from all initial
and owner log interests. Wildcard addresses, non-live block ranges, non-header
block interests and an address count beyond the configured single-subscription
limit reject. The extra base filter lets the cache's existing exact fan-in
collapse source subscriptions; adapter and owner routing still occurs locally.
This can increase received log volume, which requires production-shaped
qualification. New or replacement source ownership requires an explicit cold
restart; this mode never silently subscribes and backfills it during delivery.

The driver accepts closure only under the existing source contract: no detected
unrepaired notification loss, combined with a later-block log on the sole
physical source, or the current header's bloom proving no matching logs. A later
header, an elapsed timer, or a successor on another independent source is not
closure. There is no fallback deadline, `getLogs` enrichment, staking-evidence
fetch or parent walk. Detected loss, reorg, removed/late conflicting logs or a
missing header parent fail closed. Pending assembly is bounded by 65 blocks and
the subscriber's configured record bound.

An unclosed block emits `WaitingForClosure { point, pending_block, ... }` and
does not advance the AMM snapshot. Consumers must fence current-head execution
while this state is present. A quiet stream can keep a bloom-positive block
waiting until another matching log arrives; hitting the assembly bound stops
the driver for explicit repair. These semantics do not establish bounded
latest-head reaction time or satisfy the full worker qualification by themselves.

Canonical `AmmStatePoint` currently represents `PostBlock` only. Publishing an
individual event prefix as post-block would be false provenance. Existing
preconfirmation snapshots are explicitly speculative and cannot replace
canonical inventory, nonce or accounting. Any event-prefix execution design
would need a separate state-position/readiness policy and is not implemented by
this change.

Complete event delivery also does not prove all execution cells. A fail-open
tape call can write silently; missing reducer evidence remains unknown. The
composition owner must prevent automatic resync or bootstrap from turning a
missing transition into normal-block RPC. Cold discovery and explicitly
authorized repair/lifecycle reads must be measured separately.

Focused tests use Alloy's public pubsub transport channels through the actual
subscriber and driver loop. They record cold chain/subscription requests, then
exercise empty-bloom advancement, a busy block held across elapsed deadlines
and new headers, later-log closure, malformed-notification failure and an actual
transport disconnect with a locally rejecting reconnect guard. The
normal delivery window has zero additional outbound requests. Separate tests
reject hidden-read configuration, reorg/gap/late logs and implicit catch-up.
This is deterministic component evidence, not whole-worker latency or live
provider qualification.

The final Rust 1.90.0 focused subscriber suite passed 56 tests with one ignored;
all four strict-mode regressions passed. All-target, all-feature Clippy passed
with warnings denied. The companion cache gap-policy suite passed 14 tests,
including lagged and malformed canonical notifications with zero recovery reads.
These use the exact locally qualified alpha archives; they do not prove registry
publication or a configured live provider's delivery guarantees.
