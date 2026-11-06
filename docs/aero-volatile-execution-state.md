# Aero Optimism volatile execution-state projection

Status: opt-in local event reducers and independent bytecode regressions. This is
not an event-only full-executor capability, and no simulator projection is enabled.

## Evidence checklist

- Trigger: complete canonical ordered pool/governance events, explicit chain 10,
  block/hash/parent/timestamp, transaction hash/index and log index. Parent cells
  must come from the same authenticated predecessor. Caller owns rollback,
  atomic application and gap/reorg rejection; these pure reducers perform no I/O.
- Quote/planning/permission: preserve existing Sync-only compatibility and exact
  reviewed pool/layout. Add event-derived execution cells only through the
  explicit `SolidlyV2Adapter::aero_volatile_execution_event_updates` method.
- Provider/latency: zero calls in reducers. Missing cells fail closed. Initial
  reserve-oracle geometry/values and governance cells still need cold discovery.
  No before/after worker latency or billable-RPC claim is made by these unit tests.
- Lifecycle/accounting: no signer, submission, journal, inventory or authority
  changes. Fees and cumulative arithmetic use Solidity's checked integer rules.
- Observability: no synchronous export. Callers must distinguish incomplete
  execution state from an exact event-derived pool projection.
- Evidence: source-specific boundary tests, canonical before/after pool fixtures,
  and deployed factory/tape bytecode with explicitly synthetic parent storage.

## Cell ownership

The reviewed pool is `0xc71b571aa9f06bfa096c24e9df5c45fc4fab4151`.

| Surface | Event/parent inputs | Owned cells |
| --- | --- | --- |
| LP global supply | mint/burn `Transfer` | 2 |
| Reserves, full block time, cumulative reserves | `Sync`, prior reserves/time/cumulatives | 196619–196623 |
| Reserve oracle | `Sync`, uint32 time, prior packed geometry and timestamp | observation triples from 11; packed header 196616 |
| LP fee indexes | `Fees`, supply, prior index | 196624–196625 |
| Factory settings | exact deployed setter events | packed 4; 5–14 |
| Custom fee | indexed `SetCustomFee` | `keccak(pool,0)` at reviewed fee module |
| MEV controls | multiplier/threshold/factor events | packed 1–2 at reviewed MEV module |

LP balances, per-holder accrued fee claims and approval mappings are not claimed
by this swap-state projection. They are distinct from pool global fee indexes.
The pool reserve oracle writes after a strictly greater than 60-second uint32
elapsed interval, grows one initialized observation at a time, and uses full
uint256 checked time arithmetic for the reserve cumulatives. Oracle growth has
a protocol maximum of 65,535 entries. Unknown replacement module identities reject.

## Proven reserve-oracle allocation

The exact immutable clone/implementation identities are required by the event
surface and again by the allocation helper. `PoolOracle.initialize` initializes
only observation zero, then sets cardinality and cardinalityNext to one.
`grow` writes exactly the contiguous `[oldNext,newNext)` suffix and only increases
next. `write` consumes that prewarmed suffix one entry at a time, and never writes
outside cardinality. There is no shrink, reset after initialization or arbitrary
storage-write entrypoint. The reviewed factory initializes each new clone in its
creation transaction; pool initialization sets its nonzero factory once, and
subsequent initialization reverts. Its token metadata reads are static calls.

Consequently, an index beyond exact parent next has never been allocated and its
three words are zero; an index in `[cardinality,next)` is exactly the prewarmed
triple `(1,1,1)`. The reducer derives these values only for that validated contiguous
range under runtime attestation. Any contradictory resident word rejects the
transaction. It never uses this proof for initialized/reused observations, current
oracle state, governance cells or arbitrary cache misses. Missing runtime evidence,
old-length mismatch, gaps, shrink and invalid geometry all reject.

The canonical Optimism fixture contains blocks 158757177, 158757182 and 158757220,
with seven swaps and exact parent/end EIP-1898 words. At 158757220 the real oracle
moves from cardinality 94 to 95. The allocation regression removes only the three
captured prewarm words (each exactly one), derives them without provider access,
and compares the written observation to the pinned public end state. Pool and
implementation bytecode hashes were separately read at the first canonical pin.

## Why logs cannot recover exact tape state

The exact factory calls `PoolTape.record` with `excessivelySafeCall`, copies no
return bytes, and discards success. The tape updates its five accumulator words
on each successful call but emits no observation event within its cadence.
Reverted nested calls leave no logs or storage writes. The factory's
`gasleft() > configuredLimit` check precedes ABI preparation and the EVM CALL;
it does not establish that EIP-150 actually forwarded the configured limit.

The passing `tape_ambiguity` regression runs both reviewed deployed runtimes, with one
fully declared synthetic parent and identical calldata. It finds a gas boundary
where both outer factory invocations succeed and return identical bytes/logs,
while one silently advances the accumulator and the other does not. This proves
that an event stream does not determine the tape accumulator or exact gas usage.
It does not prove different token settlement outputs. The configured tape cap was
35,020 gas; the bounded outer transaction used a 62,020 limit. The complete
record used 65,821 transaction gas, versus 61,655 for the swallowed failure.

## Narrow noninterference argument to review

For these exact identities, the volatile pool computes output transfers, input
balances, all fee amounts, and the curve invariant before calling the tape.
Afterwards it updates its own reserves/oracle and emits `Swap`. The reviewed
CustomFeeModule reads only its custom-fee mapping. The reviewed MEV module reads
its multiplier/threshold controls and block/transaction fee context. Neither
reads tape. The tape `record` call writes only its own mappings, makes no external
call, transfers no native value, and returns no value used by the factory. Its
observation logs do not feed subsequent on-chain control flow.

Thus tape state cannot change those swap amounts or pool poststate, provided the
rest of the transaction completes. It can change gas, its own state and its own
observation logs. Any proposed abstraction must establish all of these predicates:

1. Exact runtime hashes for pool implementation/proxy, factory and tape; exact
   immutable/storage bindings, chain and canonical publication lineage.
2. Only the known factory-to-tape CALL, `record(address,(uint128,...))` selector,
   zero value, the intended pool argument, and exact calldata length/encoding.
   No arbitrary caller, selector, DELEGATECALL or STATICCALL substitution.
3. Execute the actual caller up to CALL so memory expansion, EIP-2929 target
   warming, configured gas checks and EIP-150 forwarding remain real.
4. Abstract only the tape subframe, consume its entire actually forwarded gas,
   return failure with empty bytes, and produce no invented tape storage. This
   is an explicit conservative result, never an exact measured execution trace.
5. Prove there is no permitted outer branch that inspects that success/returndata,
   no outside tape writes/calls/reentrancy, and no later economic tape getter.
6. Prove the remaining permitted batch is gas-monotone. In particular, a later
   gas-sensitive fee-module call must not change from a successful custom fee
   to a fallback default under the smaller abstract remaining gas. A generic
   repeated-swap batch cannot be accepted on the single-swap argument alone.
7. Treat cost as an upper bound with zero tape refunds; never label the missing
   tape observation logs or state as exact. Prove strict simulation, receipt
   accounting and budget checks consume the appropriate result category.
8. Revoke this capability on any unreviewed identity/configuration change or
   event-delivery gap. Unknown state stays unknown.

These are proof obligations, not an implemented bypass. Production stays held.

## Primary sources

Sourcify responses include deployed runtime match and complete storage layout:

- [Pool implementation](https://sourcify.dev/server/v2/contract/10/0x0526cf035c1156b5c598b0d3c2445e38af2e8bde?fields=all)
- [Factory](https://sourcify.dev/server/v2/contract/10/0x8e6a9fcd1d638cc760ffba397605fe3355de76d2?fields=all)
- [PoolTape](https://sourcify.dev/server/v2/contract/10/0x34bdd64b19d8631f936c2d07f19aaa6b83515992?fields=all)
- [Custom fee module](https://sourcify.dev/server/v2/contract/10/0x7949de8d67e4f6b8509bdbb43ff0503af1247a7a?fields=all)
- [MEV module](https://sourcify.dev/server/v2/contract/10/0x5de55ebe5b789bd309809c7969334c2c16e64eaf?fields=all)

Embedded test runtime keccaks:
factory `bb6f945479608857a220f9643db76e877f1e52844d953d980c18f5ea38d9574b`;
tape `dfff920fc935eb9b3c1f202ca7332e9131e3f0ea3c82dfa0d791f782ecd0163b`.
