//! Live RPC parity for **event-sourced** Balancer V2 swaps (env-gated,
//! `#[ignore]`).
//!
//! For a real vault `Swap` on both a `TWO_TOKEN` pool (a single shared cash
//! slot, two 112-bit fields) and a `GENERAL` pool (per-token cash slots), this
//! fetches the transaction's *exact per-tx* storage diff via
//! `trace_replayTransaction(stateDiff)` — the on-chain ground truth, immune to
//! other transactions in the same block — derives each swapped token's cash
//! field location from that diff, warms the pre-tx (`from`) state into a cache,
//! applies the real `Swap` log through the adapter, and asserts the adapter's
//! event-sourced writes reproduce the on-chain post-tx (`to`) `cash` field for
//! both `tokenIn` (`+amountIn`) and `tokenOut` (`-amountOut`).
//!
//! The `lastChangeBlock` field (top 32 bits) is deliberately *not* compared: the
//! event-sourced write maintains only `cash`, leaving the block stamp as warmed,
//! whereas the on-chain word also bumps the block. Quotes read `cash`, not the
//! stamp, so field-level parity is the correctness bar.
//!
//! Run: `E2E_RPC_URL=<archive+trace endpoint> cargo test --test balancer_liquidity_rpc -- --ignored --nocapture`

use std::collections::HashMap;
use std::sync::Arc;

use alloy_eips::BlockId;
use alloy_primitives::{Address, B256, U256, address};
use alloy_provider::{Provider, RootProvider, network::AnyNetwork};
use alloy_rpc_types_eth::Filter;
use anyhow::Result;
use evm_amm_state::adapters::driver::AdapterDriver;
use evm_amm_state::adapters::{
    AdapterCache, AdapterRegistry, AmmAdapter, BalancerTokenBalance, BalancerV2Adapter,
    BalancerV2Metadata, PoolKey, PoolRegistration, ProtocolMetadata, StateUpdate,
};
use evm_fork_cache::cache::EvmCache;

const VAULT: Address = address!("BA12222222228d8Ba445958a75a0704d566BF2C8");

// A `TWO_TOKEN` pool (80BAL/20WETH): both token balances share one vault slot,
// packed `[block:32][cash_high:112][cash_low:112]`.
const TWO_TOKEN_POOL: &str = "0x5c6ee304399dbdb9c8ef030ab642b10820db8f56000200000000000000000014";
const TWO_TOKEN_SWAP_TX: &str =
    "0x13e54562bc37d6d86a1f8a2840ff28821ff63d258f4fe8ce90464756ca44804c";

// A `GENERAL` pool (Balancer stable USDT/DAI/USDC): each token's balance lives in
// its own vault slot (low 112-bit `cash` field).
const GENERAL_POOL: &str = "0x06df3b2bbb68adc8b0e302443692037ed9f91b42000000000000000000000063";
const GENERAL_SWAP_TX: &str = "0xa109fec4cfc1c3336151594364afc71474f480f19d171eb031ce0ba47561f5e9";

const CASH_BITS: usize = 112;

fn swap_topic() -> B256 {
    alloy_primitives::keccak256("Swap(bytes32,address,address,uint256,uint256)")
}

fn parse_u256(hex: &str) -> U256 {
    U256::from_str_radix(hex.trim_start_matches("0x"), 16).unwrap_or(U256::ZERO)
}

/// Extract a packed vault balance's 112-bit `cash` field (low = bits 0–111, high
/// = bits 112–223). Mirrors the adapter's crate-internal `cash_field`.
fn cash_field(word: U256, high: bool) -> U256 {
    let mask = (U256::from(1) << CASH_BITS) - U256::from(1);
    let shift = if high { CASH_BITS } else { 0 };
    (word >> shift) & mask
}

/// Fetch the per-tx storage diff for `VAULT`: slot -> (from, to).
async fn vault_state_diff(
    provider: &RootProvider<AnyNetwork>,
    tx: &str,
) -> Result<HashMap<U256, (U256, U256)>> {
    let value: serde_json::Value = provider
        .raw_request(
            "trace_replayTransaction".into(),
            (tx.to_string(), vec!["stateDiff".to_string()]),
        )
        .await?;

    let vault_lc = format!("{VAULT:?}").to_lowercase();
    let mut diff = HashMap::new();
    let Some(accounts) = value["stateDiff"].as_object() else {
        anyhow::bail!("trace_replayTransaction returned no stateDiff (trace unsupported?)");
    };
    for (addr, account) in accounts {
        if addr.to_lowercase() != vault_lc {
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

/// Block number containing `tx` (via `eth_getTransactionByHash`).
async fn tx_block(provider: &RootProvider<AnyNetwork>, tx: &str) -> Result<u64> {
    let value: serde_json::Value = provider
        .raw_request("eth_getTransactionByHash".into(), (tx.to_string(),))
        .await?;
    let hex = value["blockNumber"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("tx {tx} has no blockNumber (not mined?)"))?;
    Ok(u64::from_str_radix(hex.trim_start_matches("0x"), 16)?)
}

/// Locate `token`'s cash field: the `(slot, high)` in `diff` whose 112-bit field
/// changed by exactly `signed_delta` (`+amount` for `tokenIn`, `-amount` for
/// `tokenOut`). Returns `None` if no field matches — a mis-pin or a fee-skimming
/// pool would fail here rather than silently pass.
fn locate_field(
    diff: &HashMap<U256, (U256, U256)>,
    amount: U256,
    is_in: bool,
) -> Option<(U256, bool)> {
    for (&slot, &(from, to)) in diff {
        for high in [false, true] {
            let (before, after) = (cash_field(from, high), cash_field(to, high));
            let matches = if is_in {
                after > before && after - before == amount
            } else {
                before > after && before - after == amount
            };
            if matches {
                return Some((slot, high));
            }
        }
    }
    None
}

async fn run_swap_parity(label: &str, pool_id_hex: &str, tx: &str) -> Result<()> {
    let Ok(url) = std::env::var("E2E_RPC_URL") else {
        eprintln!("E2E_RPC_URL unset — skipping Balancer swap RPC parity test.");
        return Ok(());
    };

    let provider = Arc::new(RootProvider::<AnyNetwork>::connect(&url).await?);
    let pool_id: B256 = pool_id_hex.parse()?;
    let tx_hash: B256 = tx.parse()?;
    let block = tx_block(&provider, tx).await?;
    let diff = vault_state_diff(&provider, tx).await?;
    anyhow::ensure!(!diff.is_empty(), "no vault storage diff for {label} swap");

    // Fetch the real Swap log (its exact topics + data) rather than
    // reconstructing it, so the test is immune to hand-copied hex errors. A
    // multi-hop tx emits one Swap per hop; select ours by poolId (topic1).
    let filter = Filter::new()
        .address(VAULT)
        .event_signature(swap_topic())
        .from_block(block)
        .to_block(block);
    let log = provider
        .get_logs(&filter)
        .await?
        .into_iter()
        .find(|entry| {
            entry.transaction_hash == Some(tx_hash)
                && entry.topic0() == Some(&swap_topic())
                && entry.inner.data.topics().get(1) == Some(&pool_id)
        })
        .expect("Swap log for this pool not found in block")
        .inner;

    let topics = log.data.topics();
    let token_in = Address::from_word(topics[2]);
    let token_out = Address::from_word(topics[3]);
    let amount_in = U256::from_be_slice(&log.data.data[0..32]);
    let amount_out = U256::from_be_slice(&log.data.data[32..64]);

    // Derive each token's cash-field location from the ground-truth per-tx diff:
    // tokenIn's field rose by +amountIn, tokenOut's fell by amountOut.
    let (in_slot, in_high) = locate_field(&diff, amount_in, true)
        .expect("tokenIn cash field (+amountIn) not found in the tx diff — mis-pin?");
    let (out_slot, out_high) = locate_field(&diff, amount_out, false)
        .expect("tokenOut cash field (-amountOut) not found in the tx diff — mis-pin?");
    eprintln!(
        "{label}: in={token_in} (+{amount_in}) @ {in_slot:#x}[{}]  out={token_out} (-{amount_out}) @ {out_slot:#x}[{}]",
        if in_high { "high" } else { "low" },
        if out_high { "high" } else { "low" }
    );
    if pool_id_hex == TWO_TOKEN_POOL {
        assert_eq!(
            in_slot, out_slot,
            "TWO_TOKEN pool: both tokens share one slot"
        );
    } else {
        assert_ne!(in_slot, out_slot, "GENERAL pool: tokens use distinct slots");
    }

    // Metadata a real cold-start would have recorded: the vault, the probed
    // token->cash map, and the balance slots (for the resync fallback).
    let mut balance_slots = vec![in_slot, out_slot];
    balance_slots.dedup();
    let metadata = BalancerV2Metadata::default()
        .with_vault(VAULT)
        .with_tokens([token_in, token_out])
        .with_balance_slots(balance_slots)
        .with_token_cash([
            BalancerTokenBalance::new(token_in, in_slot, in_high),
            BalancerTokenBalance::new(token_out, out_slot, out_high),
        ]);

    // Warm every changed vault slot with its exact per-tx `from` value (pre-swap
    // state), so the read-modify-write sees the true pre-image.
    let mut cache = EvmCache::at_block(provider.clone(), BlockId::number(block - 1)).await;
    let warm: Vec<StateUpdate> = diff
        .iter()
        .map(|(&slot, &(from, _))| StateUpdate::slot(VAULT, slot, from))
        .collect();
    AdapterCache::apply_updates(&mut cache, &warm);

    // Register the pool + adapter and apply the real Swap through the driver.
    let adapter = Arc::new(BalancerV2Adapter::default());
    let mut registration = PoolRegistration::new(PoolKey::BalancerV2(pool_id))
        .with_state_address(VAULT)
        .with_metadata(ProtocolMetadata::BalancerV2(metadata));
    let sources = adapter.event_sources(&registration);
    registration = registration.with_event_sources(sources);

    let mut registry = AdapterRegistry::new();
    registry.register_adapter(adapter).unwrap();
    registry.register_pool(registration).unwrap();
    let driver = AdapterDriver::new(registry);

    driver
        .apply_log(&mut cache, &log)?
        .expect("Swap must route and apply");

    // The event-sourced `cash` fields must equal the on-chain post-tx values.
    let (_, in_to) = diff[&in_slot];
    let (_, out_to) = diff[&out_slot];
    let in_word = cache
        .cached_storage_value(VAULT, in_slot)
        .expect("in slot cached");
    let out_word = cache
        .cached_storage_value(VAULT, out_slot)
        .expect("out slot cached");
    assert_eq!(
        cash_field(in_word, in_high),
        cash_field(in_to, in_high),
        "{label}: tokenIn cash != on-chain post-tx (expected +{amount_in})"
    );
    assert_eq!(
        cash_field(out_word, out_high),
        cash_field(out_to, out_high),
        "{label}: tokenOut cash != on-chain post-tx (expected -{amount_out})"
    );
    eprintln!(
        "{label}: OK  in cash -> {}  out cash -> {}",
        cash_field(in_to, in_high),
        cash_field(out_to, out_high)
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires an archive+trace RPC via E2E_RPC_URL; run with --ignored"]
async fn two_token_swap_event_sourcing_matches_onchain() -> Result<()> {
    run_swap_parity("TWO_TOKEN", TWO_TOKEN_POOL, TWO_TOKEN_SWAP_TX).await
}

#[tokio::test]
#[ignore = "requires an archive+trace RPC via E2E_RPC_URL; run with --ignored"]
async fn general_swap_event_sourcing_matches_onchain() -> Result<()> {
    run_swap_parity("GENERAL", GENERAL_POOL, GENERAL_SWAP_TX).await
}
