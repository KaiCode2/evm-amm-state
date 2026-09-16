# Canonical Slipstream staking continuity

`0.3.1-alpha.3` maintains the staking fields used by subsequent swaps and quotes
for the reviewed Optimism deployment. `Swap` and `Mint`/`Burn` alone cannot do
this: gauge `stake` calls also change the pool's active staked liquidity and the
signed staked-liquidity net at each position boundary.

## Evidence and ordering

The adapter adds the reviewed gauge's `Deposit`/`Withdraw` and the reviewed NFT
manager's `Collect`/`Transfer` to its event sources. Within one complete delivered
transaction, it binds the pool `Collect` range to the manager's token ID and
recipient, then checks custody and the gauge's indexed liquidity delta. The
reviewed manager emits exactly one `MetadataUpdate` between the two Collect
logs; their canonical log indices must therefore differ by two.

A deposit applies at `Deposit`. A withdrawal applies at the preceding NFT
`Transfer`, because the pool has already unstaked the position and the receiver
callback can execute another pool operation before `Withdraw` is emitted. The
later `Withdraw` confirms the same operation without applying it twice. A direct
NFT transfer into the gauge alone is not evidence of staking.

`adapters::slipstream_staking` owns the correlation and checked state transition.
`AmmSyncEngine` prepares a bounded immutable index before replay and clears it
on both success and failure. `AmmRuntime` uses that path automatically. The
reactive handler stays a synchronous function of supplied evidence and state;
no partially processed token history survives batches or reorgs.

The index accepts at most 16,384 records. Matching is indexed by transaction,
token and log position. It requires canonical or owner-catchup delivery visible
to the exact pool generation, coherent block/parent/transaction identities,
canonical ABI payloads, and unique associations. Runtime hashes qualify the
pool proxy/implementation, gauge proxy/implementation and NFT manager, together
with cached pool gauge/NFT bindings. Incomplete evidence invalidates the pool
and exposes a typed `SlipstreamStakingEvidence` error.

The deployed source used to review event ordering is available from Sourcify:
[pool implementation](https://sourcify.dev/server/v2/contract/10/0xc28ad28853a547556780bebf7847628501a3bcbb?fields=all),
[gauge implementation](https://sourcify.dev/server/v2/contract/10/0x7155b84a704f0657975827c65ff6fe42e3a962bb?fields=all),
[NFT manager](https://sourcify.dev/server/v2/contract/10/0x416b433906b1b72fa758e166e239c43d68dc6f29?fields=all).
The on-chain runtime bytes match the hashes checked by the implementation.

## State and I/O boundary

The transition changes only active staked liquidity (low 128 bits of slot 15)
and initialized boundary staked-liquidity nets (low 128 bits of tick word 1).
It preserves other packed fields, ordinary liquidity, bitmap and price state.
All writes are derived and checked before application. Missing required words
or arithmetic failure produces invalidation instead of a partial update.

This is quote/search state continuity. Gauge rewards, NFT/position accounting,
ERC-20 balances and complete pool reward accounting are outside the guarantee.
Optional full-accounting swap replay still requires an independently exact
accounting parent; staking quote updates do not establish that stronger parent.
Other deployments, including Base staking transitions, are not qualified by
this change. Preconfirmed staking batches fail closed.

The pool, gauge and NFT sources share one topic union, so the subscriber
consolidates them into **one provider log filter**, as before. Local decoding
checks the qualified emitter and exact event. The response now includes the
manager's Collect/Transfer activity, which increases log bytes even when those
NFTs belong to other pools. There are no per-token receipt or position reads.

Event preparation and application make **zero provider requests**. Staking
batches obtain a local immutable cache view for runtime qualification; ordinary
swap-only batches skip that extra view. Cold preparation adds two pool words
(slots 3 and 4) to the existing bulk/windowed footprint and three code targets
(gauge proxy, implementation, NFT manager) to existing code preparation. In the
live prepared-state path those targets cost six initial RPC elements: one code
read and one proof read per account, before retries. They are not one billable
request merely because they serve one preparation operation. No new poller or
quote-time I/O is introduced.

## Integration and diagnostics

Restart from an authoritative cold baseline with the new event interests and
code targets. Do not reuse a cache that has already missed staking mutations.
Use complete canonical batches through `AmmRuntime` or `AmmSyncEngine`; delivering
only individual gauge logs cannot provide the required range and ordering proof.
Raw driver users can call `SlipstreamStakingBatch::from_batch` and attach its
opaque per-event evidence to `AdapterEventContext` while replaying that same
canonical batch in order.

`AmmSyncBatchReport::decode_diagnostics()` and
`AmmChangeSet::decode_diagnostics()` preserve the original typed cause, exact
source `InputRef`, and `PoolInstanceId`. Committed diagnostics retain at most
32 failures, plus an omitted count; custom adapter strings are capped at 512
bytes. Logging can include source identities as fields. Metrics should use
`error_class()` and protocol, never transaction hashes, slot keys or raw error
text as labels. Consumers should alert on any decoded-state invalidation and
on repeated failures in a bounded window. Export/log asynchronously outside
quote and simulation paths.

## Regression evidence

The suite exercises provider-disconnected public engine replay, deposit and
withdrawal cycles, a receiver callback that burns liquidity and swaps before
`Withdraw`, payload/runtime mismatch, missing correlation evidence, unrelated
NFT transfers, and reorg rollback followed by replacement replay. A separate
continuous test compares the staking quote words against execution of the
reviewed deployed pool bytecode. Runtime tests verify diagnostic publication,
its bound, and isolation between commits.

Validation on 2026-09-16 used Rust 1.90 on an Apple M1 Pro (macOS, arm64):

| Configuration | Passing tests, including doctests |
| --- | ---: |
| All features | 613 |
| Default features | 500 |
| No default features | 115 |

The optimized staking-batch regression ran 1,000 alternating deposit/withdrawal
samples with 10,000 unrelated cached slots: p50 0.274 ms, p95 3.981 ms,
p99 14.061 ms, maximum 34.708 ms. It includes local evidence preparation and
event application, with zero provider elements, retries, fallbacks or repairs.
Concurrent local builds were active; these measurements are not a production
opportunity-latency distribution. Reproduce with:

```sh
SLIPSTREAM_STAKING_SAMPLES=1000 SLIPSTREAM_STAKING_BACKGROUND_SLOTS=10000 \
  cargo test --release --locked --test slipstream_liquidity_transition \
  staking::repeated_deposit_withdraw_cycles_preserve_packed_words_and_never_repair \
  -- --exact --nocapture
```

The existing 1,000-sample event-to-bid/ask regressions also passed for both
reviewed deployments, with zero provider calls, invalidations or resyncs.
Their p95 values were 0.893 ms (Base) and 1.690 ms (Optimism). Mandatory live
Base parity/discovery checks passed through a paid QuickNode endpoint. This
validates the existing Base quote path; it does not qualify Base staking.
