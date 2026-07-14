//! Live WebSocket soak for the Curve adapter (env-gated, `#[ignore]`) — MANAGER RUNS.
//!
//! Verifies the slice-3 liquidity-event routing against the LIVE chain, end to
//! end over a `wss://` subscription:
//!   1. **Liquidity signatures are real + flowing**: subscribe TOPIC-ONLY to the
//!      union of every Curve event signature this adapter derives (swap +
//!      liquidity, StableSwap + CryptoSwap, arities 2/3) and confirm liquidity
//!      events when the network produces them. Set
//!      `E2E_REQUIRE_CURVE_ACTIVITY=1` for a strict activity soak; the default
//!      release check does not confuse an inactive market window with a broken
//!      subscription or adapter.
//!   2. **Live routing**: register 3pool (StableSwap) + tricrypto2 (CryptoSwap),
//!      route each delivered log through the real reactive runtime, and confirm
//!      registered-pool events apply (resync) live. Liquidity events on a given
//!      registered pool are rare per-window, so any observed are logged + counted
//!      as a bonus; deterministic resync correctness is covered by the offline
//!      reactive tests (`curve_*_liquidity_events_resync`).
//!   3. **Live accuracy**: at the post-soak head `M`, a fresh pinned cache +
//!      cold-start + `simulate_swap` == `eth_call get_dy` at `M` for BOTH variants.
//!
//! Run (`ETH_WS_URL` takes precedence; otherwise derives `wss://` from
//! `E2E_RPC_URL`; default 180s, override `E2E_WS_SECONDS`):
//! ```text
//! E2E_RPC_URL=<https-archive> cargo test --test reactive_curve_ws_e2e -- --ignored --nocapture
//! ```

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use alloy_eips::{BlockId, BlockNumberOrTag};
use alloy_network::{AnyNetwork, Ethereum, TransactionBuilder};
use alloy_primitives::{Address, B256, Bytes, U256, address, keccak256};
use alloy_provider::{Provider, RootProvider};
use alloy_rpc_types_eth::{Filter, Log as RpcLog, TransactionRequest};
use alloy_sol_types::SolCall;
use anyhow::{Context, Result, anyhow};
use evm_amm_state::adapters::{
    AdapterRegistry, AmmAdapter, AmmReactiveHandler, ColdStartPolicy, CurveAdapter, CurveMetadata,
    CurveVariant, PoolKey, PoolRegistration, ProtocolMetadata, SimConfig,
};
use evm_fork_cache::cache::EvmCache;
use evm_fork_cache::reactive::{
    BlockRef, ChainStatus, InputSource, ReactiveConfig, ReactiveContext, ReactiveInput,
    ReactiveInputBatch, ReactiveInputRecord, ReactiveRuntime,
};
use futures::StreamExt;

// Local `get_dy` quote ABI (the crate's own bindings are crate-internal):
// builds the ground-truth `eth_call`s the soak's parity checks compare against.
alloy_sol_types::sol! {
    function get_dy(int128 i, int128 j, uint256 dx) returns (uint256 dy);

    interface CurveCryptoSwap {
        function get_dy(uint256 i, uint256 j, uint256 dx) returns (uint256 dy);
    }
}

// 3pool (StableSwap, DAI/USDC/USDT).
const THREEPOOL: Address = address!("bEbc44782C7dB0a1A60Cb6fe97d0b483032FF1C7");
const DAI: Address = address!("6B175474E89094C44Da98b954EedeAC495271d0F");
const USDC: Address = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
const USDT: Address = address!("dAC17F958D2ee523a2206206994597C13D831ec7");
// tricrypto2 (CryptoSwap, USDT/WBTC/WETH).
const TRICRYPTO2: Address = address!("D51a44d3FaE010294C616388b506AcdA1bfAAE46");
const WBTC: Address = address!("2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599");
const WETH: Address = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
// tricryptoUSDC (Tricrypto-NG, USDC/WBTC/WETH).
const TRICRYPTO_USDC_NG: Address = address!("7F86Bf177Dd4F3494b841a37e810A34dD56c829B");

/// Every Curve event signature this adapter derives (swap + liquidity, both
/// variants, arities 2 and 3), labelled. Subscribed TOPIC-ONLY so we observe
/// them ecosystem-wide; the "liquidity" flag drives assertion (1).
fn curve_topics() -> Vec<(B256, &'static str, bool)> {
    let mut v: Vec<(B256, &'static str, bool)> = vec![
        (
            keccak256("TokenExchange(address,int128,uint256,int128,uint256)"),
            "swap/stable",
            false,
        ),
        (
            keccak256("TokenExchange(address,uint256,uint256,uint256,uint256)"),
            "swap/crypto",
            false,
        ),
        // Tricrypto-NG: 7-arg TokenExchange + 6-arg RemoveLiquidityOne.
        (
            keccak256("TokenExchange(address,uint256,uint256,uint256,uint256,uint256,uint256)"),
            "swap/cryptong",
            false,
        ),
        (
            keccak256("RemoveLiquidityOne(address,uint256,uint256)"),
            "removeone/2arg",
            true,
        ),
        (
            keccak256("RemoveLiquidityOne(address,uint256,uint256,uint256)"),
            "removeone/3arg",
            true,
        ),
        (
            keccak256("RemoveLiquidityOne(address,uint256,uint256,uint256,uint256,uint256)"),
            "removeone/ng6arg",
            true,
        ),
    ];
    for n in [2usize, 3] {
        v.push((
            keccak256(
                format!("AddLiquidity(address,uint256[{n}],uint256[{n}],uint256,uint256)")
                    .as_bytes(),
            ),
            "add/stable",
            true,
        ));
        v.push((
            keccak256(format!("AddLiquidity(address,uint256[{n}],uint256,uint256)").as_bytes()),
            "add/crypto",
            true,
        ));
        v.push((
            keccak256(
                format!("AddLiquidity(address,uint256[{n}],uint256,uint256,uint256)").as_bytes(),
            ),
            "add/cryptong",
            true,
        ));
        v.push((
            keccak256(
                format!("RemoveLiquidity(address,uint256[{n}],uint256[{n}],uint256)").as_bytes(),
            ),
            "remove/stable",
            true,
        ));
        v.push((
            keccak256(format!("RemoveLiquidity(address,uint256[{n}],uint256)").as_bytes()),
            "remove/crypto",
            true,
        ));
        v.push((
            keccak256(
                format!(
                    "RemoveLiquidityImbalance(address,uint256[{n}],uint256[{n}],uint256,uint256)"
                )
                .as_bytes(),
            ),
            "imbalance/stable",
            true,
        ));
    }
    v
}

fn ctx_from_log(log: &RpcLog) -> ReactiveContext {
    let number = log.block_number.unwrap_or_default();
    let block = BlockRef {
        number,
        hash: log.block_hash.unwrap_or_default(),
        parent_hash: None,
        timestamp: log.block_timestamp,
    };
    ReactiveContext {
        chain_id: Some(1),
        source: InputSource::Synthetic,
        chain_status: ChainStatus::Included {
            block: block.clone(),
            confirmations: 0,
        },
        block: Some(block),
        transaction_index: log.transaction_index,
        log_index: log.log_index,
    }
}

fn curve_registration(
    pool: Address,
    coins: Vec<Address>,
    variant: CurveVariant,
) -> PoolRegistration {
    PoolRegistration::new(PoolKey::Curve(pool))
        .with_state_address(pool)
        .with_metadata(ProtocolMetadata::Curve(
            CurveMetadata::default()
                .with_coins(coins)
                .with_discovered_slots(Vec::new())
                .with_variant(variant),
        ))
}

/// On-chain `get_dy(0, 1, dx)` at `block` for the pool's variant (ground truth).
async fn get_dy_at(
    provider: &RootProvider<AnyNetwork>,
    pool: Address,
    variant: CurveVariant,
    dx: U256,
    block: u64,
) -> Result<U256> {
    let calldata = match variant {
        CurveVariant::StableSwap => Bytes::from(
            get_dyCall {
                i: 0i128,
                j: 1i128,
                dx,
            }
            .abi_encode(),
        ),
        CurveVariant::CryptoSwap | CurveVariant::CryptoSwapNG => Bytes::from(
            CurveCryptoSwap::get_dyCall {
                i: U256::ZERO,
                j: U256::from(1),
                dx,
            }
            .abi_encode(),
        ),
        other => panic!("unexpected CurveVariant: {other:?}"),
    };
    let tx = TransactionRequest::default()
        .with_to(pool)
        .with_input(calldata);
    let out = provider
        .call(tx.into())
        .block(BlockId::Number(BlockNumberOrTag::Number(block)))
        .await
        .with_context(|| format!("eth_call get_dy at {block}"))?;
    Ok(match variant {
        CurveVariant::StableSwap => get_dyCall::abi_decode_returns_validate(&out)?,
        CurveVariant::CryptoSwap | CurveVariant::CryptoSwapNG => {
            CurveCryptoSwap::get_dyCall::abi_decode_returns_validate(&out)?
        }
        other => panic!("unexpected CurveVariant: {other:?}"),
    })
}

/// Live accuracy at head `m`: fresh pinned cache → cold-start → simulate_swap ==
/// eth_call get_dy. Proves sim is accurate against the live chain for `variant`.
async fn assert_live_accuracy(
    provider: Arc<RootProvider<AnyNetwork>>,
    pool: Address,
    coins: Vec<Address>,
    variant: CurveVariant,
    dx: U256,
    m: u64,
) -> Result<()> {
    let mut cache = EvmCache::at_block(
        provider.clone(),
        BlockId::Number(BlockNumberOrTag::Number(m)),
    )
    .await;
    let mut reg = curve_registration(pool, coins.clone(), variant);
    let mut cold = AdapterRegistry::new();
    cold.register_adapter(Arc::new(CurveAdapter::default()))?;
    cold.cold_start(&mut reg, &mut cache, ColdStartPolicy::Eager)?;
    let sim = CurveAdapter::default()
        .simulate_swap(
            &reg,
            &mut cache,
            coins[0],
            coins[1],
            dx,
            &SimConfig::default(),
        )
        .map_err(|e| anyhow!("simulate_swap {variant:?}: {e}"))?;
    let truth = get_dy_at(&provider, pool, variant, dx, m).await?;
    eprintln!(
        "[curve-ws] accuracy {variant:?} @ {m}: sim={} eth_call={}",
        sim.amount_out, truth
    );
    assert!(
        truth > U256::ZERO,
        "{variant:?} ground-truth get_dy should be non-zero"
    );
    assert_eq!(
        sim.amount_out, truth,
        "{variant:?} sim must match eth_call get_dy @ {m}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "live WS soak against E2E_RPC_URL; run with --ignored --nocapture"]
async fn ws_curve_liquidity_events_flow_route_and_stay_accurate() -> Result<()> {
    let Ok(rpc) = std::env::var("E2E_RPC_URL") else {
        eprintln!("E2E_RPC_URL unset; skipping");
        return Ok(());
    };
    let ws_url = std::env::var("ETH_WS_URL").unwrap_or_else(|_| {
        rpc.replacen("https://", "wss://", 1)
            .replacen("http://", "ws://", 1)
    });
    let secs: u64 = std::env::var("E2E_WS_SECONDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(180);

    let provider = Arc::new(
        RootProvider::<AnyNetwork>::connect(&ws_url)
            .await
            .context("connect wss://")?,
    );
    let b0 = provider.get_block_number().await?;
    eprintln!(
        "[curve-ws] B0={b0}; cold-starting 3pool (StableSwap) + tricrypto2 (CryptoSwap); soak {secs}s"
    );

    // Head-tracking cache (NOT pinned): Curve resync re-reads the backend, so a
    // pinned backend would re-read stale cold-start state. Unpinned lets resyncs
    // pull fresh post-event state.
    let mut cache = EvmCache::new(provider.clone()).await;
    let mut registry = AdapterRegistry::new();
    registry.register_adapter(Arc::new(CurveAdapter::default()))?;
    for (pool, coins, variant) in [
        (THREEPOOL, vec![DAI, USDC, USDT], CurveVariant::StableSwap),
        (TRICRYPTO2, vec![USDT, WBTC, WETH], CurveVariant::CryptoSwap),
        (
            TRICRYPTO_USDC_NG,
            vec![USDC, WBTC, WETH],
            CurveVariant::CryptoSwapNG,
        ),
    ] {
        let mut reg = curve_registration(pool, coins, variant);
        registry.cold_start(&mut reg, &mut cache, ColdStartPolicy::Eager)?;
        let sources = CurveAdapter::default().event_sources(&reg);
        registry.register_pool(reg.with_event_sources(sources))?;
    }
    let mut runtime = ReactiveRuntime::<Ethereum>::new(ReactiveConfig::default());
    runtime.register_handler(Arc::new(AmmReactiveHandler::new(registry)))?;

    // Subscribe TOPIC-ONLY to the union of all Curve event topics (this provider
    // does not push address-filtered subs reliably; the handler routes by emitter).
    let topics = curve_topics();
    let liquidity_topics: HashMap<B256, &'static str> = topics
        .iter()
        .filter(|(_, _, liq)| *liq)
        .map(|(h, l, _)| (*h, *l))
        .collect();
    let topic0_set: Vec<B256> = topics.iter().map(|(h, _, _)| *h).collect();
    let filter = Filter::new().event_signature(topic0_set);
    let mut stream = provider
        .subscribe_logs(&filter)
        .await
        .context("subscribe_logs (wss)")?
        .into_stream();

    let mut liquidity_seen: HashMap<&'static str, u64> = HashMap::new();
    let mut ecosystem_events = 0u64;
    let mut routed = 0u64;
    let mut routed_liquidity = 0u64;
    let mut stream_ended = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => break,
            maybe = stream.next() => {
                let Some(log) = maybe else {
                    stream_ended = true;
                    break;
                };
                ecosystem_events += 1;
                // (1) ecosystem-wide liquidity-signature delivery.
                if let Some(label) = log.topics().first().and_then(|t| liquidity_topics.get(t)) {
                    *liquidity_seen.entry(label).or_default() += 1;
                }
                let is_liq = log.topics().first().is_some_and(|t| liquidity_topics.contains_key(t));
                // (2) route through the runtime; only registered pools apply.
                let ctx = ctx_from_log(&log);
                let batch = ReactiveInputBatch::new(vec![ReactiveInputRecord::new(ReactiveInput::Log(log), ctx)]);
                let report = runtime.ingest_batch(&mut cache, batch)?;
                if !report.applied.is_empty() {
                    routed += 1;
                    if is_liq {
                        routed_liquidity += 1;
                        eprintln!("[curve-ws] routed a LIQUIDITY event to a registered pool (total {routed_liquidity})");
                    }
                }
            }
        }
    }
    let total_liq: u64 = liquidity_seen.values().sum();
    eprintln!(
        "[curve-ws] window done: {ecosystem_events} matching events seen ecosystem-wide; routed {routed} registered-pool events ({routed_liquidity} liquidity); {total_liq} liquidity events: {liquidity_seen:?}"
    );

    assert!(
        !stream_ended,
        "Curve WebSocket subscription ended during soak"
    );
    assert!(
        ecosystem_events > 0,
        "no Curve events matched the derived topics in {secs}s"
    );

    // Market activity is not deterministic. Strict manual soaks can require a
    // matching ecosystem event and an event from one of the registered pools;
    // the release matrix separately proves WebSocket delivery with its broad
    // V2 health probe and proves Curve routing/resync with offline fixtures.
    let require_activity = std::env::var("E2E_REQUIRE_CURVE_ACTIVITY")
        .is_ok_and(|value| matches!(value.as_str(), "1" | "true" | "yes" | "on"));
    if require_activity {
        assert!(
            total_liq > 0,
            "no liquidity events matched our derived topics in {secs}s"
        );
        assert!(
            routed > 0,
            "no events routed to the registered pools in {secs}s"
        );
    } else if routed == 0 {
        eprintln!(
            "[curve-ws] registered pools were inactive; continuing with post-soak live parity"
        );
    }

    // (3) Live accuracy for ALL THREE variants at the post-soak head.
    let m = provider.get_block_number().await?;
    assert_live_accuracy(
        provider.clone(),
        THREEPOOL,
        vec![DAI, USDC, USDT],
        CurveVariant::StableSwap,
        U256::from(1_000_000_000_000_000_000u64),
        m,
    )
    .await?;
    assert_live_accuracy(
        provider.clone(),
        TRICRYPTO2,
        vec![USDT, WBTC, WETH],
        CurveVariant::CryptoSwap,
        U256::from(100_000_000u64),
        m,
    )
    .await?;
    assert_live_accuracy(
        provider.clone(),
        TRICRYPTO_USDC_NG,
        vec![USDC, WBTC, WETH],
        CurveVariant::CryptoSwapNG,
        U256::from(1_000_000u64),
        m,
    )
    .await?;

    eprintln!(
        "[curve-ws] PASS: subscription stayed healthy, observed activity is reported above, and sim is accurate at head for all three variants (StableSwap/CryptoSwap/CryptoSwapNG)."
    );
    Ok(())
}
