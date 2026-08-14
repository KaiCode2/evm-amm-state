# Exact V3 swap transitions

`evm-amm-state 0.3.0-alpha.8` can replay a canonical Uniswap V3 `Swap` and the
reviewed Base/Optimism Slipstream `Swap`, `Mint`, `Burn`, and `Collect` events
without a provider read. Exactness is deliberately narrower than “V3-like”:
it is granted only to a proven runtime family, deployment, and declared state
surface. Cold start retains the reviewed quote runtime's external fee cells,
the complete initialized observation ring, and all six Slipstream tick words so
the resulting event transition can be searched and simulated without repair or
RPC reconstruction.

## Exactness contract

Canonical Uniswap's exact path requires all of the following:

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

### Reviewed Slipstream search exactness

The Aerodrome Base BIFI pool and Velodrome Optimism mooBIFI pool receive a
narrower event-scoped exact capability when chain, address, positive tick
spacing, deployed core layout, complete lineage, and an exact parent all match.
For `Swap`, the event's signed amounts, final price, tick, and liquidity let the
adapter replay the exact geometry and infer the unique effective fee. It then
publishes slot0, active liquidity, staked-liquidity bounds, oracle state, and
the initialized-tick traversal needed by subsequent executable quotes. No fee
evaluation or provider reconstruction is required on this ordinary path.

`SlipstreamSwapFeeEvidence` is optional. When the caller supplies evidence
bound to the exact runtime hashes, lineage, and provider-free fee evaluation,
the same transition also reproduces fee growth, gauge fees, reward accounting,
and crossed-tick outside accumulators. Invalid supplied evidence fails closed;
its absence merely selects quote/search exactness instead of forcing an RPC
rebuild.

For `Mint` and `Burn`, replay updates both six-word boundary ticks, gross/net
liquidity, initialization state, one or two bitmap words, current active
liquidity, and the oracle state. It models the deployed Base/Optimism difference
in reward-growth initialization. A zero-amount `Burn` is an exact search no-op.
`Collect` is also an exact empty search transition because it changes position
and ERC-20 balance accounting, not pool pricing state.

This is not byte parity for the whole Slipstream system. Position ownership,
token transfers, silent gauge stake/unstake activity, gauges and rewards outside
the pool search surface, administrative mutations, and arbitrary Slipstream
deployments remain outside the capability. Their presence is not inferred from
layout similarity.

## Family capability boundary

| Family | Exact `Swap` capability | Reason |
| --- | --- | --- |
| Canonical Uniswap V3 | `Exact` with complete parent/context | Canonical storage and swap semantics are covered by historical traces and local deployed-runtime differential execution. |
| PancakeSwap V3 | `Unsupported` | Its extended event and family-specific fee/oracle/tick behavior require independent deployed-bytecode parity. Canonical semantics are not inferred from similar fields or slots. |
| Reviewed Base/Optimism Slipstream pools | event-scoped `Exact` for the declared quote/search surface | Deployed proxy and implementation runtimes, historical crossings, generated swap shapes, Mint/Burn state, and provider-disconnected follow-up quotes are differential-tested. Optional runtime-bound evidence covers the stronger swap accounting surface. |
| Other Slipstream / Aerodrome CL | `Unsupported` | Address, chain, runtime semantics, and layout have not been independently proven. |

The checked-in Base and Optimism Slipstream fixtures pin the deployed proxy and
implementation identities and complete accessed/write sets used by the stronger
accounting differential. The core layout includes slot0 at 6,
active/staked-liquidity packing at 15/16, ticks at mapping base 17, and the
bitmap at mapping base 18. The public no-evidence path deliberately writes only
the quote/search subset of that state; callers must not reinterpret
`V3SwapTransitionCapability::Exact` as protocol-wide storage parity.

## Non-Swap parent integrity

Canonical Uniswap still treats every standard non-`Swap` mutation as
unsupported and requests an explicit whole-storage rebuild. Reviewed
Slipstream handles `Mint`, `Burn`, and `Collect` as described above. `Initialize`,
`Flash`, `IncreaseObservationCardinalityNext`, `SetFeeProtocol`,
`CollectProtocol`, malformed events, missing parent cells, wrong chain/address,
and unreviewed Slipstream registrations remain typed fail-closed invalidations.
Exact eligibility resumes only after authoritative repair for those cases.

## Verification evidence

The provider-free test suite contains complementary canonical and Slipstream
layers:

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
- deployed Base and Optimism Slipstream proxy/implementation execution across
  five swap shapes per family, including both directions, exact input/output,
  fee variants, and a partial price limit. The accounting-evidence path is
  compared over every accessed pool cell; the evidence-free path is applied to
  a mock-provider cache and both follow-up quote directions must match the real
  deployed runtime without provider access; and
- Mint/Burn round trips on both Slipstream runtimes, including initialized tick
  creation/removal, bitmap changes, active liquidity, and the chain-specific
  reward initialization behavior, followed by provider-disconnected quotes
  after each state transition.

The generated local corpus is deterministic and network-free. It is not a
claim that arbitrary future forks share canonical behavior; family promotion
always requires its own deployed-runtime evidence.
