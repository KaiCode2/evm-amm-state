# Aero (Dromos) families

Status: release candidate `0.3.1-alpha.8`. Everything below was read with
hash-pinned calls at Ethereum 26292020
(`0xc7d9bbfa9eeec36423d72a7b193ec991d0e92ca56d0a6a1d3714de36965d44af`),
Base 53205395
(`0x82167d8751d4e3731d238bb99c86a45cd84722760cbf77912a8b020dd23a40df`) and
Optimism 158800699
(`0x69f98edbc36470a6954f5e1f2522c698666f30cb812775c206c2f6b8869b0666`),
and is exercised by `tests/aero_parity_rpc.rs`, `tests/aero_replay_rpc.rs`
(`.github/workflows/live-aero.yml`) and the long-window
`tests/aero_campaign_rpc.rs` (`scripts/stage10-aero-campaign.sh`).

## Registering the pools

```rust
use evm_amm_state::adapters::{
    PoolKey, PoolRegistration, ProtocolMetadata, SimConfig, SolidlyV2Metadata, V3Metadata,
    V3StorageLayout,
};

// Ethereum BIFI/WETH, tick spacing 50 (Base: 0xEEBE78d7…, spacing 100).
let aero_cl = PoolRegistration::new(PoolKey::Slipstream(pool))
    .with_state_address(pool)
    .with_metadata(ProtocolMetadata::Slipstream(
        V3Metadata::default()
            .with_token0(token0)
            .with_token1(token1)
            .with_tick_spacing(50)
            .with_storage_layout(V3StorageLayout::aero_cl(50))
            .with_metaquoter(metaquoter), // 0x1dDfeC50… on every chain
    ));

// Optimism Aero volatile WETH/mooBIFI.
let aero_volatile = PoolRegistration::new(PoolKey::SolidlyV2(volatile_pool))
    .with_state_address(volatile_pool)
    .with_metadata(ProtocolMetadata::SolidlyV2(
        SolidlyV2Metadata::default()
            .with_token0(weth)
            .with_token1(moo_bifi)
            .with_aero_volatile(),
    ));

// Quotes run under the candidate's transaction context.
let cl_config = SimConfig::default()
    .with_tx_origin(bot_eoa)
    .with_effective_gas_price(basefee, priority_fee);
// The volatile pool getter receives its caller as the fee payer.
let volatile_config = cl_config.with_from(executor);
```

The Aero CL pools register under `PoolKey::Slipstream`; the adapter
classifies the family from `V3StorageLayout::slipstream_family` (legacy
Aerodrome/Velodrome core slots versus Aero core slots) and from the reviewed
pool address. A legacy layout on an Aero pool, an Aero layout on a legacy pool,
a wrong tick spacing or an unreviewed address all fail closed.

## Storage layouts

| Cell | Legacy Slipstream | Aero CL |
| --- | ---: | ---: |
| `slot0` | 6 | 6 |
| fee growth 0 / 1, reward growth, gauge fees | 7 / 8, 9, 10 | 7 / 8, 9, 10 |
| rollover | 14 | 11 |
| `stakedLiquidity` (low 128) + `lastUpdated` + `tickSpacing` | 15, `uint32` | 12, `uint48` |
| `liquidity` (low 128) + `maxLiquidityPerTick` | 16 | 13 |
| `ticks` (six words, same order) | 17 | 14 |
| `tickBitmap` | 18 | 15 |
| `observations[65535]` | 20 | 17 |
| `secondsPerStakedLiquidityCumulativeX128` | — | 65552 |

Aero volatile pool: `reserve0` 196619, `reserve1` 196620, `token0` 7,
`token1` 8, `factory` 10 (`SolidlyStorageLayout::aero_volatile`).

## Reviewed identities

Shared: CL implementation `0x1642634C…` keccak `0x041172c7…` (EIP-1167
proxies, proxy runtime keccak `0x55e3735a…`), discount registry `0xaA48AAef…`
keccak `0x0b972ebf…`, pool tape `0x27D5EB73…` keccak `0x6e8c12f8…`, gauge
proxy keccak `0xb03fb822…` to implementation `0x89e91a96…` keccak
`0xfdb516e0…`.

| | Ethereum (`AeroEthereumBifi`) | Base (`AeroBaseMooBifi`) |
| --- | --- | --- |
| pool | `0xff35aa95…` | `0xEEBE78d7…` |
| factory | `0xF4Fd2E6c…` `0x3e53af14…` | `0x9CB22058…` `0x6c090908…` |
| hook `0xC0ed888A…` | `0xd64b9e76…` | `0x198b45ce…` |
| MEV-tax module | none | `0x29F12cE5…` `0xfb10c802…` |
| unstaked-fee module `0xcc421483…` | `0x90c9628c…` (wired) | `0xe3ec637a…` (present, not wired) |
| gauge | `0x0eE89BDf…` | `0x84C029F2…` |

Warmed but not pinned (proof-verified on the prepared path): LeafVoter
`0x382409fa…`, the Metaquoter's factory registry (`0xdd1705e2…` /
`0x7b2A2c12…`), gauge factory `0xf437f620…` and the two gauge-settlement
contracts `0x0575b823…` / `0xc7e262e8…`.

Aero volatile (`AeroOptimismMooBifi`): pool `0xC71B571a…` proxy `0x7604f8b1…`,
implementation `0x0526cF03…` `0x15269fbb…`, factory `0x8E6a9FCd…`
`0xbb6f9454…`, fee module `0x7949DE8d…` `0xa56614fe…`, exact-output fee
quoter `0x95F6B665…` `0xd102c04e…`, MEV-tax module `0x5De55Ebe…`
`0x59de6c7e…`.

`AmmAdapter::reviewed_code_identities` exposes the pins; cold start fails the
pool closed (`ColdStartOutcome::Unsupported`) when a resident or fetched
runtime hashes differently, and `ConcentratedLiquidityAdapter::validate_aero_cl_runtime`
checks a snapshot.

For the Optimism volatile pool, the prepared worker proves the constant
`POOL_TYPE` getter from the exact-block runtime hashes of the fixed proxy and
its implementation on chain 10. Storage-only cold starts execute the getter.
Strict and eager preparation require a successful exact-block read for each
reviewed fee cell. A failed or missing result returns the cell for repair
even when the previous block's value is resident.

## Warmed cells

`verified_storage_targets` returns the cells the quote path reads whose keys
are fixed by the registration (observed with `debug_traceCall` prestate
traces of the Metaquoter quotes):

- factory: `voter` 4, `unstakedFeeModule` 8, `defaultSwapHook` 9,
  `swapFeeManager`/`defaultUnstakedFee` 11, `discountRegistry` 12,
  `clPoolTape` 13, `poolSwapHook[pool]`, `tickSpacingToFee[spacing]`,
  `isPool[pool]`;
- hook: `defaultScalingFactor` 0, `defaultFeeCap` 1,
  `secondsAgo`/`recordCallGasLimit` 2, `mevTaxModule` 5,
  `dynamicFeeConfig[pool]`;
- MEV-tax module (Base): slots 1 and 2; unstaked-fee module (Ethereum):
  `customFee[pool]`; LeafVoter `gauges[gauge]` liveness word; gauge factory
  per-gauge cell; pool tape `pool` hook word and five-word record;
- with the Metaquoter registered: factory registry `targetToFactory[pool]`,
  `poolToGauge[pool]`, `gaugeToFactory[gauge]` and the discount registry
  cell of the Metaquoter.

Two cells cannot be derived from the registration alone and are exposed as
helpers for exact-block hydration (see [Strict snapshot quotes](#strict-snapshot-quotes)):

- `ConcentratedLiquidityAdapter::aero_cl_block_scoped_storage_targets(pool,
  block)` / `aero_cl_block_scoped_materialization`: the hook's
  `blockFee[pool][block]` cell, read first on every swap; zero for a block in
  which the pool has not yet swapped.
- `ConcentratedLiquidityAdapter::aero_cl_discount_storage_target(pool,
  account)`: the discount registry's `discounts[account]` cell for the swap
  caller and for `tx.origin`.

The pool tape's period-indexed records and the gauge settlement path a
tick-crossing swap executes are stubbed out of snapshot quotes
(`aero_cl_quote_code_overrides`); live-backed caches execute the real
runtimes and still warm them.

Aero volatile: factory `defaultFee` 5, `feeModule` 7, `mevTaxModule` 13,
`exactOutFeeQuoter` 14; fee module `customFee[pool]`; MEV module slots 1 and
2; pool `factory` 10. The factory's `getFee` does not consult the discount
registry at the pin. The volatile adapter routes pool `Sync` events.
Consumers must verify the fee cells and their active module bindings at the
quote block or invalidate them on governance changes before reusing a quote.

## Quotes and the transaction context

`V3QuoterKind::Metaquoter` quotes through
`Metaquoter.quoteExactInputSingleV3(pool, tokenIn, amountIn, 0)` and
`simulate_swap_exact_output` through `quoteExactOutputSingleV3`; both execute
`pool.swap` and therefore the swap hook, the MEV-tax module (`tx.gasprice`
against `block.basefee`), the discount registry (`tx.origin` once the hook's
caller is the Metaquoter) and the pool tape. The Aero volatile variant quotes
through `getAmountOutWithTotalFee` / `getAmountInWithTotalFee`, whose fee is
`factory.getFee(pool, msg.sender, …)` including the MEV tax (30 bps up to a
1 gwei tip, 10% cap from 1.1 gwei at the pin).

`SimConfig::with_effective_gas_price(basefee, tip)` and `with_tx_origin`
carry that context. A cache facade that cannot honour a configured gas price
fails the quote (`AdapterCache::call_raw_with_context` default) instead of
quoting at a zero gas price; `EvmCache`, the quote read-set recorder and
`overlay_call_raw_with_context` honour it.

## Event replay

Ordered `Swap`, `Mint`, `Burn`, `Collect`, `CollectFees` and
`IncreaseObservationCardinalityNext` on the reviewed Aero CL pools replay
exactly on the quote/search surface: slot0, active liquidity, the
staked-liquidity bound, the observation ring (including reservation growth),
both boundary ticks (six words) and their bitmap words, and the packed
`gaugeFees` word after a gauge fee claim. `tests/fixtures/*aero_cl*.json`
replay real transactions, including nineteen-event CLM rebalances and the
gauge flows below, against the chain's post-state.

### Gauge staking

The pool emits nothing when the gauge stakes or unstakes a position. The
reviewed Aero gauges (`0x0eE89BDf…` Ethereum, `0x84C029F2…` Base) and the
shared NonfungiblePositionManager `0x4857db57…` (pinned per chain) jointly
attest every staking transition, exactly as the legacy Optimism module does:

- `Deposit(sender, owner, tokenId, liquidityToStake)` applies at the gauge
  event; `Withdraw(sender, account, tokenId, liquidityToStake)` applies at
  the NFT `Transfer(gauge → sender, tokenId)` that precedes it, so a receiver
  callback cannot swap against stale staked state. The range comes from the
  pool `Collect` the gauge's `nft.collect` emitted two logs before the
  manager's `Collect(tokenId, …)` (owner = manager on deposit, gauge on
  withdrawal); the custody transfer and the collect recipient must agree
  with the gauge event. Ambiguity inside a transaction invalidates its proofs
  and the event fails closed into a repair.
- Settlements are events of their own. Every `GaugeSettled(cumulative,
  delta)` the gauge emits is the pool's `_settleToBlock` at that log: `delta`
  is credited to `rewardGrowthGlobalX128` per unit of the liquidity staked at
  that moment (or banked into `rollover` with nothing staked),
  `secondsPerStakedLiquidityCumulativeX128` advances by the elapsed seconds
  per staked unit and `lastUpdated` (`uint48`) becomes the block timestamp.
  A `stake` in range, a tick-crossing swap, an emission claim
  (`claimEmissions`, the claim step of every withdrawal) and `forfeitRollover`
  all settle this way; the last three run `pool.settleToBlock()`, which also
  clears `rollover`, so a `GaugeSettled` that no deposit window or following
  `Swap` claims is applied with `rollover = 0`. A claim whose gauge accrued
  nothing emits no `GaugeSettled` but still settles silently and clears
  `rollover`: the emission-claim logs (`EmissionsClaimed`,
  `EmissionsDeferred`, `ReferralEmissionsDeferred`,
  `DeferredEmissionsClaimed`) carry that transition. Stakes themselves settle
  silently (a withdrawal unconditionally, a deposit when in range); a block
  that already settled changes nothing.
- The staked word (slot 12), both ticks' `stakedLiquidityNet` (word 1),
  slot 9, slot 11 and slot 65552 are therefore exact after every rebalance
  and claim without a repair (`tests/fixtures/base_aero_cl_gauge_rebalance.json`,
  `ethereum_aero_cl_gauge_{deposit,withdraw}.json`).

Live subscriptions (`event_sources`) include the gauge's `Deposit`,
`Withdraw`, `GaugeSettled` and emission-claim events and the manager's
`Transfer` from the gauge; the complete-block driver fetches the manager's
`Transfer` / `Collect` logs for the staked token ids. Historical replay builds
the same evidence from a block's ordered logs with
`SlipstreamStakingBatch::from_block_logs`. `EmergencyWithdraw` (never
observed) has no evidence and fails closed through the custody transfer; a
claim that accrued nothing and emitted no claim event settles the pool
unobservably (measured: none in the campaign windows).

### Swap accounting (fee evidence)

With `SlipstreamSwapFeeEvidence` for an Aero family the swap transition also
reproduces the accounting writes: `feeGrowthGlobal0/1X128` and the packed
`gaugeFees` word from the event-derived effective fee split by the exact
unstaked fee, and on a tick-crossing swap the silent settlement (the
`GaugeSettled` a crossing swap pulls is applied at its own, earlier log, so
`gauge_settlement_delta` stays zero whenever gauge logs are routed; a consumer
that does not route them passes the transaction's delta instead) followed by
the six words of every crossed tick.
`evaluate_slipstream_unstaked_fee` evaluates `pool.unstakedFee()` /
`factory.getUnstakedFee(pool)` on the exact parent snapshot with the pinned
identities (Ethereum: the wired module's `customFee[pool]`, 0 at the pins;
Base: the factory default, 5%), and `infer_slipstream_swap_fee` derives the
effective fee from the event. Fee evidence is optional: without it the
quote/search surface is still exact and only the accounting words drift.

### Governance

Configuration the quote path reads is replayed from the setters' events
(`AdapterEventKind::Unknown`, exact writes, confirmed against every event the
contracts emitted since deployment on both chains):

| emitter | event | effect |
| --- | --- | --- |
| hook | `CustomFeeSet(pool, uint24)`, `FeeCapSet(pool, uint256)`, `ScalingFactorSet(pool, uint256)` | `dynamicFeeConfig[pool]` bits 0..24, 24..48, 48..112 (packing confirmed on live pools) |
| hook | `DefaultScalingFactorSet`, `DefaultFeeCapSet`, `SecondsAgoSet`, `RecordCallGasLimitSet` | slots 0, 1, 2 bits 0..32, 2 bits 32..64 |
| hook | `InitialFeeSet`, `InitialFeeDisabled`, `DynamicFeeReset` | unconfirmed packing: `dynamicFeeConfig[pool]` is purged and verified |
| hook | `MevTaxModuleSet` | re-setting the reviewed module is exact; any other module purges the pool (re-review) |
| MEV module | `MultiplierSet(uint64)`, `MinThresholdSet(uint96)`, `BaseFeeFactorSet(uint96)` | slot 1 bits 160..224, slot 2 bits 0..96, slot 2 bits 96..192 (confirmed by state-override calls) |
| factory | `TickSpacingEnabled(spacing, fee)`, `DefaultUnstakedFeeChanged`, `SwapFeeManagerChanged` | `tickSpacingToFee[spacing]`, slot 11 bits 160..184, slot 11 bits 0..160 |
| factory | `DefaultSwapHookChanged`, `PoolSwapHookChanged(pool)`, `UnstakedFeeModuleChanged`, `DiscountRegistryChanged`, `ClPoolTapeChanged` | a reviewed runtime binding changed: the pool is purged and its next cold start re-checks identities |
| discount registry | `DiscountSet(account, uint24)`, `PoolDiscountSet(pool, account, uint24)`, `0xc5dba97f…` | the named accounts' `discounts[account]` cells are purged and verified |
| unstaked-fee module (Ethereum) | `CustomFeeSet(pool, uint24)` | `customFee[pool]` is purged and verified |

Hook events carry the pool as `topic1`; setters for other pools never route
to a reviewed registration.

## Strict snapshot quotes

A Metaquoter quote over an immutable snapshot (no provider, no lazy fetch)
resolves every read locally when the snapshot holds, besides the cold-start
state above:

- the pool tape and the pool's gauge are replaced by a zero-returning stub
  (`aero_cl_quote_code_overrides`): neither can change the quoted amounts
  (`record` return data is discarded; `settleGauge` only feeds reward
  accounting), so their period-indexed / settlement cells are not read;
- `blockFee[pool][height]` for the simulated height
  (`aero_cl_block_scoped_storage_targets`): zero before the pool's first swap
  of that block (`aero_cl_block_scoped_materialization`), otherwise the hook's
  stored first-swap fee `min(baseFee + dynamic, feeCap)`, which must be
  hydrated from the chain when quoting inside a block that already swapped;
- `discounts[msg.sender]` and `discounts[tx.origin]` on the discount registry
  (`aero_cl_discount_storage_target`): the hook applies the registry's
  discount for the swap caller (the Metaquoter, warmed) and for the
  transaction origin;
- the account headers of `tx.origin`, the block's coinbase and the path
  tokens (cold start warms the tokens; `EvmCache::ensure_account` the rest).

`tests/aero_campaign_rpc.rs` measures this: at every check block the quote
runs on such a snapshot and every missing cell is reported by contract and
slot.

## Replay campaign

`scripts/stage10-aero-campaign.sh` (`tests/aero_campaign_rpc.rs`) replays
long windows (Ethereum 20 000 blocks, Base and Optimism 100 000 blocks by
default, ending near head) through the driver with staking and fee evidence
built from the chain's logs, and at every event block, the block after it,
every `AERO_CAMPAIGN_STRIDE`-th block (default 100) and the window end:

- reads the complete quote-relevant pool state with one `eth_call` storage
  program (statics, every bitmap word, all six words of every initialized
  tick, the whole observation ring) plus the reviewed external cells and
  classifies every divergent cell;
- quotes both directions and exact output at 1 and 5 mooBIFI-equivalent
  under tips 0 and 1 gwei (Optimism: 10 000 001 wei, 1.05 and 1.1 gwei) on
  the strict snapshot, wei-exact against `eth_call` at that block, and once
  per check block with a synthetic non-zero scaling factor (state override on
  both sides) so the hook's `observe`-based dynamic fee is exercised;
- counts adapter repairs, evidence failures, campaign re-syncs by cause and
  the `blockFee` hydration outcome, and writes `aero-campaign-<chain>.{json,md}`
  under `AERO_CAMPAIGN_REPORT_DIR`.

The short replay includes the gauge's transaction receipts when checking CL
staked liquidity. The campaign writes its diagnostics before failing a run
with divergent storage, repairs, missing evidence, or unmatched quotes.

## Known gaps

- The Optimism Aero CL deployment (USDC pool, hook `0x3064a620…`) and the
  Aero factories' discovery (`getPool`) are not reviewed in this release.
- `EmergencyWithdraw`, the hook's initial-fee setters and dynamic-fee reset,
  and discount-registry writes have no confirmed packing: they fail closed
  into a verify/purge of the affected cells instead of an exact write.
- `blockFee[pool][height]` inside a block that already swapped is hydrated,
  not derived.
