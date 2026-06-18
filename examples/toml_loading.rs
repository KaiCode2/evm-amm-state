//! Load AMMs from an `amms.toml` file (the optional `toml` feature).
//!
//! TOML is one supported source of AMM definitions, not a requirement — see
//! `examples/programmatic_loading.rs` for the config-free path. This example
//! parses `examples/amms.toml`, loads the `ethereum` chain's pools against a
//! fork, and prices a pair.
//!
//! Run with:
//!
//! ```bash
//! ETH_RPC_URL=https://eth.llamarpc.com cargo run --example toml_loading
//! ```

use std::path::Path;
use std::sync::Arc;

use alloy_primitives::{Address, address};
use alloy_provider::{ProviderBuilder, network::AnyNetwork};
use amms::amms::amm::AutomatedMarketMaker;
use evm_amm_state::configured_amms::{load_amm_config_entries, load_configured_amms_from_entries};
use evm_fork_cache::cache::EvmCache;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    // Parse the config first so we can inspect/filter before any RPC work.
    let toml_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/amms.toml");
    let entries = load_amm_config_entries("ethereum", Some(&toml_path))?;
    println!(
        "Parsed {} entries from {}",
        entries.len(),
        toml_path.display()
    );

    let Ok(rpc_url) = std::env::var("ETH_RPC_URL") else {
        eprintln!("\nSet ETH_RPC_URL to an Ethereum mainnet HTTP endpoint to load and price them.");
        return Ok(());
    };

    let provider = Arc::new(
        ProviderBuilder::new()
            .network::<AnyNetwork>()
            .connect_http(rpc_url.parse()?),
    );
    let mut cache = EvmCache::new(provider).await;

    let amms = load_configured_amms_from_entries(&mut cache, &entries, Address::ZERO).await?;
    let loaded = amms.values().filter(|m| m.is_some()).count();
    println!("Loaded {}/{} pools from the fork.\n", loaded, entries.len());

    let weth = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
    let usdc = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
    for amm in amms.values().flatten() {
        if amm.tokens().contains(&weth)
            && amm.tokens().contains(&usdc)
            && let Ok(price) = amm.calculate_price(weth, usdc)
        {
            println!(
                "pool {:.12} ({:?}): 1 WETH ~= {:.2} USDC",
                amm.address(),
                amm.variant(),
                price
            );
        }
    }

    Ok(())
}
