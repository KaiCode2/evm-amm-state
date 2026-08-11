# Exact V3 swap transitions

`evm-amm-state 0.3.0-alpha.5` can replay a canonical Uniswap V3 `Swap` from
event data without a provider read. Exactness is deliberately narrower than
“V3-like”: it is a capability granted only to a proven runtime family and
storage surface.

## Exactness contract

The exact path requires all of the following:

- canonical Uniswap V3 protocol metadata, fee, positive tick spacing, and the
  canonical storage layout;
- an exact parent snapshot containing slot0, active liquidity, both global fee
  accumulators, protocol fees, the current oracle observation, every bitmap
  word traversed, and all four `Tick.Info` words for every initialized tick
  crossed;
- chain id, block number, block hash, parent hash, block timestamp, transaction
  hash, transaction index, and log index in `AdapterEventContext`; and
- events presented by the caller in canonical transaction/log order, without
  duplicates.

`AdapterEventContext` records ordering evidence; it does not itself sequence or
deduplicate events. `AmmCanonicalBatch` requires strictly increasing
`(transaction_index, log_index)` positions, and the live actor rejects a second
submission of its exact current `(number, hash)` before adapter invocation.
Same-height different-hash batches remain reorg replacements. The stateless
direct driver cannot enforce cross-call ordering, so its caller owns that
boundary. An absent storage cell is unknown, never EVM zero. Cold-start writes
an explicit zero only when a provider response proved the bitmap word was zero.

For a supported event the transition locally reproduces every swap-induced
canonical mutable write:

- packed slot0 price, tick, oracle index, and oracle cardinality;
- active liquidity;
- the input token's `feeGrowthGlobal*X128` and packed protocol-fee accumulator;
- the canonical observation-ring write, including cardinality growth, wrap,
  and same-timestamp behavior; and
- all four outside-accumulator words for each crossed initialized tick.

Replay stops at every canonical swap-step boundary, including uninitialized
bitmap-word boundaries, so per-step ceil/floor rounding and fees match deployed
bytecode. It handles both directions, exact input and exact output, partial
price-limit steps, zero-liquidity gaps, and tiny exact-input swaps whose entire
input becomes a fee without moving price.

Before returning one atomic update batch, replay validates the event's signed
token deltas, final `sqrtPriceX96`, tick, and active liquidity. Missing context
or parent cells, contradictory state, oracle/tick inconsistency, arithmetic
failure, or a postcondition mismatch returns a typed error. Both the direct and
reactive paths purge the stale pool storage and request repair before surfacing
that error; they never leave the parent state quote-ready. A deterministic
4,096-step ceiling bounds adversarial work and also fails closed.

No provider or network I/O occurs in this transition.

## Family capability boundary

| Family | Exact `Swap` capability | Reason |
| --- | --- | --- |
| Canonical Uniswap V3 | `Exact` with complete parent/context | Canonical storage and swap semantics are covered by historical traces and local deployed-runtime differential execution. |
| PancakeSwap V3 | `Unsupported` | Its extended event and family-specific fee/oracle/tick behavior require independent deployed-bytecode parity. Canonical semantics are not inferred from similar fields or slots. |
| Slipstream / Aerodrome CL | `Unsupported` | Deployed runtimes mutate reward, gauge, staked-liquidity, oracle, and extended tick state beyond canonical Uniswap V3. |

The checked-in Base and Optimism Slipstream trace fixtures pin actual deployed
runtime identities and complete accessed/write sets for continued independent
research. In those captured runtimes, the core layout includes slot0 at
6, active/staked liquidity packing at 15/16, ticks at mapping base 17, and the
bitmap at mapping base 18. A swap can also mutate global fee growth, gauge fees,
reward growth/reserve, staked-liquidity/time state, observations, and words 2–5
of a six-word `Tick.Info`. Therefore `V3StorageLayout`'s four-field legacy
Slipstream preset is quote/cold-start configuration only, not an exactness
claim.

The crate retains a provider-free candidate transition, address-bound runtime
attestation, unique event-derived fee inference, two historical initialized
crossings, and a ten-case local runtime corpus. That corpus is deliberately
non-authoritative: it does not yet cover the required generated mixed-liquidity
fee branch, multiple initialized crossings with staked-liquidity net changes,
ordered same-timestamp sequences, oracle growth/wrap, and reward
reserve/rollover/no-staked variants. Consequently valid research evidence can
never elevate the public capability above `Unsupported`; every real reactive
Slipstream swap purges and requests repair. Base/Optimism Flashblocks execution
is not enabled by alpha.5.

## Non-Swap parent integrity

`Exact` in alpha.5 applies only to the canonical Uniswap V3 `Swap` transition.
The adapter subscribes every standard pool mutation topic: `Initialize`,
`Mint`, `Collect`, `Burn`, `Flash`, `IncreaseObservationCardinalityNext`,
`SetFeeProtocol`, and `CollectProtocol`. Each recognized non-`Swap` event emits
a typed unsupported/`RequiresRepair` result with an explicit whole-storage purge.
This is intentionally conservative: warm `Mint`/`Burn` quote cells do not cover
the complete Tick.Info and position-accounting writes, so no later Swap may use
that partial state as an exact parent. Exact Swap eligibility resumes only after
an authoritative complete rebuild.

## Verification evidence

The provider-free test suite contains three complementary layers:

- deterministic transition regressions for empty bitmap words, initialized
  crossings, missing cells, oracle growth/wrap/same-timestamp sequences,
  protocol fees, zero-liquidity gaps, work limits, and rounding edges;
- a local revm differential corpus that runs an embedded deployed canonical
  Uniswap V3 pool runtime and compares every declared pool slot, including every
  reference-changed slot, over fixed semantic cases plus a generated cross
  product of four fee tiers, three liquidity scales, both directions, and exact
  input/output. A fixed-seed sequence corpus also varies fee-tier tick spacing,
  initialized-tick distributions and both `liquidityNet` signs, oracle
  index/cardinality/timestamps, and applies four ordered swaps to the derived
  parent while comparing deployed bytecode after every step; and
- a two-swap Ethereum historical differential at block 25,723,647. Transactions
  `0x8ce97be13effe21a24b234df1db74ea3569ce9953c54b4373154c5e3dc849b92`
  and `0x75b5edcaf9e5da77246fddc2cdb5aeb43fc2baa0c6d5a7f94db6730ecd3615bf`
  were independently traced with geth `debug_traceTransaction` and
  `prestateTracer(diffMode=true)`; the fixture asserts both the intermediate
  first-transaction poststate and final second-transaction poststate.

The generated local corpus is deterministic and network-free. It is not a
claim that arbitrary future forks share canonical behavior; family promotion
always requires its own deployed-runtime evidence.
