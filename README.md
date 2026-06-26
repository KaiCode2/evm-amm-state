# evm-amm-state

`evm-amm-state` is a real-time AMM state engine built on a forked-EVM state
cache ([`evm-fork-cache`]). It tracks a working set of pools, **cold-starts**
their on-chain state into the cache, keeps them current **purely from chain log
events** (no RPC in the hot path), and runs fast, **fully-offline swap
simulations** against the live-synced state.

The defining design choice: **no reimplemented AMM math.** Every quote runs the
pool's *own* canonical on-chain quote entrypoint inside a local revm against the
warmed cache (e.g. Uniswap `QuoterV2`, Curve `get_dy`), then decodes the result.
There is no `LocalAMM`/`amm-math` formula layer to drift from the real contracts.

[`evm-fork-cache`]: https://github.com/KaiCode2/evm-fork-cache

## The pipeline

Each protocol is a single [`AmmAdapter`] implementation; the
[`AdapterRegistry`] dispatches by pool key.

| Stage | What it does |
| --- | --- |
| **Register** | Describe a pool: a [`PoolKey`] + [`ProtocolMetadata`] (tokens, fee, storage layout / coins, …). |
| **Cold-start** | `registry.cold_start(pool, cache, policy)` warms the pool's read-set into the [`EvmCache`] from forked storage. Named-slot protocols (Uniswap V2/V3, Solidly) warm known slots; layout-free protocols (Balancer, Curve) **discover → verify** the exact slots a quote call SLOADs. |
| **Subscribe** | `adapter.event_sources(pool)` lists the log topics to subscribe to over a `wss://` endpoint. |
| **React** | Decoded logs flow through [`AmmReactiveHandler`] + the `evm_fork_cache` reactive runtime, updating cached state with **no RPC**. Some protocols event-source exact writes (Uniswap V2/Solidly `Sync` carry absolute reserves); others re-verify the affected slots (Balancer/Curve events carry deltas, so the runtime refetches just those slots). |
| **Simulate** | `adapter.simulate_swap(pool, cache, token_in, token_out, amount_in, &config)` executes the pool's own quote against the cached state and returns a [`SwapQuote`] — fully offline. |

[`AmmAdapter`]: src/adapters/traits.rs
[`AdapterRegistry`]: src/adapters/registry.rs
[`AmmReactiveHandler`]: src/adapters/reactive.rs
[`PoolKey`]: src/adapters/types.rs
[`ProtocolMetadata`]: src/adapters/types.rs
[`EvmCache`]: https://github.com/KaiCode2/evm-fork-cache
[`SwapQuote`]: src/adapters/sim.rs

## Supported protocols

| Protocol | Feature | Quote entrypoint | Cold-start | Reactive |
| --- | --- | --- | --- | --- |
| Uniswap V2 | `uniswap-v2` | `Router02.getAmountsOut` | named slots | `Sync` → exact masked write |
| Uniswap V3 family (V3, PancakeSwap V3, Slipstream) | `uniswap-v3` (`pancake-v3`, `slipstream`) | `QuoterV2.quoteExactInputSingle` | slot0 + liquidity + adaptive multi-word tick scan | `Swap` → slot0/liquidity; `Mint`/`Burn` → tick-range resync |
| Balancer V2 | `balancer-v2` | `Vault.queryBatchSwap` | discover → verify (`getPoolTokens`) | `Swap` → balance-slot resync |
| Solidly V2 (Aerodrome / Velodrome) | `solidly-v2` | pool `getAmountOut` | named slots (config layout) | `Sync` → two exact slot writes |
| **Curve** (StableSwap, StableSwap-NG, CryptoSwap v2, Tricrypto-NG) | `curve` | pool `get_dy` | discover → verify (`get_dy` read-set) | `TokenExchange` + liquidity events → slot resync |

All protocol features are on by default. See [`docs/curve-adapter.md`](docs/curve-adapter.md)
for the Curve adapter in depth.

## Quickstart

Register a pool, cold-start it into a forked cache, and simulate a swap entirely
offline once warmed:

```rust,ignore
use std::sync::Arc;

use alloy_eips::{BlockId, BlockNumberOrTag};
use alloy_network::AnyNetwork;
use alloy_primitives::{U256, address};
use alloy_provider::{Provider, RootProvider};
use evm_fork_cache::cache::EvmCache;
use evm_amm_state::adapters::{
    AdapterRegistry, AmmAdapter, ColdStartPolicy, PoolKey, PoolRegistration,
    ProtocolMetadata, SimConfig, UniswapV3Adapter, V3Metadata,
};
use evm_amm_state::adapters::storage::V3StorageLayout;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let provider = Arc::new(RootProvider::<AnyNetwork>::connect("https://eth.llamarpc.com").await?);
    let block = provider.get_block_number().await?;
    let mut cache = EvmCache::at_block(provider, BlockId::Number(BlockNumberOrTag::Number(block))).await;

    let usdc = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
    let weth = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
    let pool = address!("88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640"); // USDC/WETH 0.05%

    let mut registry = AdapterRegistry::new();
    registry.register_adapter(Arc::new(UniswapV3Adapter::default()))?;

    let mut reg = PoolRegistration::new(PoolKey::UniswapV3(pool))
        .with_state_address(pool)
        .with_metadata(ProtocolMetadata::UniswapV3(V3Metadata {
            token0: Some(usdc),
            token1: Some(weth),
            fee: Some(500),
            tick_spacing: Some(10),
            storage_layout: Some(V3StorageLayout::uniswap(10)),
        }));
    registry.cold_start(&mut reg, &mut cache, ColdStartPolicy::Eager)?;

    // Offline from here: no RPC needed to quote.
    let out = UniswapV3Adapter::default().simulate_swap(
        &reg, &mut cache, usdc, weth, U256::from(1_000_000_u64), &SimConfig::default(),
    )?;
    println!("1 USDC -> {} WETH (raw)", out.amount_out);
    Ok(())
}
```

The full **cold-start → WebSocket subscribe → react → simulate** loop is in
[`examples/adapter_pipeline.rs`](examples/adapter_pipeline.rs):

```bash
ETH_WS_URL=wss://your-node cargo run --example adapter_pipeline
# or derive wss:// from an https endpoint:
E2E_RPC_URL=https://your-archive-node cargo run --example adapter_pipeline
```

## Crate boundaries

This crate owns generic AMM state loading, event-driven synchronization, and
offline swap simulation. It deliberately does **not** contain transaction
signing/broadcasting, strategy scheduling, or multi-leg arbitrage routing (the
legacy routing layer was removed; it is rebuildable on top of `simulate_swap`).
Standard view interfaces are declared locally with `alloy_sol_types::sol!`, so
the crate builds from source with no generated bindings crate.

## Testing

```bash
cargo test                       # unit + offline integration tests
cargo test --no-default-features # protocol-neutral core
```

Network-dependent tests are env-gated and `#[ignore]`d. With an archive RPC they
pin a block, cold-start a real pool, and assert `simulate_swap` **equals the
on-chain quote** at the same block (`eth_call`), plus a live WebSocket soak that
keeps state in sync from events only:

```bash
# RPC parity (mainnet pools; Base for Solidly via host-swap):
E2E_RPC_URL=<archive-url> cargo test --test adapter_swap_sim_rpc -- --ignored
# Live WS soak (Uniswap V2, and Curve across all dialects):
E2E_RPC_URL=<archive-url> cargo test --test reactive_ws_e2e --test reactive_curve_ws_e2e -- --ignored --nocapture
```

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this crate by you, as defined in the Apache-2.0 license, shall
be dual licensed as above, without any additional terms or conditions.
