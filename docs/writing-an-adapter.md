# Writing an adapter

You can add a brand-new AMM to `evm-amm-state` from *outside* the crate — no
fork, no `src/` edit, no new enum variant. This guide shows how, and points at
the runnable example and the built-in adapters you should copy from.

> **Try it:** `cargo run --example custom_adapter` — a self-contained, no-RPC
> demo that defines a novel protocol, registers it, and quotes a swap in both
> directions. The rest of this guide explains what that example does and how a
> production adapter differs.

All the types below live in `evm_amm_state::adapters`.

## The `AmmAdapter` trait

An adapter is one implementation of [`AmmAdapter`](../src/adapters/traits.rs).
It has **one required method** and six defaulted ones:

| Method | Required? | Default |
| --- | --- | --- |
| `protocol(&self) -> ProtocolId` | **yes** | — |
| `protocols(&self) -> Vec<ProtocolId>` | no | `[self.protocol()]` |
| `event_sources(&self, pool) -> Vec<EventSource>` | no | the pool's configured sources |
| `route_log(&self, log, registry) -> Option<PoolKey>` | no | generic address routing |
| `cold_start_planner(&self, pool, policy)` | no | `Err(UnsupportedReason::Protocol(..))` |
| `decode_event(&self, pool, log, view)` | no | `AdapterEventResult::ignored()` |
| `after_apply(&self, pool, event, diff)` | no | `RepairAction::None` |
| `simulate_swap(&self, pool, cache, token_in, token_out, amount_in, config)` | no | `Err(SimError::Unsupported(..))` |

A **minimal adapter is `protocol()` + `simulate_swap()`**. Leaving the rest
defaulted means the adapter can register and quote, but is inert to cold-start
and reactive event sync — fine when you warm/feed state yourself, or for the
purely-local quote in the example. Override `cold_start_planner` / `event_sources`
/ `decode_event` only when you want the crate to warm and reactively sync the
pool for you (see *Cold-start* below).

`protocols()` matters only if one adapter instance serves several protocol ids
from a shared storage layout — the built-in `ConcentratedLiquidityAdapter`
returns `UniswapV3`, `PancakeV3`, and `Slipstream` from a single adapter. A
custom adapter usually serves exactly one id and can ignore it.

## The three `Custom` escape hatches

Because [`ProtocolId`](../src/adapters/types.rs),
[`PoolKey`](../src/adapters/types.rs), and
[`ProtocolMetadata`](../src/adapters/types.rs) each carry a `Custom` variant, a
third party can name and describe an AMM the crate has never heard of without
editing any enum:

- **`ProtocolId::Custom(&'static str)`** — a protocol identity. The `&'static str`
  is the whole mechanism; pick a stable, unique name.
- **`PoolKey::Custom(CustomPoolKey)`** — a pool identity tagged with that same
  string. `CustomPoolKey` is `Address { protocol, address }`,
  `Bytes32 { protocol, id }`, or `Composite { protocol, address, id }`.
  `key.protocol()` derives the matching `ProtocolId::Custom(..)` used for
  dispatch, so keep the string identical to the adapter's.
- **`ProtocolMetadata::Custom(Arc<dyn Any + Send + Sync>)`** — arbitrary per-pool
  config you define. The crate stores it opaquely; your adapter recovers the
  concrete type inside `simulate_swap` with `downcast_ref`:

  ```rust,ignore
  let meta = match &pool.metadata {
      ProtocolMetadata::Custom(any) => any
          .downcast_ref::<ReservesMeta>()
          .ok_or(SimError::MissingMetadata("reserves"))?,
      _ => return Err(SimError::MissingMetadata("reserves")),
  };
  ```

  Attach it at registration with
  `PoolRegistration::new(key).with_metadata(ProtocolMetadata::Custom(Arc::new(meta)))`.

## Registration and dispatch

The [`AdapterRegistry`](../src/adapters/registry.rs) keys adapters by
`ProtocolId` and pools by `PoolKey`:

```rust,ignore
let mut registry = AdapterRegistry::new();
registry.register_adapter(Arc::new(MyAdapter))?;   // keyed by MyAdapter::protocol()
registry.register_pool(registration)?;             // keyed by its PoolKey

// Dispatch: look the adapter up by the pool's protocol id.
let adapter = registry.adapter(pool_key.protocol()).unwrap();
let pool = registry.pool(&pool_key).unwrap();
let quote = adapter.simulate_swap(pool, &mut cache, token_in, token_out, amount_in, &config)?;
```

`register_adapter` registers the `Arc` under every id returned by
`protocols()`; duplicate ids or pool keys return a `RegistryError`.

## Implementing `simulate_swap`: two strategies

`simulate_swap` returns a [`SwapQuote`](../src/adapters/sim.rs) (`{ amount_out }`)
or a [`SimError`](../src/adapters/sim.rs). There are two ways to produce it.

### (a) RECOMMENDED — run the pool's on-chain quote (no reimplemented math)

This is the crate's whole design philosophy: **reimplement no AMM math.** Build
the pool's *own* canonical quote calldata, run it against the warmed cache, and
decode the output. You get `eth_call`-grade correctness — the deployed bytecode
does the math — with nothing to drift from the real contract.

The public helper is
[`quote_via_call(cache, target, calldata) -> Result<Bytes, SimError>`](../src/adapters/sim.rs).
It runs the calldata with `from = ZERO`, `commit = false` (never mutates the
cache), returns the raw success output, and maps a revert/halt to
`SimError::Reverted`. Your adapter only builds calldata and decodes the return:

```rust,ignore
use alloy_sol_types::SolCall;

let calldata = Bytes::from(getAmountOutCall { amountIn: amount_in, tokenIn: token_in }.abi_encode());
let output = quote_via_call(cache, pool_address, calldata)?;
let amount_out = getAmountOutCall::abi_decode_returns_validate(&output)
    .map_err(|_| SimError::MalformedOutput("getAmountOut return"))?;
Ok(SwapQuote::new(amount_out))
```

The **best minimal template** is
[`SolidlyV2Adapter::simulate_swap`](../src/adapters/solidly_v2.rs): it encodes the
pool's `getAmountOut(amountIn, tokenIn)`, calls `quote_via_call` against the pool
address, and decodes one `uint256`. The other built-ins follow the same shape
against different targets — `UniswapV2Adapter` → `Router02.getAmountsOut`,
`ConcentratedLiquidityAdapter` → `QuoterV2.quoteExactInputSingle`,
`BalancerV2Adapter` → the pool's `Vault.queryBatchSwap`, `CurveAdapter` → the
pool's `get_dy`. Quote targets that aren't the pool itself (the V2 router, the V3
quoter) are resolved from [`SimConfig`](../src/adapters/sim.rs); per-pool targets
(the Balancer vault) come from the pool's metadata.

For this to work, the quote contract's bytecode and the slots it `SLOAD`s must be
reachable in the cache — either lazily fetched from a live backend, or warmed by
cold-start (below). A `quote_via_call` against an unwarmed offline cache reverts.

### (b) Local math — when no on-chain quote exists

If the pool has no callable quote entrypoint, or you deliberately want a
self-contained adapter with no state, compute `amount_out` yourself from config
held in metadata. This is what `examples/custom_adapter.rs` does (constant
product `x*y=k` over reserves stored in `ProtocolMetadata::Custom`). It is the
*fallback* — prefer (a) whenever the pool exposes a quote, because local math is
exactly the drift the crate is designed to avoid.

## Cold-start (optional)

Cold-start warms a pool's read-set into the cache so later quotes and reactive
updates run offline. It is **optional**: the default `cold_start_planner` returns
`Err(UnsupportedReason::Protocol(..))`, so an adapter that quotes from metadata
(strategy (b)) or against a live-backed cache needs no planner.

If you do want the crate to warm state for you, implement `cold_start_planner` to
return a boxed [`AdapterColdStartPlanner`](../src/adapters/cold_start.rs). The
planner declares per-round slot work (verify/probe) that
`AdapterRegistry::cold_start` drives, then finalizes the pool's metadata/status.
If the adapter also knows the pool's canonical deployed runtime bytecode,
override `code_seeds` to return [`AdapterCodeSeed`](../src/adapters/bytecode.rs)
entries. `AdapterRegistry::cold_start` writes those bytes into `EvmCache` before
the first round; `evm-fork-cache` verifies pending code seeds against on-chain
code hashes before any cold-start simulation runs.

When the bytecode can be derived from existing metadata, build the seed with a
plain function rather than adding bytecode fields to the metadata. The built-in
adapters are the template: Uniswap V2's `code_seeds` returns
`uniswap_v2_pair_code_seed(address)` (the shared pair runtime plus its
precomputed code hash), and Uniswap V3's returns
`v3_code_seed_from_metadata(address, metadata)` — both in
[`src/adapters/bytecode.rs`](../src/adapters/bytecode.rs), with the runtime
artifacts under [`src/adapters/bytecodes`](../src/adapters/bytecodes). A pool
that is simply not seedable (missing/incomplete metadata, wrong protocol)
returns `Ok(vec![])`; only a genuine template render failure returns
`Err(BytecodeTemplateError)`, which the facade treats as a safe skip. V3
automatic seeding is enabled only when metadata includes the factory/deployer
context, which factory discovery supplies without assuming a chain-global
address.

The built-in planners are the template:
[`SolidlyV2ColdStartPlanner`](../src/adapters/solidly_v2.rs) is the simplest
(a single verify-only round over named slots); the Balancer and Curve adapters
show the discover→verify pattern for layout-free protocols. Pair it with
`event_sources` + `decode_event` (again, Solidly V2 is the minimal reference) if
you also want reactive event sync.

## Try it

```bash
cargo run --example custom_adapter
```

The example ([`examples/custom_adapter.rs`](../examples/custom_adapter.rs)) walks
every step above — `Custom` id/key/metadata, a minimal `AmmAdapter`, registration,
dispatch, and a self-asserting quote — with no RPC or env vars.
