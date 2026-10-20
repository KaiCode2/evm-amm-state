# Canonical reconciliation comparison and bloom audit

This optional diagnostic compares the driver's consumed subscription log buffer
with the result of an **already scheduled** exact-hash reconciliation. Enable the
existing default-off `ingress-timing` Cargo feature and configure
`AmmSubscriberDriverConfig::with_reconciliation_observation(true)`. It is
independent of `with_input_timing`; both runtime options default to false.
`handle.stats().reconciliation_observation()` returns `None` when disabled and
otherwise an `adapters::ReconciliationObservationStats` snapshot.

## Measurement boundary

The cutoff is **driver-consumed Subscription records at fetch start**, not raw
WebSocket receipt time. The driver does not consume queued subscriber records
while awaiting this existing fetch. A log already physically received but still
queued therefore can be absent here. Missing means absent at this cutoff; it
never means permanently lost, dropped by a provider, or proof of network delay.
Only reconciled blocks are sampled. Stream-sealed blocks are not an independent
completeness census, and their absence from this denominator is intentional.

The four cumulative block outcomes are mutually exclusive per reconciliation:

- `equal_blocks`: identical unique interested-log sets, including both empty.
- `missing_blocks`: one or more reference logs absent from the consumed buffer,
  without conflicting contents or unexpected consumed logs.
- `conflicting_blocks`: contradictory consumed duplicates, different contents at
  the same global index, or consumed indices absent from the reference.
- `unavailable_blocks`: failed/malformed reference, missing header observation,
  reset/gap/reorg history, diagnostic capacity refusal, malformed input, or
  mixed backfill/poll records whose upstream deduplication obscures WS coverage.

`reference_logs`, `consumed_logs`, `missing_logs`, `unexpected_logs` and
`conflicting_logs` count unique global indices only for comparable outcomes.
Missing logs may also occur in a conflicting block. Conflicting and unexpected
log categories can overlap for a contradictory duplicate absent from reference;
never add them as a unique-log census. HTTP/provider errors contribute only an
unavailable outcome, never zero reference logs or missing logs. Outcomes describe
comparison, not successful publication: canonical validation and actor acceptance
remain independent, unchanged gates. Relaxed atomic snapshots can straddle a
counter update. Counters reset with driver lifetime.

The reference is the merged existing exact-hash `eth_getLogs` result before
supplemental staking evidence. Its address/topic union is checked, then the same
registered `LogInterest::matches` selectors used by the subscriber narrow it to
the driver-deliverable domain. Overlapping filters and duplicate notifications
are deduplicated. A block-number/hash/global-index key plus transaction identity,
position and content digest prevents cross-branch or changed-content matches;
optional timestamp absence is harmless but contradictory timestamps are not.

Local matchers must be pure and stable within an interest revision for this
comparison to be meaningful. Custom stateful/time-sensitive matchers cannot be
snapshotted by cloning a trait object; their changes can change the compared
domain without a revision and this API must not be used to infer WS completeness
with those matchers. Driver-controlled interest revision changes reset the
observation history. This limitation is distinct from provider delivery quality.

## Bounds and cost

The private `subscriber_driver::observation` leaf owns the ledger, comparison and
atomic snapshots. It depends only on existing driver types, Alloy log/filter
predicates and the private input fingerprint helper. The subscriber-driver facade
owns configuration and calls it at input, reset/retirement and the existing read
result. Applications import the public snapshot through `adapters`.

At most 64 exact block identities and 4096 total log fingerprints are retained.
No log payload or unbounded late-history queue is copied. Overflow marks evidence
unavailable; retirement retains a refusal watermark so late records cannot
recreate apparently complete history. Reset clears the diagnostic epoch. Opt-out
allocates no ledger and performs no fingerprints, comparisons, selector calls or
new clock reads. Feature-off removes the new API/fields/code. Opt-in performs
bounded local hashing/set comparisons and no I/O. No timer, RPC, fetch filter,
retry, canonical publication or receipt authority changes. There is no exporter,
metric-label selection, persistent schema, background task or raw payload log.

## Bloom soundness audit

The extracted `subscriber_driver::bloom` leaf preserves the alpha.6 predicate:
address alternatives are OR; alternatives inside each topic slot are OR; each
constrained slot is AND with the address clause. A matching exact-filter log
contributes its address and every topic to the Ethereum block bloom. Therefore
absence of all address alternatives, or all alternatives in any constrained
slot, excludes that filter. Every registered log filter must be excluded before
bloom closure succeeds. Bloom collisions and contributions split across several
logs admit false positives; they cannot establish a missing matching log. Position
or address/topic co-occurrence is not inferred from bloom membership.

This proof is conditional on the block's actual valid header bloom, not an
untrusted arbitrary bitfield. The seal uses that pending header's bloom; the
canonical batch recomputes its advertised header hash before acceptance. Reorgs
and gaps reset buffered logs, coverage, successor proof and diagnostics; lineage
checks bind parent hashes/numbers, and fetched logs are validated against the
exact requested header identity. A same-height replacement does not reuse the
old branch's seal decision. No independent consensus or receipt-root validation
against a dishonest provider is added or claimed.

The existing conservative policy deliberately remains unchanged:

- An address-wildcard filter refuses bloom exclusion even when a constrained
  topic alone is absent. This can cause unnecessary reconciliation but is safe.
- No registered log interests also refuses the proof. This can be conservative.
- Absence requires coverage first. An all-zero bloom with coverage can seal
  immediately; grace expiry itself does not prove completeness.
- Multiple registered log interests cannot use a single stream successor as
  closure proof, even where upstream coalescing might create one subscription.
- Reconciliation still fetches all registered filter groups after fallback;
  per-filter bloom pruning is a separate unimplemented policy/cost improvement.

Tests use Alloy's actual exact predicate to exercise no-false-negative cases,
address/topic alternatives, missing slots, wildcard/all-zero/saturated blooms,
false positives split across logs, hash mismatch and existing reorg/gap paths.
Comparison fixtures cover empty/equal/missing/conflicting/duplicate/unavailable,
local selection, errors, capacity, retirement and late input. A driver fixture
holds publication and existing request counts equal with observation off/on.
No production coverage, RPC saving or latency improvement is inferred from these
deterministic fixtures. Publication and downstream rollout remain separate work.
