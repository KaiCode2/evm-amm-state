//! Live RPC parity for **event-sourced** Uniswap V3 `Mint`/`Burn` (env-gated,
//! `#[ignore]`).
//!
//! For a real add-liquidity (`Mint`) and remove-liquidity (`Burn`) transaction,
//! this fetches the transaction's *exact per-tx* storage diff via
//! `trace_replayTransaction(stateDiff)` — the on-chain ground truth, immune to
//! other transactions in the same block — warms the pre-tx (`from`) values into a
//! cache, applies the event through the adapter, and asserts the adapter's
//! event-sourced writes reproduce the on-chain post-tx (`to`) values for every
//! slot the adapter maintains (each boundary tick's packed `Tick.Info` word 0 and
//! the in-range global `liquidity`).
//!
//! The pinned pair is a just-in-time (JIT) add + remove of the same liquidity on
//! the same in-range, already-initialized ticks of the USDC/WETH 0.05% pool, so
//! it exercises `liquidityGross`/`liquidityNet` (including a negative net in
//! two's complement) and the in-range liquidity delta against real state.
//!
//! Run: `E2E_RPC_URL=<archive+trace endpoint> cargo test --test v3_liquidity_rpc -- --ignored --nocapture`

use std::collections::HashMap;
use std::sync::Arc;

use alloy_eips::BlockId;
use alloy_primitives::{Address, B256, U256, address};
use alloy_provider::{Provider, RootProvider, network::AnyNetwork};
use alloy_rpc_types_eth::Filter;
use anyhow::Result;
use evm_amm_state::adapters::driver::AdapterDriver;
use evm_amm_state::adapters::storage::{
    V3StorageLayout, v3_tick_bitmap_storage_key_with_base, v3_tick_info_storage_keys_with_base,
    v3_word_position,
};
use evm_amm_state::adapters::{
    AdapterCache, AdapterRegistry, AmmAdapter, ConcentratedLiquidityAdapter, PoolKey,
    PoolRegistration, ProtocolMetadata, StateUpdate, V3Metadata,
};
use evm_fork_cache::cache::EvmCache;

const POOL: Address = address!("88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640");
const TICK_SPACING: i32 = 10;
const TICK_LOWER: i32 = 201_470;
const TICK_UPPER: i32 = 201_480;
const EVENT_BLOCK: u64 = 0x184c2a1; // block containing both the mint and the burn

// A JIT add (Mint) and remove (Burn) of the same liquidity on [201470, 201480].
const MINT_TX: &str = "0xdfd6176e2c22e1acad7d7ebfff14541c2c5ed0c31ec06b4812391c200bbac5a5";
const BURN_TX: &str = "0x9d230517d95e119b6a4b5c300c1184455018074ab361df3368330358cdb88e4c";

fn mint_topic() -> B256 {
    alloy_primitives::keccak256("Mint(address,address,int24,int24,uint128,uint256,uint256)")
}
fn burn_topic() -> B256 {
    alloy_primitives::keccak256("Burn(address,int24,int24,uint128,uint256,uint256)")
}

fn parse_u256(hex: &str) -> U256 {
    U256::from_str_radix(hex.trim_start_matches("0x"), 16).unwrap_or(U256::ZERO)
}

/// Fetch the per-tx storage diff for `POOL`: slot -> (from, to).
async fn pool_state_diff(
    provider: &RootProvider<AnyNetwork>,
    tx: &str,
) -> Result<HashMap<U256, (U256, U256)>> {
    let value: serde_json::Value = provider
        .raw_request(
            "trace_replayTransaction".into(),
            (tx.to_string(), vec!["stateDiff".to_string()]),
        )
        .await?;

    let pool_lc = format!("{POOL:?}").to_lowercase();
    let mut diff = HashMap::new();
    let Some(accounts) = value["stateDiff"].as_object() else {
        anyhow::bail!("trace_replayTransaction returned no stateDiff (trace unsupported?)");
    };
    for (addr, account) in accounts {
        if addr.to_lowercase() != pool_lc {
            continue;
        }
        if let Some(storage) = account["storage"].as_object() {
            for (slot_hex, change) in storage {
                let slot = parse_u256(slot_hex);
                if let Some(star) = change.get("*") {
                    diff.insert(
                        slot,
                        (
                            parse_u256(star["from"].as_str().unwrap_or("0x0")),
                            parse_u256(star["to"].as_str().unwrap_or("0x0")),
                        ),
                    );
                } else if let Some(plus) = change.get("+") {
                    diff.insert(
                        slot,
                        (U256::ZERO, parse_u256(plus.as_str().unwrap_or("0x0"))),
                    );
                }
            }
        }
    }
    Ok(diff)
}

async fn run_parity(is_mint: bool) -> Result<()> {
    let Ok(url) = std::env::var("E2E_RPC_URL") else {
        eprintln!("E2E_RPC_URL unset — skipping V3 liquidity RPC parity test.");
        return Ok(());
    };
    let (tx, topic0) = if is_mint {
        (MINT_TX, mint_topic())
    } else {
        (BURN_TX, burn_topic())
    };

    let provider = Arc::new(RootProvider::<AnyNetwork>::connect(&url).await?);
    let diff = pool_state_diff(&provider, tx).await?;

    let layout = V3StorageLayout::uniswap(TICK_SPACING);
    let lower_w0 = v3_tick_info_storage_keys_with_base(TICK_LOWER, layout.ticks_base_slot)[0];
    let upper_w0 = v3_tick_info_storage_keys_with_base(TICK_UPPER, layout.ticks_base_slot)[0];
    let bitmap = v3_tick_bitmap_storage_key_with_base(
        v3_word_position(TICK_LOWER, TICK_SPACING),
        layout.tick_bitmap_base_slot,
    );

    // The adapter maintains exactly these on a warm in-range liquidity event.
    let asserted = [
        ("tickLower.word0", lower_w0),
        ("tickUpper.word0", upper_w0),
        ("global liquidity", layout.liquidity_slot),
    ];
    // Every asserted slot must actually be one the tx changed (else the test is
    // vacuous / mis-mapped).
    for (name, slot) in asserted {
        assert!(
            diff.contains_key(&slot),
            "{name} ({slot:#x}) not in the tx storage diff — mapping/pin is wrong"
        );
    }
    // These ticks are already initialized (other LPs hold them), so this event
    // does not flip the bitmap — and the adapter must not touch it.
    assert!(
        !diff.contains_key(&bitmap),
        "unexpected bitmap change: pinned event should not (de)initialize a tick"
    );

    // Warm the slots the decode reads. Written slots (the tick word0s and, for an
    // in-range event, global liquidity) use the exact per-tx `from`. Read-only
    // slots — `slot0` (a burn reads it to decide in-range but does not write it)
    // and the unchanged `bitmap` — are absent from the stateDiff, so fall back to
    // their pre-block chain value.
    let mut cache = EvmCache::at_block(provider.clone(), BlockId::number(EVENT_BLOCK - 1)).await;
    let pre = |slot: U256| diff.get(&slot).map(|(from, _)| *from);
    let pre_block = BlockId::number(EVENT_BLOCK - 1);
    let slot0_pre = match pre(layout.slot0_slot) {
        Some(from) => from,
        None => {
            provider
                .get_storage_at(POOL, layout.slot0_slot)
                .block_id(pre_block)
                .await?
        }
    };
    let bitmap_pre = provider
        .get_storage_at(POOL, bitmap)
        .block_id(pre_block)
        .await?;
    AdapterCache::apply_updates(
        &mut cache,
        &[
            StateUpdate::slot(POOL, layout.slot0_slot, slot0_pre),
            StateUpdate::slot(
                POOL,
                layout.liquidity_slot,
                pre(layout.liquidity_slot).unwrap(),
            ),
            StateUpdate::slot(POOL, lower_w0, pre(lower_w0).unwrap()),
            StateUpdate::slot(POOL, upper_w0, pre(upper_w0).unwrap()),
            StateUpdate::slot(POOL, bitmap, bitmap_pre),
        ],
    );

    // Register the pool and apply the real event through the adapter.
    let adapter = Arc::new(ConcentratedLiquidityAdapter::default());
    let mut registration = PoolRegistration::new(PoolKey::UniswapV3(POOL))
        .with_state_address(POOL)
        .with_metadata(ProtocolMetadata::UniswapV3(
            V3Metadata::default()
                .with_storage_layout(layout)
                .with_tick_spacing(TICK_SPACING),
        ));
    let sources = adapter.event_sources(&registration);
    registration = registration.with_event_sources(sources);

    let mut registry = AdapterRegistry::new();
    registry.register_adapter(adapter).unwrap();
    registry.register_pool(registration).unwrap();
    let driver = AdapterDriver::new(registry);

    // Fetch the real event log from the chain (its exact topics + data) rather
    // than reconstructing it, so the test is immune to hand-copied hex errors.
    let tx_hash: B256 = tx.parse()?;
    let filter = Filter::new()
        .address(POOL)
        .event_signature(topic0)
        .from_block(EVENT_BLOCK)
        .to_block(EVENT_BLOCK);
    let log = provider
        .get_logs(&filter)
        .await?
        .into_iter()
        .find(|entry| entry.transaction_hash == Some(tx_hash))
        .expect("event log not found in block")
        .inner;

    driver
        .apply_log(&mut cache, &log)?
        .expect("event must route and apply");

    // The adapter's event-sourced writes must equal the on-chain post-tx values.
    for (name, slot) in asserted {
        let (_from, to) = diff[&slot];
        assert_eq!(
            cache.cached_storage_value(POOL, slot),
            Some(to),
            "{name} ({slot:#x}): event-sourced value != on-chain post-tx value"
        );
        eprintln!(
            "{}  {name}: matches on-chain {to:#x}",
            if is_mint { "MINT" } else { "BURN" }
        );
    }
    // The bitmap was not flipped, so it is left exactly as warmed.
    assert_eq!(cache.cached_storage_value(POOL, bitmap), Some(bitmap_pre));
    Ok(())
}

#[tokio::test]
#[ignore = "requires an archive+trace RPC via E2E_RPC_URL; run with --ignored"]
async fn mint_event_sourcing_matches_onchain_state_diff() -> Result<()> {
    run_parity(true).await
}

#[tokio::test]
#[ignore = "requires an archive+trace RPC via E2E_RPC_URL; run with --ignored"]
async fn burn_event_sourcing_matches_onchain_state_diff() -> Result<()> {
    run_parity(false).await
}
