# evm-amm-state

`evm-amm-state` is a real-time AMM state and routing engine, built to show how
two companion crates compose into something larger:

- [`evm-fork-cache`] — a forked-EVM state cache, and
- [`amm-math`] — deterministic, RPC-free pool math.

Together they let you track a set of AMMs, keep them current from chain events,
and run fast, parallel, fully-offline swap simulations against them — for
example, searching for 3-leg arbitrage the moment a pool updates.

[`evm-fork-cache`]: https://github.com/KaiCode2/evm-fork-cache
[`amm-math`]: https://github.com/KaiCode2/amm-math

## The pipeline

| Stage | Module | What it does |
| --- | --- | --- |
| Model | [`amm_wrapper`] | `LocalAMM`, one enum over every pool type implementing the `amms` `AutomatedMarketMaker` trait. |
| Load | [`configured_amms`] | Initialize a working set of pools from an `EvmCache`, from code or (optionally) `amms.toml`. |
| Sync | [`cache_sync`] | Initialize and incrementally refresh each pool family from forked storage, incl. adaptive V3 tick scanning. |
| React | [`events`] | Apply swap / liquidity logs to pools in place (no RPC), then optionally mirror the new state back into the cache. |
| Route | [`routing`] | Enumerate and evaluate multi-leg routes (e.g. triangular arbitrage) over an immutable snapshot, in parallel and offline. |
| Discover | [`discovery`] | Find pools for caller-supplied token pairs from configured factories. |

[`amm_wrapper`]: src/amm_wrapper.rs
[`configured_amms`]: src/configured_amms.rs
[`cache_sync`]: src/cache_sync/mod.rs
[`events`]: src/events/mod.rs
[`routing`]: src/routing/mod.rs
[`discovery`]: src/discovery.rs

## What it provides

- A unified `LocalAMM` enum for Uniswap V2, Uniswap V3, PancakeSwap V3,
  Balancer V2/V3, Curve (stable & crypto), Solidly V2, Slipstream, ERC4626, and
  a Uniswap V4 stub — all simulated through one trait.
- Forked-state initialization and incremental refresh from
  `evm_fork_cache::cache::EvmCache`, including V3 slot0/liquidity/tick sync with
  adaptive bitmap scanning and snapshotting.
- Event-driven updates: decode `Sync` / `Swap` / `Mint` / `Burn` /
  `TokenExchange` / vault `Swap` / `Deposit` / `Withdraw` logs and apply them to
  the matching pool in memory, with cache mirroring for EVM-level consistency.
- Offline, parallel multi-leg routing with optimal-size search, built on the
  pure pool math so it runs deterministically with no RPC.
- Programmatic *or* TOML-driven AMM configuration (TOML is an optional,
  default-on feature).
- Factory discovery for caller-provided token pairs.

## Features

| Feature | Default | Effect |
| --- | --- | --- |
| `toml` | on | Parse AMM definitions from an `amms.toml` file. Without it, build `AmmConfigEntry` values in code and use the `*_from_entries` loaders; the `toml` crate is not pulled in. |

## Crate boundaries

This crate owns generic AMM state, synchronization, and routing primitives. It
deliberately does **not** contain transaction signing, broadcasting, or bundle
submission, nor any application-specific strategy scheduling. Standard view
interfaces are declared locally with `alloy_sol_types::sol!`, so the crate
builds from source without a generated bindings crate.

## Quickstart

Define a working set of pools in code — no config file required — and simulate a
swap entirely offline once they're loaded:

```rust,ignore
use std::sync::Arc;

use alloy_primitives::{Address, U256, address};
use alloy_provider::{ProviderBuilder, network::AnyNetwork};
use amms::amms::amm::AutomatedMarketMaker;
use evm_fork_cache::cache::EvmCache;
use evm_amm_state::configured_amms::{AmmConfigEntry, AmmType, load_configured_amms_from_entries};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // AnyNetwork is required by the EvmCache backend (foundry-fork-db).
    let provider = Arc::new(
        ProviderBuilder::new()
            .network::<AnyNetwork>()
            .connect_http("https://eth.llamarpc.com".parse()?),
    );
    let mut cache = EvmCache::new(provider, None).await; // None = latest block

    let weth = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
    let usdc = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");

    let entries = vec![
        AmmConfigEntry::new(AmmType::UniswapV3, address!("88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640"))
            .with_tokens(vec![usdc, weth])
            .with_fee_tier(500),
    ];

    let amms = load_configured_amms_from_entries(&mut cache, &entries, Address::ZERO).await?;

    for amm in amms.values().flatten() {
        let out = amm.simulate_swap(weth, usdc, U256::from(10).pow(U256::from(18)))?;
        println!("1 WETH -> {out} USDC (raw)");
    }
    Ok(())
}
```

The same pools can instead be loaded from `amms.toml` (with the `toml` feature)
via `load_configured_amms` / `load_amm_config_entries`.

### React to events, then search for arbitrage

```rust,ignore
use evm_amm_state::events::EventRouter;
use evm_amm_state::routing::find_triangular_arbitrage;

let router = EventRouter::from_loaded(amms);

// Subscribe to the topics the tracked pools emit.
let filter = alloy_rpc_types_eth::Filter::new()
    .event_signature(router.subscription_topics());
let mut stream = provider.subscribe_logs(&filter).await?.into_stream();

while let Some(log) = stream.next().await {
    if router.apply(&log)?.is_some() {
        // Pools now reflect the event. Search an immutable snapshot in parallel.
        let snapshot = router.snapshot();
        if let Some(arb) = find_triangular_arbitrage(&snapshot, weth, min_in, max_in) {
            println!("arb: in {} -> out {} (profit {})", arb.amount_in, arb.amount_out, arb.profit);
        }
    }
}
```

## Examples

| Example | Needs a node? | Shows |
| --- | --- | --- |
| `triangular_arbitrage` | no | The full event → snapshot → parallel offline 3-leg search loop on synthetic pools. The best starting point. |
| `programmatic_loading` | HTTP | Building `AmmConfigEntry` in code and loading from a fork. |
| `toml_loading` | HTTP | Loading the same pools from `examples/amms.toml`. |
| `event_subscription` | WebSocket | A live subscription updating pools and the cache per event. |

```bash
# Fully offline — always runnable:
cargo run --example triangular_arbitrage

# Against a node:
ETH_RPC_URL=https://eth.llamarpc.com cargo run --example programmatic_loading
ETH_RPC_URL=https://eth.llamarpc.com cargo run --example toml_loading
ETH_WS_URL=wss://your-node cargo run --example event_subscription
```

## Benchmarks

Offline benchmarks for the latency-sensitive paths (single-pool simulation,
event apply, and the parallel triangular search):

```bash
cargo bench
```

## Testing

```bash
cargo test
```

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this crate by you, as defined in the Apache-2.0 license, shall
be dual licensed as above, without any additional terms or conditions.
