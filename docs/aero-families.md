# Aero (Dromos) families

Status: release candidate `0.3.1-alpha.8`. Everything below was read with
hash-pinned calls at Ethereum 26292020
(`0xc7d9bbfa9eeec36423d72a7b193ec991d0e92ca56d0a6a1d3714de36965d44af`),
Base 53205395
(`0x82167d8751d4e3731d238bb99c86a45cd84722760cbf77912a8b020dd23a40df`) and
Optimism 158800699
(`0x69f98edbc36470a6954f5e1f2522c698666f30cb812775c206c2f6b8869b0666`),
and is exercised by `tests/aero_parity_rpc.rs` and `tests/aero_replay_rpc.rs`
(`.github/workflows/live-aero.yml`).

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
let config = SimConfig::default()
    .with_tx_origin(bot_eoa)
    .with_effective_gas_price(basefee, priority_fee);
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
helpers for exact-block hydration:

- `ConcentratedLiquidityAdapter::aero_cl_block_scoped_storage_targets(pool,
  block)`: the hook's `blockFee[pool][block]` cell, read first on every swap.
  It is zero for a block in which the pool has not yet swapped.
- `ConcentratedLiquidityAdapter::aero_cl_discount_storage_target(pool,
  account)`: the discount registry's `discounts[account]` cell for the
  executor and for `tx.origin`.

The pool tape's period-indexed records (slots 4 and 6) and the gauge
settlement path a tick-crossing swap executes are learned by quote read-set
warming (`warm_quote_read_sets`) and refreshed by `hydrate_quote_read_sets`.

Aero volatile: factory `defaultFee` 5, `feeModule` 7, `mevTaxModule` 13,
`exactOutFeeQuoter` 14; fee module `customFee[pool]`; MEV module slots 1 and
2; pool `factory` 10. The factory's `getFee` does not consult the discount
registry at the pin.

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

Ordered `Swap`, `Mint`, `Burn` and `Collect` on the reviewed Aero CL pools
replay exactly on the quote/search surface: slot0, active liquidity, the
staked-liquidity bound, the observation ring, both boundary ticks (six words)
and their bitmap words. `tests/fixtures/*aero_cl*.json` replay real
transactions, including nineteen-event CLM rebalances, against the chain's
post-state.

## Known gaps

- Gauge `stake` / `unstake` emit no pool event. After a staked position is
  burned, the cached `stakedLiquidity` and the ticks' `stakedLiquidityNet`
  stay stale until a swap replay observes `stakedLiquidity > liquidity` and
  fails closed into a repair. The Base pool is almost fully staked, so each
  CLM rebalance costs one repair; Ethereum (0.2% staked) is unaffected. Aero
  gauge staking evidence (the counterpart of the Optimism legacy module) is a
  follow-up.
- No runtime-bound accounting evidence: fee growth, gauge fees, reward growth,
  `lastUpdated`, `secondsPerStakedLiquidityCumulativeX128` and crossed-tick
  outside accumulators are not reproduced (they do not affect quotes).
- The Optimism Aero CL deployment (USDC pool, hook `0x3064a620…`) and the
  Aero factories' discovery (`getPool`) are not reviewed in this release.
- `blockFee[pool][block]` and the tape's period-indexed cells are not part of
  cold start; see the helpers and read-set warming above.
