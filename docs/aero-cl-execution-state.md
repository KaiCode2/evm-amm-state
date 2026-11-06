# Aero CL execution-state transitions

This work separates exact quote state from the additional state required by a
real executor. It does not make an ordinary log subscription an authenticated
complete execution-state feed.

## Evidence checklist

1. The trigger is an ordered canonical event, or an authenticated successful
   call with its exact call-time state. Chain, block hash, parent hash,
   timestamp, transaction and log order remain explicit.
2. Pool quote, planner, calldata and permissions are unchanged. Real tape and
   gauge execution must never inherit the quote path's code substitutions.
3. Reducers use immutable state only and perform zero provider calls. Missing
   parent cells and unproved code identities reject the transition. A separate
   input owner must establish complete block delivery and supply evidence for
   writes not reconstructible from logs.
4. Updates are returned as one batch for atomic application. The caller owns
   canonical ordering, deduplication, restart baseline and rollback. No mutable
   partial transaction evidence is retained by these leaves.
5. Gauge reward cursors, pool reward growth, tape fee/volume cumulatives and
   oracle accumulators retain the exact Solidity integer widths and rounding.
6. No new telemetry, blocking export, polling or runtime network path is added.
7. Validation must cover real canonical event identities, missing state,
   wrong-chain/runtime evidence, stale cursors, duplicate application,
   malformed payloads, arithmetic boundaries and local fork parity. Full
   execution qualification additionally requires an actual complete input
   source; unit tests are not that evidence.

## Proven boundaries

The existing pool reducers own slot0, tick/bitmap changes, oracle observations,
fee/reward growth and staked liquidity. `GaugeSettled(cumulative, delta)` also
determines the gauge's `lastCumulativeRewardShare` exactly: the present parent
cursor must equal `cumulative - delta`, and `delta` must be positive.

`DynamicSwapFeeHook.afterSwap` calls `CLPoolTape.record` through a gas-capped
fail-open call. The tape writes five accumulator words even when cadence means
it emits no `ObservationRecorded`. A pool `Swap` or the absence of a tape log
cannot prove whether the call succeeded. Its input also distinguishes dynamic
and MEV fees and toxic volume through transaction-local hook state.

The tape receives the final in-memory swap tick, but reads the pool's oracle
and staked-seconds getters before final pool slot0/liquidity/oracle writes.
Therefore a final post-swap pool snapshot cannot be substituted for call-time
state without a proof. Gauge tape recording has a separate fail-open boundary;
it can refresh on unchanged nonzero cumulatives without a `GaugeSettled` log.
LeafVoter writes are not established merely by the gauge's returned delta.

Reconstructing these groups' actual writes requires verified call outcomes with
exact inputs or authenticated covered post-state. Complete receipts alone are
insufficient. The possible conservative treatment below concerns proving a
trade without reconstructing irrelevant tape writes; it is a separate,
unimplemented capability, not permission to call stale tape cells exact.

## Source identity

The reviewed Base CL pool tape is
`0x27d5eb73e7406675f6b0cbd037490ec3dba935a4`, runtime keccak
`0x6e8c12f89d5bf6abe6abae4c2e11f3539349a7cb82fac167e183ff046e24c288`.
The source and compiler storage layout were fetched from
[Sourcify's verified contract](https://sourcify.dev/server/v2/contract/8453/0x27D5EB73e7406675F6B0CbD037490ec3dba935a4?fields=all)
and the runtime hash independently matched the crate's existing reviewed pin.
Relevant source owners are `CLPoolTape.sol`, `BasePoolTape.sol`,
`VolatilityRingLibrary.sol`, `DynamicSwapFeeHook.sol`, `CLPool.sol`, `Gauge.sol`
and `CLGauge.sol`. Source payloads used during development are retained outside
the package; permanent fixtures must retain their own source hashes and pins.

## Implemented narrow event surface

`ConcentratedLiquidityAdapter::derive_aero_execution_event` delegates to
`uniswap_v3/aero_execution/{block_fee,gauge,tape,voter}.rs`. It returns ordinary `StateUpdate`
values and typed errors. The caller must atomically combine them with existing
pool transitions. This method does not register new quote-mode subscriptions or
publish a complete execution generation.

- `GaugeSettled`: one present gauge cursor at slot2, exact positive delta and
  cumulative match. Pool/gauge proxy and implementation hashes are required.
- Tape `DefaultCadenceIntervalSet` and `PoolCadenceSet`: replace the uint32
  cadence at bit160, preserving the present owner/caller and padding.
- Tape `VolatilityRecorded`: replace the current observation's uint48
  volatility, the accumulator's uint48 volatility and uint8 count. All parent
  words and the ring index must be present; unrelated packed fields survive.
- A tape-emitted `Swap`, `ObservationRecorded` or other unsupported tape mutation never
  returns a successful empty transition.

The new Base gauge fixture records eight public canonical reads at block
53,203,953 (`0x1a069211d855484ee291187406227d5280fc967eb3c5d5c9b845945f61760b9b`),
including exact-hash parent/post cursor and code, all gauge settlement logs and
a closing canonical header. Its only settlement is log641 in transaction
`0xcbc5d0a580b4d31e5e0e21e8e7c0bf9e06019c566cefda55f43e075f2e1b145c`.
The recorded cursor changes from `0x46e53ffd1a24bdece` to
`0x483ea28a10e6a9aea`, exactly delta `0x159628cf6c1ebc1c`.

### Base voter: ordinary progression versus exceptional invalidation

`aero_execution/voter/{settlement,accounting}.rs` adds the ordinary Base
LeafVoter state progression. The baseline must contain all required parent
cells and the retained expiry schedule. There is no per-block read or implicit
repair. The same method rejects unsupported allocation/administrative events;
the publication owner must revoke readiness and require a separately authorized
repair instead of applying that error as a successful no-op.

| State group | Ordinary authority | Required publication interest |
| --- | --- | --- |
| global `index`, `timeIndex`, `lastSettlement` (slots3–5) and weekly maps6/7 | checked integration of the known slot2 emissions rate to canonical event timestamp, matching `_settleIndex` | every gauge's `GaugeCapSettled`, plus explicit invalidation on other entries which settle globals |
| global `surplusAccrued` (slot15) | add `GaugeCapSettled.surplusDelta` and `EmissionsForfeited.amount` | all gauges, not only the traded gauge |
| target gauge ceiling/claimed/surplus at mapping8 | exact event increments with checked uint128/headroom constraints | target `GaugeCapSettled`, `EmissionsMinted`, `EmissionsForfeited` |
| target gauge point and cursors | checked signed decay and known mapping9 expiry deltas; final weight must equal `GaugeCapSettled.gaugeWeight` | target settlement plus preserved exact starting point/schedule |
| pool reward growth, rollover and staked seconds | existing staking/pool reducer using `GaugeSettled` | gauge and pool ordered events, as before |
| gauge reward cursor | new exact `GaugeSettled` leaf | target gauge |

The ordinary reducer bounds both global and target gauge progression to two
crossed week boundaries, matching the existing Base execution preparation
horizon. A missing scheduled delta is missing state, never an implied zero.
The event's credited and surplus amounts are authoritative outputs of the
reviewed runtime, rather than re-estimates of emissions from end weight alone.
Signed slope decay, expiry reductions, defensive negative-weight clamp and
packed-field preservation follow `LeafAllocationLibrary` exactly. All returned
writes must be applied atomically. Input ordering/deduplication remains the
publication owner's responsibility, including non-target gauge events.

`GaugeRegistered`, `GaugeActivated`, `ChainStatusSet`, `ChainAllocated`,
`GaugesAllocated` and `EmergencyDeallocationApplied` are explicit invalidation
interests while their complete reducers are unavailable. They must not be
omitted merely because the normal swap path does not emit them. Some can be
implemented from complete payloads; allocation shape cannot be guessed:
`applyGaugeAllocations` consumes a calldata `TokenSnapshot(staked, stakeEnd,
isPermanent)` that `GaugesAllocated` does not emit. `applyChainAllocation`
similarly refreshes `latestTokenSnapshot`, while `ChainAllocated` omits it.
`IMessageOrchestrator.MessageReceived` exposes chain/nonce/type, not that body.
An already-known schedule therefore supports ordinary swaps until a relevant
unhandled mutation or missing input revokes the baseline. This is not a claim
that every future mutation can be reconstructed from local voter logs.

`Redeemed` does not mutate these voter storage groups and is excluded from this
owner's subscription interest; its token burn and bridge dispatch have other
owners. Similarly, a non-target `EmissionsMinted` has no effect on this target's
or global voter groups. A known no-op for this owner does not attest the other
contracts' state.

The voter source was independently matched to deployed Base runtime
`0x363145d181429cc6f7b7c55a7b332fedb5a06b44da5371acee8a94d34d140f35`
at `0x382409fad8503fbcdbb27a4211551fa03e884078`. Its linked library
`0xc7e262e848fca78c3c66ee3c9835e4e65e3e0398` matches runtime
`0x3759e4a45455ad769a000e151c3423658493e9be84015da913ea992c4c0e2d8e`.
Both hashes are checked by the reducer; Ethereum's voter identity is not
implicitly approved by the shared address.

`tests/fixtures/aero_execution/base-voter-rpc.json` retains 26 read-only RPC
elements: both runtime bytes, eleven exact-hash parent cells, eleven post
cells, a closing canonical header and all voter logs for the same block used
by the gauge fixture. That block contains 26 voter events, all settlement,
mint, forfeiture or redeem events. It contains no allocation/admin event.
The regression replays the relevant ordered events and compares every captured
post cell. This one block demonstrates ordinary continuity; it does not measure
how often allocation/admin invalidations occur or establish a production uptime
claim.

## Proposed conservative pool-tape capability — not implemented

The intended claim is narrower than full EVM state equivalence:

> For an approved executor swap whose entire economically relevant state is
> exact, replacing this one observational call outcome with failure after
> consuming the complete forwarded gas cannot make an accepted trade less
> solvent or reduce its on-chain token settlement, provided the conditions
> below hold. The simulation and cost estimate are conservative; tape state
> and tape logs are deliberately not predicted.

This claim still needs independent review and differential fork tests before
implementation. The existing quote stub is not a proof and is not reused.

### Identity and call predicates

The capability must be bound to a chain, pool family and exact runtime hashes:

| Role | Address | Runtime keccak256 |
| --- | --- | --- |
| CL pool implementation | `0x1642634c8cb1c9f1b84c9a1a08534f8c3f4e89f8` | `0x041172c7cbffc45627f9dd52727a0124cd6eeb2b5830ab392df54f71a6948693` |
| CL pool clone | reviewed Ethereum/Base pool only | `0x55e3735acd886d76160bfd959db31dd476e73e46e8a0d70ac36e4c53ef5a2a45` |
| Ethereum hook | `0xc0ed888afe87ed1acf01d9f19bb850159e3ce434` | `0xd64b9e760431c312c53515167ffb260b357705388da9354f31095d974e19e449` |
| Base hook | same address | `0x198b45ce022f423a9852526890d236531333e97861078501357e1c72f0eeecbd` |
| Pool tape | `0x27d5eb73e7406675f6b0cbd037490ec3dba935a4` | `0x6e8c12f89d5bf6abe6abae4c2e11f3539349a7cb82fac167e183ff046e24c288` |
| Ethereum factory | `0xf4fd2e6c5b60c5b365cd1b96dd6245e6d60414c2` | `0x3e53af145d8106d91fad73c6dbbbfe3229c5aa96aab1e18a4bf49a705c773ef5` |
| Base factory | `0x9cb220581b91b0c83223012fe69688b92ad07a76` | `0x6c09090837ace0aefe6bfa24a3bc772d13a9775b7e3109c9fece534a0ce053a6` |

At the exact call boundary, require `CALL`, source equal to the reviewed hook,
target equal to the reviewed tape, value zero, selector `0x11f14a52`
(`record(address,(uint128,uint128,uint128,uint128,uint128,uint128,uint128,uint128,int24))`),
canonical fixed-width ABI calldata and its pool argument equal to the reviewed
pool. Require the already-executed parent call to be this pool's `afterSwap`
path. Address or selector alone is insufficient. The factory's tape pointer,
hook binding and packed `recordCallGasLimit` must be exact current state; an
unknown change disables the capability.

Use the actual gas forwarded by EVM CALL after EIP-150 and account-access
charges, not a hard-coded gas constant. Preserve the caller's real ABI memory
work, CALL overhead, account warming, `require(gasleft() > recordCallGasLimit)`
and all code before and after the boundary. The only proposed abstraction is
the interior of that zero-value call. No general missing-state fallback exists.

### Source argument and observable state

`CLPool.sol` lines656–671 calls the reviewed hook after computing swap state
but before committing final slot0/liquidity/oracle cells. The hook's
`afterSwap` lines310–389 constructs tape calldata, checks its gas threshold,
then invokes `excessivelySafeCall` with `_maxCopy: 0`. The returned success flag
and data are unused. It subsequently clears transient MEV data. Tape success,
failure or its returned committed flag is not consulted by pool settlement.

`CLPoolTape.record` lines61–109 changes only tape-owned accumulator, observation
and volatility-ring storage. The observed branch calls only the reviewed
pool's `observe([0])` and `getSecondsPerStakedLiquidityCumulativeX128` getters
(`_writeObservation`, lines231–314). These execute as static calls, perform no
token transfer or mutable callback, and cannot change balances, pool state,
allowances, nonce or executor authorization. A getter revert makes the capped
tape call fail, which the hook already tolerates. The record path does not call
the mutable `elasticFeeModule`; that address only authorizes a separate setter.

All output groups of this call are therefore tape-owned storage, tape-owned
logs, returndata/success, gas usage/refunds and EVM access warmth. The reviewed
hook's dynamic fee reads the **pool** oracle (`_getFirstSwapFee` and
`_getDynamicFee`, lines567–666), not tape observations or accumulators. The
reviewed swap path and executor do not subsequently read the tape's metrics.
The claim excludes any batch operation or future protocol version that does.

Charging every forwarded gas unit, with no subcall refund, upper-bounds the
subcall's fee contribution. A failed call also retains no tape-state writes or
callee-scoped access warming. Actual success may warm additional cells and
reduce later gas. However, gas is observable: acceptance additionally requires
an audit of the entire remaining reviewed call graph showing that more gas
cannot choose a worse economic branch. Known outer `gasleft()` predicates are
minimum-budget guards. Unreviewed token code, callbacks, ownerCall operations,
other protocol calls or any new gas-dependent branch invalidate this argument.

The model must count each approved occurrence, including repeated swaps in a
batch, and debit the complete cumulative bound in profit and gas-limit checks.
It must not use the measured cheap stub gas, claim exact gas parity, subtract
unknown refunds, or label the hidden tape cells current. Strict-offline misses
everywhere else remain rejection.

### Decisive counterexamples and required qualification

- A tape runtime that can mutate the pool, transfer tokens or invoke an
  arbitrary callback defeats noninterference even when the hook ignores return
  data. Unknown hashes and proxy target changes must reject.
- A caller that branches on success/returndata, pays value, copies unbounded
  returndata, or later consumes the recorded tape fields defeats the claim.
- Merely returning zero from cheap replacement code underestimates gas and
  does not prove the outer gas guards. Merely increasing a configured gas cap
  without charging it to executable economics is also invalid.
- Using final post-swap state to derive call-time tape getter output is wrong:
  tape getters run before final pool storage writes. The proposed capability
  deliberately avoids claiming those output values.
- An unknown hook gas limit, a stale factory binding, an unexpected caller or
  malformed ABI must never enter this boundary.
- Passing isolated swap tests does not cover repeated calls, near-OOG branches,
  access-list warmth, refund limits, gauge crossings or a later tape consumer.

Qualification must compare exact full-state fork execution to the conservative
execution on both chains, both directions, same calldata and gas limits, with
cadence hit/miss, authorized/unauthorized tape caller, first record, ring growth,
multi-swap batches, deliberate tape revert/OOG and near-threshold parent gas.
Accepted conservative results must preserve non-tape writes, token settlement,
executor nonce and permissions, and bound charged gas above exact execution.
Failure cases must demonstrate no abstract result when predicates are altered.
This does not by itself solve LeafVoter rewards, vault state, fee cells,
transaction-specific discounts or complete ordered event delivery.

The gauge tape has a similar source boundary in
`CLGaugeFactory.recordClGaugeTape` lines296–309, but it is a separate capability
with separate factory/tape identities and gas predicates. This document grants
no authority to abstract `CLGauge.settleGauge` or `LeafVoter.settleGauge`:
their returned emissions mutate pool reward accounting and are not irrelevant.

Source SHA-256 values used for this assessment:

| Source | SHA-256 |
| --- | --- |
| `CLPoolTape.sol` | `bdb365b98b57c85100739f7c12cb2cf68b47e4b58119424798c2b4d98c7d9560` |
| `BasePoolTape.sol` | `58f38cce80f1fe2f55204aa3065e842e559fa233016481096fe7981bdc287cc6` |
| `VolatilityRingLibrary.sol` | `68965b556f1c020e2d450f1023e5b51d0e107b383530ef0400549fbd730c56a7` |
| reviewed `new-cl.sol` | `f557066247e8fe493671f030a986833d5fb5c07f3bc7a7d3f8da9771e3e91b48` |
| reviewed `new-hook.sol` | `97ff4bc4e5fba60018b940b5900d7b05579bb58494ac7a420a720aa620b745ed` |

## Exact raw first-swap fee

`aero_execution/block_fee.rs` and `block_fee/oracle.rs` derive the raw value
stored in `DynamicSwapFeeHook.blockFee[pool][height]` before the first swap.
They port the reviewed hook's `_getFirstSwapFee` and tick-only oracle
calculation. This value is independent of the initial transaction fee,
caller discount and later MEV fee. Inferring it from an effective Swap fee is
incorrect. The hook's `beforeSwap` call is not fail-open: a successful pool
Swap with the exact selected reviewed hook implies that this call returned.

The transition consumes an attested pre-event immutable state, exact canonical
context, pool factory and factory hook bindings, the present block-fee cell,
packed pool configuration/defaults, mutable lookback and the selected pool
oracle words. Oracle search takes at most 16 iterations for the uint16 ring;
it preserves Solidity signed-width wrapping, interpolation division order,
negative-infinity TWAP rounding and checked hook arithmetic. Missing words or
unattested identities reject. There is no provider fallback. A nonzero stored
fee is retained without recomputing from the already changed pool tick.

The canonical owner must establish that a new-height zero is valid before
materializing it. It must combine the fee write atomically with the ordinary
pool transition, using the state from before that Swap. Receipt ordering alone
cannot disambiguate a transaction in which an authorized callback changes hook
or factory controls after `beforeSwap` but before the outer Swap log. Such a
transaction needs a call-order proof or fails closed; ordinary log order must
not silently authorize the wrong control snapshot. Likewise, an already
processed Swap in the current height prohibits resetting its stored fee to
zero. Restart and reorg owners must restore the canonical baseline, rather
than reuse an unproven prior-branch block key.

The new fixtures retain 24 public RPC elements each, all code/storage reads
pinned to the parent hash, all relevant pool/hook/factory block logs, the actual
post-block stored fee and a closing canonical header:

| Chain | Block | Parent hash | Block hash | Actual raw fee |
|---|---:|---|---|---:|
| Ethereum |26,291,964|`0x0282c03339b7c40609d23e73e47819275fe480e1d8454b716c970104dda62a22`|`0x84f506d4bd095934c592cbefe44c4431633c66e9d357af29660f50ea0f894d0e`|3,000|
| Base |53,203,896|`0xd29876b016f9e2a5ff4d4ac830c6023d56108090ef3ec42b5bd94b08c5d5fbd8`|`0x351180ceb550f2a8b2ec4f7069ae7443217853d915f7bbda7a9dc07a556b3016`|10,000|

The archives are `tests/fixtures/aero_execution/{ethereum,base}-first-swap-fee-rpc.json`.
Neither canonical frame needs a historical binary search: their target can be
computed from the latest observation. Maximum-ring search and negative TWAP
rounding are separate deterministic model tests, not canonical replay claims.
Transport failures from the development capture are retained under
`/tmp/aero-cl-source/*fee-failed-*.json`; they are not silently counted as
successful capture elements or production provider evidence.

The focused governance child `aero_governance/initial_fee.rs` also converts the
reviewed `InitialFeeSet(address,uint24)`, `InitialFeeDisabled` and
`DynamicFeeReset` events into exact packed updates. It preserves unrelated
bits; reset preserves base fee and removes cap/scaling/initial configuration.
The alternate unreviewed uint256 event stays an invalidation. Existing event
interests already include these topics, so this adds no subscription or RPC.

This is a narrow reducer capability. The shared publication owner must still
supply actual code attestations, complete canonical event coverage and the
transaction ambiguity rejection. It does not by itself prove normal-worker
readiness, tape execution completeness, or release qualification.

### Validation of this reducer change

With Rust 1.90.0 and the exact external source-only cache-alpha qualification
configuration, final focused results are 25/25 CL execution tests, 9/9 governance
tests and 15/15 existing `aero_cl_family` integration tests. Strict
`clippy --all-targets --all-features -- -D warnings` passes. The governance
suite includes four new packed-write and invalidation regressions; the CL
execution suite includes ten raw-fee/oracle tests in addition to fifteen
existing gauge/tape/voter tests. The final oracle cast/floor order was included
in these passing results. An independent read-only source review found no
arithmetic parity defect, while preserving the caller obligations above.

These are reducer and existing fixture checks, not a completed event-only
worker release gate, performance distribution, production continuity proof or
permission to deploy. The shared publisher must exercise the composed state
transition, ambiguity rejection and lifecycle boundaries before qualification.

## Canonical log evidence retained for consumers

`AmmStateCommit::canonical_logs()` exposes the exact ordered RPC log envelopes
validated by its `AmmCanonicalBatch`, paired with the existing sealed canonical
header. A source-closed empty batch is `Some(&[])`; header-only, lifecycle,
prepared-state and repair commits provide `None`. The canonical constructor
rejects more than 4,096 logs, more than four topics per log, or more than 2 MiB
of retained data/topics plus 256 bytes of metadata allowance per log. It never
truncates an accepted batch. The record limit matches the existing observation
and ingress-timing ledgers; stricter source `max_pending_records` limits still
apply independently. The byte accounting concerns retained binary envelopes,
not JSON wire bytes or transport billing.

This preserves existing source closure, chain, pin, canonicality and ordering
validation. It does not add receipt-root authentication, prove silent storage
writes or let lifecycle commits borrow prior block evidence. Consumers must
still enforce uninterrupted runtime/version/interest/parent provenance and
source incidents before applying local execution reducers.
