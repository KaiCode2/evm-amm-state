# Canonical input timing

With the default-off `ingress-timing` Cargo feature enabled,
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

The standalone workspace resolves exact registry `evm-fork-cache = "=0.4.1"`.
That published archive identifies commit
`8726fe3f2d6b653a912e9f4af84a7f113d1db5da` and SHA256
`d5be321d4ab546df56ef8f765ae0401f8352c44be4a6c763ac1efea277b0ce7c`.
The temporary Git patch has been removed; package validation uses the real
registry dependency. AMM 0.3.1-alpha.6 uses that verified registry dependency; release validation
includes the full feature, package, benchmark and canonical live gates.

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

## Optional ingress timing feature

The `ingress-timing` Cargo feature is disabled by default and forwards only
`evm-fork-cache/ingress-timing`. Without it the new public input timing DTO,
accessors, configuration flag, inline metadata, ledger and clockwork are absent.
Legacy `AmmCommitTiming` methods and timing behavior remain available unchanged.
Enabling the Cargo feature still requires `with_input_timing(true)` on the driver
for collection. An application-owned cache receipt lookup is additionally needed
for transport-receipt coverage; without a lookup, driver/assembly clocks can be
collected while consumed receipt origins remain explicitly unknown.

CI exercises default/no-default/enabled configurations, verifies the default
resolved graph does not enable either crate's ingress feature, and checks API
absence and original commit-record layout with timing disabled. Enabled tests
retain capacity, duplicate, content-conflict and exact-input provenance coverage.
No exporter, subscriber, global setup or runtime dependency is introduced.
