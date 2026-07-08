//! V3 full-sync RPC parity tests (env-gated, `#[ignore]`) — MANAGER RUNS THIS.
//!
//! Given `E2E_RPC_URL` (an archive node), these fork mainnet at a PINNED
//! block and prove the one-shot sync program against on-chain ground truth:
//!
//! 1. **State parity** — the program's statics / ticks / bitmap / observation
//!    output equals `eth_getStorageAt` at the same block, slot for slot.
//! 2. **Quote parity** — a tick-crossing `simulate_swap` over a cache warmed
//!    *only* by the injected snapshot equals both the provider's own QuoterV2
//!    `eth_call` and a quote over a classically cold-started cache.
//! 3. **Partial window parity** — the calldata-driven partial program returns
//!    exactly the full-sync ticks of the requested words.
//!
//! ```text
//! E2E_RPC_URL=<archive-url> cargo test --test v3_full_sync_rpc -- --ignored
//! ```

use std::sync::Arc;

use alloy_eips::{BlockId, BlockNumberOrTag};
use alloy_network::TransactionBuilder;
use alloy_primitives::aliases::U24;
use alloy_primitives::{Address, Bytes, U256, address};
use alloy_provider::network::AnyNetwork;
use alloy_provider::{Provider, RootProvider};
use alloy_rpc_types_eth::TransactionRequest;
use alloy_sol_types::SolCall;
use anyhow::{Context, Result, anyhow};

use evm_amm_state::adapters::storage::{
    V3StorageLayout, v3_tick_bitmap_storage_key_with_base, v3_tick_info_storage_keys_with_base,
    v3_word_position,
};
use evm_amm_state::adapters::v3_sync::{V3SyncSpec, run_full_sync, run_partial_sync};
use evm_amm_state::adapters::{
    AdapterRegistry, AmmAdapter, ColdStartPolicy, ConcentratedLiquidityAdapter, PoolKey,
    PoolRegistration, ProtocolMetadata, SimConfig, V3Metadata,
};
use evm_fork_cache::cache::EvmCache;

// Local QuoterV2 ABI: the crate's own quote-call bindings are crate-internal.
alloy_sol_types::sol! {
    struct QuoteExactInputSingleParams {
        address tokenIn;
        address tokenOut;
        uint256 amountIn;
        uint24 fee;
        uint160 sqrtPriceLimitX96;
    }

    function quoteExactInputSingle(QuoteExactInputSingleParams params)
        returns (
            uint256 amountOut,
            uint160 sqrtPriceX96After,
            uint32 initializedTicksCrossed,
            uint256 gasEstimate
        );
}

const FORK_BLOCK: u64 = 20_000_000;

const USDC: Address = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
const WETH: Address = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
const V3_QUOTER_V2: Address = address!("61fFE014bA17989E743c5F6cB21bF9697530B21e");
const POOL: Address = address!("88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640");

fn rpc_url() -> Option<String> {
    std::env::var("E2E_RPC_URL").ok()
}

fn block() -> BlockId {
    BlockId::Number(BlockNumberOrTag::Number(FORK_BLOCK))
}

fn spec() -> V3SyncSpec {
    V3SyncSpec::uniswap(V3StorageLayout::uniswap(10))
}

async fn provider(url: &str) -> Result<RootProvider<AnyNetwork>> {
    RootProvider::<AnyNetwork>::connect(url)
        .await
        .context("connect RPC url")
}

async fn storage_at(provider: &RootProvider<AnyNetwork>, slot: U256) -> Result<U256> {
    Ok(provider
        .get_storage_at(POOL, slot)
        .block_id(block())
        .await?)
}

fn registration() -> PoolRegistration {
    PoolRegistration::new(PoolKey::UniswapV3(POOL))
        .with_state_address(POOL)
        .with_metadata(ProtocolMetadata::UniswapV3(
            V3Metadata::default()
                .with_token0(USDC)
                .with_token1(WETH)
                .with_fee(500)
                .with_tick_spacing(10)
                .with_storage_layout(V3StorageLayout::uniswap(10)),
        ))
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires E2E_RPC_URL archive node; run with --ignored"]
async fn full_sync_matches_onchain_ground_truth() -> Result<()> {
    let Some(url) = rpc_url() else {
        eprintln!("E2E_RPC_URL unset; skipping");
        return Ok(());
    };
    let provider = provider(&url).await?;
    let spec = spec();

    let snapshot = run_full_sync(&provider, block(), POOL, &spec)
        .await
        .map_err(|e| anyhow!("full sync failed: {e}"))?;

    // Shape sanity: a deep mainnet pool, ascending ticks, real liquidity.
    assert!(
        snapshot.ticks.len() > 200,
        "expected a deep pool, got {} ticks",
        snapshot.ticks.len()
    );
    assert!(
        snapshot
            .ticks
            .windows(2)
            .all(|pair| pair[0].tick < pair[1].tick),
        "ticks must be strictly ascending"
    );
    let liquidity_mask = (U256::from(1u64) << 128usize) - U256::from(1u64);
    assert!(
        snapshot
            .ticks
            .iter()
            .all(|t| !(t.info[0] & liquidity_mask).is_zero()),
        "every initialized tick must carry nonzero liquidityGross"
    );

    // Statics vs eth_getStorageAt.
    for (slot, value) in &snapshot.statics {
        assert_eq!(
            storage_at(&provider, *slot).await?,
            *value,
            "static slot {slot:#x}"
        );
    }
    // A spread of tick records (first, middle, last), all four words each.
    let picks = [
        &snapshot.ticks[0],
        &snapshot.ticks[snapshot.ticks.len() / 2],
        &snapshot.ticks[snapshot.ticks.len() - 1],
    ];
    for tick in picks {
        let keys = v3_tick_info_storage_keys_with_base(tick.tick, spec.layout.ticks_base_slot);
        for (key, expected) in keys.into_iter().zip(tick.info) {
            assert_eq!(
                storage_at(&provider, key).await?,
                expected,
                "tick {} slot {key:#x}",
                tick.tick
            );
        }
    }
    // The reconstructed bitmap word holding the first tick, and one empty word.
    let entries = snapshot.storage_entries(&spec);
    let first_word = v3_word_position(snapshot.ticks[0].tick, spec.layout.tick_spacing);
    for word in [first_word, 300i16] {
        let key = v3_tick_bitmap_storage_key_with_base(word, spec.layout.tick_bitmap_base_slot);
        let reconstructed = entries
            .iter()
            .find(|(slot, _)| *slot == key)
            .map(|(_, value)| *value)
            .expect("bitmap word materialized");
        assert_eq!(
            storage_at(&provider, key).await?,
            reconstructed,
            "bitmap word {word}"
        );
    }
    // Observation ring: first and last live entries.
    assert!(!snapshot.observations.is_empty());
    let ring = spec.observations.as_ref().unwrap().array_slot;
    let last = snapshot.observations.len() as u64 - 1;
    assert_eq!(storage_at(&provider, ring).await?, snapshot.observations[0]);
    assert_eq!(
        storage_at(&provider, ring + U256::from(last)).await?,
        snapshot.observations[last as usize]
    );

    println!(
        "full sync verified: {} ticks, {} observations, {} materialized slots — one eth_call",
        snapshot.ticks.len(),
        snapshot.observations.len(),
        entries.len(),
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires E2E_RPC_URL archive node; run with --ignored"]
async fn full_sync_quote_parity_with_cold_start_and_eth_call() -> Result<()> {
    let Some(url) = rpc_url() else {
        eprintln!("E2E_RPC_URL unset; skipping");
        return Ok(());
    };
    let provider = provider(&url).await?;
    let spec = spec();

    // A hard tick-crossing swap: 2,000,000 USDC in (6 decimals).
    let amount_in = U256::from(2_000_000_000_000u64);
    let adapter = ConcentratedLiquidityAdapter::default();
    let config = SimConfig::default();
    let registration = registration();

    // Cache A: warmed ONLY by the one-shot program snapshot.
    let snapshot = run_full_sync(&provider, block(), POOL, &spec)
        .await
        .map_err(|e| anyhow!("full sync failed: {e}"))?;
    let mut synced_cache = EvmCache::at_block(Arc::new(provider.clone()), block()).await;
    snapshot.inject(&mut synced_cache, POOL, &spec);
    let synced_quote = adapter
        .simulate_swap(
            &registration,
            &mut synced_cache,
            USDC,
            WETH,
            amount_in,
            &config,
        )
        .map_err(|e| anyhow!("synced-cache sim failed: {e}"))?;

    // Cache B: the classic windowed cold start.
    let mut lazy_cache = EvmCache::at_block(Arc::new(provider.clone()), block()).await;
    let registry = {
        let mut r = AdapterRegistry::new();
        r.register_adapter(Arc::new(ConcentratedLiquidityAdapter::default()))?;
        r
    };
    let mut lazy_registration = registration.clone();
    registry.cold_start(
        &mut lazy_registration,
        &mut lazy_cache,
        ColdStartPolicy::Eager,
    )?;
    let lazy_quote = adapter
        .simulate_swap(
            &lazy_registration,
            &mut lazy_cache,
            USDC,
            WETH,
            amount_in,
            &config,
        )
        .map_err(|e| anyhow!("lazy-cache sim failed: {e}"))?;

    // Ground truth: the same QuoterV2 call via the provider at the pin.
    let calldata = Bytes::from(
        quoteExactInputSingleCall {
            params: QuoteExactInputSingleParams {
                tokenIn: USDC,
                tokenOut: WETH,
                amountIn: amount_in,
                fee: U24::from(500u32),
                sqrtPriceLimitX96: U256::ZERO.to(),
            },
        }
        .abi_encode(),
    );
    let tx = TransactionRequest::default()
        .with_to(V3_QUOTER_V2)
        .with_input(calldata);
    let out = provider.call(tx.into()).block(block()).await?;
    let truth = quoteExactInputSingleCall::abi_decode_returns_validate(&out)?;

    assert_eq!(
        synced_quote.amount_out, truth.amountOut,
        "program-synced cache must match on-chain QuoterV2"
    );
    assert_eq!(
        synced_quote.amount_out, lazy_quote.amount_out,
        "program-synced cache must match the classic cold-start cache"
    );
    println!(
        "quote parity at {} USDC in: {} wei WETH out (program-synced == cold-start == eth_call)",
        amount_in, synced_quote.amount_out,
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires E2E_RPC_URL archive node; run with --ignored"]
async fn partial_sync_window_matches_full_sync() -> Result<()> {
    let Some(url) = rpc_url() else {
        eprintln!("E2E_RPC_URL unset; skipping");
        return Ok(());
    };
    let provider = provider(&url).await?;
    let spec = spec();

    let snapshot = run_full_sync(&provider, block(), POOL, &spec)
        .await
        .map_err(|e| anyhow!("full sync failed: {e}"))?;

    // The planner's shape: a ±2-word window around the current tick.
    let slot0 = snapshot.statics[0].1;
    let tick_word = {
        let raw = ((slot0 >> 160usize) & U256::from(0xFF_FFFFu64)).to::<u64>() as u32;
        let tick = if raw & 0x80_0000 != 0 {
            (raw | 0xFF00_0000) as i32
        } else {
            raw as i32
        };
        v3_word_position(tick, spec.layout.tick_spacing)
    };
    let words: Vec<i16> = (tick_word - 2..=tick_word + 2).collect();

    let window_ticks = run_partial_sync(&provider, block(), POOL, &spec, &words)
        .await
        .map_err(|e| anyhow!("partial sync failed: {e}"))?;

    let expected: Vec<_> = snapshot
        .ticks
        .iter()
        .filter(|t| words.contains(&v3_word_position(t.tick, spec.layout.tick_spacing)))
        .cloned()
        .collect();
    assert_eq!(window_ticks, expected);
    assert!(
        !window_ticks.is_empty(),
        "the active-tick window of a deep pool should hold initialized ticks"
    );
    println!(
        "partial sync verified: {} ticks across the ±2-word active window",
        window_ticks.len()
    );
    Ok(())
}
