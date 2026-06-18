//! Keep pools current from a live log subscription, updating both the in-memory
//! models and the [`EvmCache`].
//!
//! This wires an [`EventRouter`] to `provider.subscribe_logs`: for every swap /
//! mint / burn that arrives, the matching pool is updated in place (no RPC) and
//! the new state is mirrored back into the forked cache. Each pool family is
//! handled — V2/V3/PancakeSwap/Solidly/Curve/Balancer/ERC4626.
//!
//! Run with a WebSocket endpoint (Ctrl-C to stop):
//!
//! ```bash
//! ETH_WS_URL=wss://eth.llamarpc.com cargo run --example event_subscription
//! ```

use std::sync::Arc;

use alloy_primitives::{Address, address};
use alloy_provider::{Provider, ProviderBuilder, WsConnect, network::AnyNetwork};
use alloy_rpc_types_eth::Filter;
use amms::amms::amm::AutomatedMarketMaker;
use evm_amm_state::configured_amms::{AmmConfigEntry, AmmType, load_configured_amms_from_entries};
use evm_amm_state::events::{EventRouter, mirror_updates_to_cache};
use evm_fork_cache::cache::{EvmCache, SlotObservationTracker};
use futures::StreamExt;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let Ok(ws_url) = std::env::var("ETH_WS_URL") else {
        eprintln!("Set ETH_WS_URL to an Ethereum mainnet WebSocket endpoint to run this example.");
        return Ok(());
    };

    let provider = Arc::new(
        ProviderBuilder::new()
            .network::<AnyNetwork>()
            .connect_ws(WsConnect::new(ws_url))
            .await?,
    );

    // Fork the current state and load a handful of busy pools.
    let mut cache = EvmCache::new(provider.clone()).await;

    let weth = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
    let usdc = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
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
    ];

    println!("Loading {} pools...", entries.len());
    let amms = load_configured_amms_from_entries(&mut cache, &entries, Address::ZERO).await?;
    let router = EventRouter::from_loaded(amms);
    println!("Tracking {} pools. Subscribing to logs...", router.len());

    // Filter by the pools' addresses and the event signatures they emit.
    let addresses: Vec<Address> = router.pools().keys().copied().collect();
    let filter = Filter::new()
        .address(addresses)
        .event_signature(router.subscription_topics());

    let mut stream = provider.subscribe_logs(&filter).await?.into_stream();
    let mut observations = SlotObservationTracker::new();

    println!("Listening for events (Ctrl-C to stop)...\n");
    while let Some(log) = stream.next().await {
        match router.apply(&log) {
            Ok(Some(update)) => {
                // Mirror the freshly-applied state into the EVM cache so any
                // EVM-level reads stay consistent with the in-memory pools.
                let summary = mirror_updates_to_cache(
                    &mut cache,
                    &router,
                    std::slice::from_ref(&update),
                    &mut observations,
                );

                // The in-memory pool already reflects the event — show its price.
                if let Some(amm_ref) = router.pools().get(&update.address) {
                    let guard = amm_ref.read().expect("lock");
                    let price = guard.calculate_price(weth, usdc).ok();
                    println!(
                        "{:.12} {:?} {:?} | 1 WETH ~= {} USDC | cache: v2={} v3={} ticks={}",
                        update.address,
                        update.variant,
                        update.kind,
                        price
                            .map(|p| format!("{p:.2}"))
                            .unwrap_or_else(|| "?".into()),
                        summary.v2_injected,
                        summary.v3_injected,
                        summary.ticks_injected,
                    );
                }
            }
            Ok(None) => {}
            Err(e) => eprintln!("apply error: {e}"),
        }
    }

    Ok(())
}
