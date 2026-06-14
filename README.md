# evm-amm-state

`evm-amm-state` is an EVM-backed AMM state loading and synchronization crate. It
turns forked EVM storage into locally simulatable AMM models, with cache-aware
refresh paths for searchers, indexers, and protocol simulation tools.

It sits on top of two companion crates: [`evm-fork-cache`] for the forked EVM
state cache, and [`amm-math`] for deterministic pool math.

[`evm-fork-cache`]: https://github.com/KaiCode2/evm-fork-cache
[`amm-math`]: https://github.com/KaiCode2/amm-math

## What It Provides

- Unified `LocalAMM` enum for Uniswap V2, Uniswap V3, PancakeSwap V3,
  Balancer V2/V3, Curve, Solidly V2, Slipstream, Uniswap V4 stubs, and ERC4626.
- Forked-state initialization from `evm_fork_cache::cache::EvmCache`.
- V2 reserve sync and direct storage injection.
- V3 slot0/liquidity refresh, adaptive bitmap scanning, tick snapshots,
  incremental tick resync, and targeted event-driven resync helpers.
- Balancer V2 weighted pool state loading and balance refresh.
- Balancer V3 weighted/stable state loading, including stable amplification
  parameter conversion.
- Curve stableswap and twocrypto state loading.
- Solidly V2 stable/volatile pool loading.
- Slipstream concentrated-liquidity loading and V3-compatible simulation.
- TOML-based AMM configuration loading and lazy deferred V3 tick initialization.
- Factory discovery for caller-provided token pairs.

## Crate Boundaries

This crate intentionally owns only generic AMM state and synchronization logic.
It does not contain:

- Strategy search, execution, or EV scoring logic.
- Protocol-specific harvest timing or keeper modeling.
- Transaction signing, broadcasting, or bundle submission.
- Generated project-local Solidity bindings.

Standard view interfaces are declared locally with `alloy_sol_types::sol!`, so
the crate builds from source without a generated bindings crate.

## Example

```rust,ignore
use std::path::Path;
use std::sync::Arc;

use alloy_primitives::Address;
use alloy_provider::{ProviderBuilder, network::AnyNetwork};
use evm_fork_cache::cache::EvmCache;
use evm_amm_state::configured_amms::{complete_deferred_v3_work, load_configured_amms_lazy};

async fn example() -> anyhow::Result<()> {
    let rpc_url = "https://arb1.arbitrum.io/rpc".parse()?;
    let chain_name = "arbitrum";
    let amms_toml = Path::new("amms.toml");

    // Build an AnyNetwork HTTP provider. AnyNetwork is required for
    // foundry-fork-db compatibility, which the EvmCache backend uses.
    let provider = Arc::new(
        ProviderBuilder::new()
            .network::<AnyNetwork>()
            .connect_http(rpc_url),
    );

    // `EvmCache::new(provider, block)` lazily fetches forked state from RPC.
    // Pass `None` for the block to fork from the latest block.
    let mut cache = EvmCache::new(provider, None).await;

    // Balancer V2 entries that omit `vault_address` fall back to this vault.
    let default_balancer_vault = Address::ZERO;

    // Lazy load: V2/Balancer pools and V3 metadata are initialized now; the
    // expensive V3 tick prefetch is deferred into `deferred_v3`.
    let (mut amms, deferred_v3) =
        load_configured_amms_lazy(&mut cache, chain_name, default_balancer_vault, Some(amms_toml))
            .await?;

    // Finish V3 tick initialization before simulating against the pools.
    complete_deferred_v3_work(&mut cache, deferred_v3, &mut amms).await?;

    Ok(())
}
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
