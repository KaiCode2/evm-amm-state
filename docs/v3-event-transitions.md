# Exact V3 event transitions

`evm-amm-state` can replay **every** canonical Uniswap V3 pool event that
mutates pricing state — `Swap`, `Mint`, `Burn`, `Collect`, `Flash`,
`SetFeeProtocol`, `CollectProtocol`, and
`IncreaseObservationCardinalityNext` — without a provider read, along with the
reviewed Base/Optimism Slipstream `Swap`, `Mint`, `Burn`, and `Collect`.
`Initialize` remains a deliberate exception: a pool emitting it has no history
worth preserving, so it still requests a cold start.

Exactness is deliberately narrower than “V3-like”: it is granted only to a
proven runtime family, deployment, and declared state surface. Cold start
retains the reviewed quote runtime's external fee cells, the complete
initialized observation ring, and all six Slipstream tick words so the resulting
event transition can be searched and simulated without repair or RPC
reconstruction.

## Why quoting is not the bar

`SwapMath.computeSwapStep` — the math behind a quote — never reads a tick's
outside accumulators. The only reader is `Tick.cross`, and cross *writes* them
rather than feeding the amounts it returns. A transition that maintains
`liquidityGross`/`liquidityNet` and the bitmap therefore quotes correctly
forever while silently corrupting `feeGrowthOutside{0,1}X128`: under a quoter,
whose callback reverts, the wrong value is discarded before anyone can observe
it; under a committed swap it persists and poisons every later
`getFeeGrowthInside`.

Quote-readiness and simulation-readiness are not the same property, and the
declared surface below is the second one. This is why the verification evidence
runs mint, swap, and burn against deployed bytecode *in sequence* and compares
after every step, rather than checking each event in isolation.

## Exactness contract

Canonical Uniswap's exact path requires all of the following:

- canonical Uniswap V3 protocol metadata, positive tick spacing, and the
  canonical storage layout — plus the pool fee for `Swap` and `Flash`, the only
  two transitions whose arithmetic consults it;
- an exact parent snapshot containing slot0, active liquidity, both global fee
  accumulators, protocol fees, and the current oracle observation. `Swap`
  additionally needs every bitmap word traversed and all four `Tick.Info` words
  of every initialized tick crossed; `Mint`/`Burn` need the bitmap word of each
  boundary tick, and that tick's four words only when its bit is set;
- chain id, block number, block hash, parent hash, block timestamp, transaction
  hash, transaction index, and log index in `AdapterEventContext`. The block
  timestamp is load-bearing rather than bookkeeping: `_modifyPosition` reads the
  oracle for any nonzero liquidity delta, so a caller with no context gets the
  conservative invalidation instead; and
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

### The `Swap` surface

For a supported `Swap` the transition locally reproduces every swap-induced
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

No provider or network I/O occurs in this transition. The canonical non-`Swap`
surfaces are described under [Canonical non-Swap
transitions](#canonical-non-swap-transitions) below.

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
| Canonical Uniswap V3 | `Exact` with complete parent/context, for every pricing-state event except `Initialize` | Canonical storage and event semantics are covered by historical traces and local deployed-runtime differential execution, including the `onlyFactoryOwner` paths and the flash callback. |
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

## Canonical non-Swap transitions

`Mint` and `Burn` replay `_modifyPosition` over the pool's pricing surface:
both boundary ticks through `Tick.update`/`Tick.clear`, the bitmap words they
flip, the oracle entry an in-range change writes, and active liquidity. The
event carries only a liquidity delta, so the accumulator writes come from the
exact parent — on first initialization Uniswap assumes all growth so far
happened *below* the tick, which copies both global fee accumulators and the
current oracle reading into the outside words, but only for a tick at or below
the current one. A tick above it is initialized with nothing but its flag.

A boundary tick's `Tick.Info` words are read only when its bitmap bit is set.
A clear bit *proves* the struct is zero — `flipTick` is called precisely on a
flip and `Tick.clear` zeroes the whole struct — so a mint opening a brand-new
position needs no tick read at all. That matters because cold start warms bitmap
words but not empty ticks. Whenever the parent happens to hold a word anyway,
the inference is cross-checked against it in both directions; a disagreement
fails closed rather than being resolved in the inference's favor.

`Collect` is an exact transition with no writes: it moves position `tokensOwed`
and ERC-20 balances and touches no pricing cell. This matters out of proportion
to its simplicity, because nearly every `Burn` through the position manager is
followed by a `Collect`.

`Flash`, `SetFeeProtocol`, `CollectProtocol`, and
`IncreaseObservationCardinalityNext` each move fee, protocol-fee, or
oracle-reservation state and are fully determined by the event plus warm parent
cells. Each validates a postcondition against the parent — the fee split the
event reports, the reservation it grows from, the balance it debits — so a
foreign or replayed event cannot be applied.

### Position accounting

Position ownership and tokens-owed accounting sit outside the declared surface:
`swap` never reads `positions`, so reconstructing them would buy nothing for
quoting or simulation and would require parent cells cold start never warms.
A `Mint`, `Burn`, or `Collect` therefore *invalidates* the position it names
rather than leaving a warm cell stale. Callers that need position state read it
on demand.

### What still fails closed

`Initialize`, malformed events, missing mandatory parent cells, a locked parent
`slot0`, a bitmap that disagrees with `Tick.Info`, a range canonical `checkTicks`
or `flipTick` would reject, arithmetic beyond `maxLiquidityPerTick`, wrong
chain/address, non-canonical layouts, unproven families, and unreviewed
Slipstream registrations all remain typed fail-closed invalidations that purge
the pool's storage. Exact eligibility resumes only after authoritative repair.

One case is deliberately weaker than a purge. A boundary tick outside the warmed
bitmap radius is *unknown*, not contradicted — an LP may open a position
anywhere. Price, active liquidity, the oracle, and any resolvable boundary are
still established exactly, and only the unresolvable boundaries' `Tick.Info`
words plus their bitmap words are dropped and re-read through
`RepairAction::VerifySlots` — five slots per boundary, and a word shared with a
resolvable boundary is withheld too, since the merged value would be missing one
of the two flips. The distinction is load-bearing: an unknown cell costs a
handful of slots, while a contradicted parent costs the pool.

## Verification evidence

The provider-free test suite contains complementary canonical and Slipstream
layers:

- deterministic transition regressions for empty bitmap words, initialized
  crossings, missing cells, oracle growth/wrap/same-timestamp sequences,
  protocol fees, zero-liquidity gaps, work limits, and rounding edges;
- a canonical liquidity differential that drives the embedded deployed pool
  runtime through ordered `Mint`/`Burn`/`Swap` sequences and compares every pool
  slot the reference touched after **each** step, plus the update batch's
  coverage of every reference-changed slot. It covers a new tick below and above
  the current one, an existing tick, a burn that clears a tick and a later
  re-initialization of it, in-range and out-of-range positions, boundaries
  sharing one bitmap word, oracle same-timestamp/growth/wrap, a nonzero
  `feeProtocol`, all four fee tiers, and a zero-amount `Burn`. After every step
  a swap is simulated against a cache holding nothing but the event-derived
  state and must return the reference's exact amounts — matching storage is
  necessary, but executability is the property a caller actually needs;
- a canonical accounting differential for `Flash`, `SetFeeProtocol`,
  `CollectProtocol`, and `IncreaseObservationCardinalityNext`. Two of these are
  `onlyFactoryOwner`, so the harness is installed at the address the pool's
  `factory` immutable names and reports itself as that factory's owner: the
  canonical privileged path executes rather than being stubbed away, and the
  flash callback repays for real;
- a fail-closed acceptance suite asserting the *boundary* rather than the happy
  path — which conditions purge the pool, which resync a single boundary, and
  that neither PancakeSwap V3's byte-identical `Mint`/`Burn` ABI nor a
  canonical protocol id over a foreign layout is ever promoted;
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
