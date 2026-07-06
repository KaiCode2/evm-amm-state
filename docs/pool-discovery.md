# Pool discovery

`evm-amm-state` can build cold-start-ready [`PoolRegistration`]s from *configured
factories and a token set* — you name the factories and the tokens you care
about, and discovery resolves the actual pool addresses. No pasted pool
addresses, no hand-maintained pool lists.

Discovery is deliberately **read-only**: it produces registrations, and the
caller still decides when to cold-start and register them. The happy path is
`find(PoolQuery::basket(..)) → cold_start_many → register` (see
[`examples/declarative_discovery.rs`](../examples/declarative_discovery.rs) and
[`examples/factory_discovery_live.rs`](../examples/factory_discovery_live.rs)).

## Declarative queries

The whole surface is one method — `PoolDiscovery::find(cache, query)` — over one
fluent [`PoolQuery`]:

```rust,ignore
use evm_amm_state::adapters::{FactoryConfig, PoolDiscovery, PoolQuery, ProtocolId};

// Wire the configured factories to the registered adapters.
let discovery = PoolDiscovery::for_registry(&registry, config);

// one token pair, across every matching factory (V2, V3, forks, …)
discovery.find(&mut cache, PoolQuery::pair(weth, usdc))?;

// every pool joining any pair of a token basket — the C(n, 2) combinations
discovery.find(&mut cache, PoolQuery::basket([weth, usdc, usdt, dai, wbtc]))?;

// an explicit set of pairs
discovery.find(&mut cache, PoolQuery::pairs([(weth, usdc), (weth, dai)]))?;

// scope any of the above to one protocol
discovery.find(&mut cache, PoolQuery::pair(weth, usdc).on(ProtocolId::UniswapV3))?;
```

Each constructor — [`pair`], [`basket`], [`pairs`] — normalizes token order
(canonical `(token0, token1)`) and de-duplicates. [`basket`] expands to the
`C(n, 2)` unordered pairs of the token set.

**One batched read.** An unscoped query spans **every** matching factory and
resolves the whole thing — all pairs, all factories, all V3 fee tiers — in a
**single batched read** ([`AdapterCache::read_storage_slots`], one bulk
`eth_call` on an `EvmCache`). Request count scales with the number of factories,
not with pairs, fee tiers, or basket size (a 5-token mainnet basket resolves 49
pools in one round-trip, ~20× faster than per-pair scans; see
[`examples/token_basket_bench.rs`](../examples/token_basket_bench.rs)).

**`find` vs `find_many`.** [`find`] resolves a single [`PoolQuery`].
[`find_many`] resolves several queries together — each scoped independently, so
one call can mix protocols across pairs (some pairs only on Uniswap V2, others
only on V3) — in that *same* single batched read, returning the de-duplicated
union:

```rust,ignore
discovery.find_many(&mut cache, [
    PoolQuery::pairs([(weth, usdc)]).on(ProtocolId::UniswapV2),
    PoolQuery::basket([weth, dai, wbtc]).on(ProtocolId::UniswapV3),
])?;
```

**Scoping and errors.** `.on(p)` filters to protocol `p` and errors
[`DiscoveryError::MissingFactory(p)`] when no factory is registered for it. An
unscoped query never errors on a missing factory (an empty pair set simply
yields `Ok(vec![])`).

`FactoryConfig` is empty by default: callers opt in with explicit factory
addresses (`FactoryConfig::default().with_uniswap_v3_factory(factory)`), so chain-
and fork-specific deployments never inherit an assumed factory address.

## Protocol coverage

Each supported protocol resolves through the pinned cache by one of two
mechanisms: a **DerivedSlot** read (a Rust-computed factory storage slot,
resolved in the batched `read_storage_slots`) or a **ViewCall** (an on-chain
`view` executed in revm through [`AdapterCache::call_raw`]).

| Protocol | Mechanism | Notes |
| --- | --- | --- |
| Uniswap V3 / Sushi V3 / Pancake V3 | DerivedSlot (fee-keyed `getPool[t0][t1][fee]`) via [`ClFactorySpec`] | per-fork base slots / fee tiers / quoter; fork presets (`uniswap_v3`, `sushi_v3`, `pancake_v3`) |
| Slipstream / Aerodrome CL | DerivedSlot (tickSpacing-keyed `getPool[t0][t1][tickSpacing]`) via [`ClFactorySpec`] | discovery only — its quoter ABI differs (int24 tickSpacing), so quoting rides a Uniswap-compatible quoter |
| Uniswap V2 | DerivedSlot (`getPair[t0][t1]`) | one pool per pair |
| Solidly V2 (Aerodrome / Velodrome) | DerivedSlot (`getPool[t0][t1][bool stable]`) | stable **and** volatile — a pair yields up to two pools |

Multiple factories of the same protocol coexist — identity is
`(protocol, factory_address)`, so Uniswap and a Sushi-style fork both resolve,
and an exact duplicate is dropped (first wins).

The V3-family entries are all one config type, [`ClFactorySpec`], driving one
[`ConcentratedLiquidityFactory`]; the fee-keyed vs tickSpacing-keyed distinction
is [`ClKeying`]. Solidly is [`SolidlyFactoryConfig`] / [`SolidlyFactory`].

## Adding your own concentrated-liquidity fork

A UniV3-mechanics fork this crate does not ship a preset for is just a
[`ClFactorySpec`] you fill in — no new adapter, no `src/` edit. It reuses the
[`ConcentratedLiquidityAdapter`] for simulation and the
[`ConcentratedLiquidityFactory`] for discovery. Pick the keying that matches the
fork's `getPool` mapping:

```rust,ignore
use alloy_primitives::U256;
use evm_amm_state::adapters::{ClFactorySpec, FactoryConfig, FeeSource, ProtocolId};

// A fee-keyed fork (getPool[t0][t1][fee], spacing from feeAmountTickSpacing[fee]):
let spec = ClFactorySpec::fee_keyed(
    ProtocolId::UniswapV3,          // the identity discovered pools register under
    my_fork_factory,                // the factory contract
    U256::from(5),                  // getPool base slot
    U256::from(4),                  // feeAmountTickSpacing base slot
    [100, 500, 3_000, 10_000],      // fee tiers to probe
)
.with_quoter(my_fork_quoter)        // a fork's own Uniswap-compatible QuoterV2
.with_create2(None, init_code_hash) // optional CREATE2 cross-check of the mapping
.with_verify_derivations(true);

let config = FactoryConfig::default().with_concentrated_liquidity(spec);
```

For a tickSpacing-keyed fork (`getPool[t0][t1][tickSpacing]`, no
`feeAmountTickSpacing`), use [`ClFactorySpec::tick_spacing_keyed`] and set a
[`FeeSource`] if the fork exposes one. The presets
([`ClFactorySpec::uniswap_v3`], [`sushi_v3`], [`pancake_v3`], [`slipstream`]) are
just pre-filled specs; start from one and override with the `with_*` builders
when a fork differs only slightly.

For a genuinely novel factory mechanism (not UniV3-mechanics), implement the
[`PoolFactory`] trait directly and add it with
[`PoolDiscovery::with_factory(Box<dyn PoolFactory>)`] — see the boundary below.

## What discovery covers, and what it leaves to you

Discovery ships for the protocols whose pools resolve through a derived storage
slot on the pinned cache. A few AMM shapes are deliberately left to the
integrator in 0.1.0:

- **Algebra-style CL forks** (Camelot, QuickSwap, and similar) — single pool
  per pair, dynamically-computed fees, and a different pool engine
  (`globalState` packing, `tickTable`, a no-fee quoter). Supporting them means
  a different simulation engine, not just a discovery config, so they are out
  of scope for now. Implement one with `register_adapter(Arc<dyn AmmAdapter>)`
  for simulation plus `with_factory(Box<dyn PoolFactory>)` for discovery.
- **Curve** — Curve has no Rust-derivable pool-key slot; resolution needs the
  Vyper MetaRegistry via a view call, which is **not wired in this release**.
  Register Curve pools explicitly (or via a custom `with_factory`); the Curve
  *adapter* still cold-starts and simulates them.
- **Balancer V2** — Balancer has no on-chain token→pool index, so discovery is
  an async log scan rather than a cache read; planned for a later release.
  Balancer pools are still registered explicitly today.

Both escape hatches (`register_adapter`, `with_factory`) are first-class and
stable — a novel AMM never requires forking the crate.

## Creation logs

Beyond pull queries, factories that carry a creation-event `topic0` also decode
their own pool-creation logs via [`PoolDiscovery::decode_creation`] (Uniswap V2
`PairCreated`, the Uniswap/Pancake `PoolCreated`, the Slipstream tickSpacing
`PoolCreated`, and the Solidly `PoolCreated`). This is the push counterpart to
the pull queries above: feed it a factory log and it yields the same
[`DiscoveredPool`].

[`PoolRegistration`]: ../src/adapters/types.rs
[`PoolQuery`]: ../src/adapters/factory.rs
[`find`]: ../src/adapters/factory.rs
[`find_many`]: ../src/adapters/factory.rs
[`pair`]: ../src/adapters/factory.rs
[`basket`]: ../src/adapters/factory.rs
[`pairs`]: ../src/adapters/factory.rs
[`ClFactorySpec`]: ../src/adapters/factory.rs
[`ClFactorySpec::uniswap_v3`]: ../src/adapters/factory.rs
[`ClFactorySpec::tick_spacing_keyed`]: ../src/adapters/factory.rs
[`sushi_v3`]: ../src/adapters/factory.rs
[`pancake_v3`]: ../src/adapters/factory.rs
[`slipstream`]: ../src/adapters/factory.rs
[`ClKeying`]: ../src/adapters/factory.rs
[`FeeSource`]: ../src/adapters/factory.rs
[`SolidlyFactoryConfig`]: ../src/adapters/factory.rs
[`SolidlyFactory`]: ../src/adapters/factory.rs
[`ConcentratedLiquidityFactory`]: ../src/adapters/factory.rs
[`ConcentratedLiquidityAdapter`]: ../src/adapters/uniswap_v3.rs
[`PoolFactory`]: ../src/adapters/factory.rs
[`PoolDiscovery::with_factory(Box<dyn PoolFactory>)`]: ../src/adapters/factory.rs
[`PoolDiscovery::decode_creation`]: ../src/adapters/factory.rs
[`DiscoveredPool`]: ../src/adapters/factory.rs
[`DiscoveryError::MissingFactory(p)`]: ../src/adapters/factory.rs
[`AdapterCache::read_storage_slots`]: ../src/adapters/cache.rs
[`AdapterCache::call_raw`]: ../src/adapters/cache.rs
