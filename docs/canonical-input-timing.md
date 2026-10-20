# Canonical input timing

`AmmCommitTiming::input_timing()` optionally exposes observational transport
receipt lineage collected by the subscriber driver when explicitly enabled with
`AmmSubscriberDriverConfig::with_input_timing(true)`. The default is disabled. This is distinct from
`source_ingress` and does not reset or replace existing execution clocks,
assembly deadlines, canonicality decisions, or retry policy.

A bounded ledger retains the first driver input for each exact block number/hash
across assembly grace expiry and reconciliation. Each accepted log identity is
matched to the consumed subscriber timing and its content fingerprint (address,
topics, data, transaction position and removed status). Reconciliation does not
manufacture clocks for fetched logs that were never observed. Missing/conflicting
receipt evidence increments `missing_events`; the earliest available event is
incomplete when that count is nonzero. Header clocks remain separate and are
never claimed as a pool event receipt. Gap/reset and block retirement clear the
associated history. Retirement and capacity refusal retain a monotonic height
watermark until reset, preventing late duplicates from creating shorter new
roots once capacity becomes available. Conflicting observed content poisons
that log identity rather than borrowing either receipt.

The ledger holds at most 64 blocks and 4096 log fingerprints per block. Capacity
pressure affects observability only. The delivered summary records assembly
release, preparation completion, optional reconciliation start, event counts,
first/last event receipt and header input. No new RPC or persisted clock is added.

The standalone workspace temporarily patches evm-fork-cache to exact commit
`e8c8dcb198c267680e20bc21a6c5cf8e7eb4da13`, implementing the optional consumed receipt
API. Downstream workspaces must select that same API explicitly because Cargo
patches do not propagate from dependencies. No crate has been published by this
change. A reviewed released version can replace the integration pin later.

Focused regression verifies grace/reconciliation retains the original receipt,
never assigns observed timing to an unobserved fetched log, and keeps reorg hashes
and retired block history separate. Full release qualification is still required.

## Instrumentation boundary

This is an optional data-only observation API, not an exporter. Public additions
are `AmmCanonicalInputTiming`, `AmmCommitTiming::input_timing`, and the driver
configuration builder/getter. Existing type constructors and execution timing
accessors remain unchanged. Enabling the builder creates a bounded ledger; while
disabled there is no ledger allocation, log fingerprinting, receipt lookup or new
assembly clock read. Existing execution timing remains active as before.

No bot/Prometheus labels, exporter dependencies, background telemetry task, or
provider operation is introduced. Applications choose how to consume the immutable
summary through ordinary tracing, metrics middleware or their own in-memory
observer. The upstream cache supplies a backend-neutral callback example. Tower
RPC request layers alone cannot observe unsolicited subscription message receipt;
these hooks preserve the metadata after Alloy's raw notification dequeue and
through this crate's private assembly/reconciliation boundary.

The structs use process-local monotonic clocks and do not define a wire format,
durable schema or execution authority. This is an additive API change on the
current prerelease line. After the reviewed cache 0.4.1 is published, replace its
integration patch with an exact registry dependency, verify the package against
that real registry release and prepare the next available AMM alpha. Publication
remains separately authorized; no package verification assertion is weakened.
