//! Load AMMs programmatically (no `amms.toml`) and simulate a swap offline.
//!
//! Demonstrates that the crate does not require a config file: AMMs are defined
//! with [`AmmConfigEntry`] builders, loaded against an [`EvmCache`] forked from
//! a node, and then simulated entirely in-memory.
//!
//! Run with an Ethereum mainnet HTTP endpoint:
//!
//! ```bash
//! ETH_RPC_URL=https://eth.llamarpc.com cargo run --example programmatic_loading
//! ```

use std::sync::Arc;

use alloy_primitives::{Address, U256, address};
use alloy_provider::{ProviderBuilder, network::AnyNetwork};
use amms::amms::amm::AutomatedMarketMaker;
use evm_amm_state::configured_amms::{AmmConfigEntry, AmmType, load_configured_amms_from_entries};
use evm_fork_cache::cache::EvmCache;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let Ok(rpc_url) = std::env::var("ETH_RPC_URL") else {
        eprintln!("Set ETH_RPC_URL to an Ethereum mainnet HTTP endpoint to run this example.");
        return Ok(());
    };

    // AnyNetwork is required by the EvmCache backend (foundry-fork-db).
    let provider = Arc::new(
        ProviderBuilder::new()
            .network::<AnyNetwork>()
            .connect_http(rpc_url.parse()?),
    );

    // Fork from the latest block.
    let mut cache = EvmCache::new(provider).await;

    // Mainnet tokens.
    let weth = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
    let usdc = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
    let dai = address!("6B175474E89094C44Da98b954EedeAC495271d0F");

    // Define the working set in code — this is the alternative to amms.toml.
    let entries = vec![
        AmmConfigEntry::new(
            AmmType::UniswapV3,
            address!("88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640"), // USDC/WETH 0.05%
        )
        .with_tokens(vec![usdc, weth])
        .with_fee_tier(500),
        AmmConfigEntry::new(
            AmmType::UniswapV2,
            address!("B4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc"), // USDC/WETH
        )
        .with_tokens(vec![usdc, weth]),
        AmmConfigEntry::new(
            AmmType::UniswapV3,
            address!("60594a405d53811d3BC4766596EFD80fd545A270"), // DAI/WETH 0.05%
        )
        .with_tokens(vec![dai, weth])
        .with_fee_tier(500),
    ];

    println!("Loading {} pools from the fork...", entries.len());
    let amms = load_configured_amms_from_entries(&mut cache, &entries, Address::ZERO).await?;

    let loaded: Vec<_> = amms
        .iter()
        .filter_map(|(a, m)| m.as_ref().map(|m| (a, m)))
        .collect();
    println!("Loaded {}/{} pools.\n", loaded.len(), entries.len());

    // Simulate 1 WETH -> USDC on each pool that supports the pair, fully offline.
    let one_weth = U256::from(10u64).pow(U256::from(18u64));
    for (addr, amm) in &loaded {
        if amm.tokens().contains(&weth) && amm.tokens().contains(&usdc) {
            match amm.simulate_swap(weth, usdc, one_weth) {
                Ok(out) => {
                    // USDC has 6 decimals.
                    let usdc_out = out / U256::from(1_000_000u64);
                    println!(
                        "pool {:.12} ({:?}): 1 WETH -> {} USDC",
                        addr,
                        amm.variant(),
                        usdc_out
                    );
                }
                Err(e) => println!("pool {:.12}: swap failed: {e}", addr),
            }
        }
    }

    Ok(())
}
