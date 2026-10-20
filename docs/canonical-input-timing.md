# Canonical input timing

`AmmCommitTiming::input_timing()` optionally exposes observational transport
receipt lineage collected by the subscriber driver. This is distinct from
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
associated history.

The ledger holds at most 64 blocks and 4096 log fingerprints per block. Capacity
pressure affects observability only. The delivered summary records assembly
release, preparation completion, optional reconciliation start, event counts,
first/last event receipt and header input. No new RPC or persisted clock is added.

The standalone workspace temporarily patches evm-fork-cache to exact commit
`cc0e000c575b8736e05e1d10642d92716506669f`, implementing the optional consumed receipt
API. Downstream workspaces must select that same API explicitly because Cargo
patches do not propagate from dependencies. No crate has been published by this
change. A reviewed released version can replace the integration pin later.

Focused regression verifies grace/reconciliation retains the original receipt,
never assigns observed timing to an unobserved fetched log, and keeps reorg hashes
and retired block history separate. Full release qualification is still required.
