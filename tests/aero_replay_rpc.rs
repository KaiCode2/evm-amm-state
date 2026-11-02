//! Live multi-block differential replay of event-driven Aero pool state
//! (env-gated, `#[ignore]`).
//!
//! For each chain the reviewed Aero pool is cold-started at the start of a
//! window that contains real activity, every pool log of the following blocks
//! is applied through the adapter driver with exact block context, and the
//! resulting quote-surface state is compared against `eth_getStorageAt` at
//! each event block and at the end of the window. Every applied transition
//! must be exact (no repair request), so the whole window runs without a
//! single provider read on the event path.
//!
//! Windows (chosen for activity before the migration evidence pins):
//! - Ethereum `0xff35aa95…` blocks 26291760..=26291970 (swaps at 26291767,
//!   26291870, 26291964).
//! - Base `0xEEBE78d7…` blocks 53203890..=53203960 (swaps at 53203896 and
//!   53203953 plus the staked CLM rebalance burns/collects/mints at 53203953).
//! - Optimism `0xC71B571a…` blocks 158757170..=158757240 (volatile swaps and
//!   `Sync` events).
//!
//! ```text
//! E2E_RPC_URL=<alchemy archive url> cargo test --test aero_replay_rpc -- --ignored --nocapture
//! ```

use std::sync::Arc;

use alloy_eips::{BlockId, BlockNumberOrTag};
use alloy_primitives::{Address, B256, U256, address, b256};
use alloy_provider::{Provider, RootProvider, network::AnyNetwork};
use alloy_rpc_types_eth::Filter;
use anyhow::{Context, Result, anyhow, ensure};

use evm_amm_state::adapters::driver::AdapterDriver;
use evm_amm_state::adapters::storage::{
    AERO_CL_OBSERVATIONS_SLOT, AERO_CL_STAKED_LIQUIDITY_SLOT, SolidlyStorageLayout,
    V3StorageLayout, slipstream_tick_info_storage_keys_with_base,
    v3_tick_bitmap_storage_key_with_base, v3_word_position,
};
use evm_amm_state::adapters::{
    AdapterEventContext, AdapterEventKind, AdapterRegistry, ColdStartOutcome, ColdStartPolicy,
    ConcentratedLiquidityAdapter, PoolKey, PoolRegistration, ProtocolMetadata, RepairAction,
    SolidlyV2Adapter, SolidlyV2Metadata, UpdateQuality, V3Metadata,
};
use evm_fork_cache::cache::EvmCache;

const METAQUOTER: Address = address!("1dDfeC5058180C207F55BB77D95AcDd0d8227868");
const ETH_POOL: Address = address!("ff35aa95669b5668AC6727FdcFc0D5582cdd78D1");
const BIFI: Address = address!("B1F1ee126e9c96231Cc3d3fAD7C08b4cf873b1f1");
const ETH_WETH: Address = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
const BASE_POOL: Address = address!("EEBE78d734dC4bAC027061cEE9bc12C5803fE4f8");
const L2_WETH: Address = address!("4200000000000000000000000000000000000006");
const MOO_BIFI: Address = address!("c55E93C62874D8100dBd2DfE307EDc1036ad5434");
const OP_POOL: Address = address!("C71B571aA9F06BFA096c24e9Df5C45fC4fAB4151");
const WORD_128_MASK: U256 = U256::from_limbs([u64::MAX, u64::MAX, 0, 0]);
/// Pool mutations the replay must route and apply exactly: V3 `Swap`, `Mint`,
/// `Burn`, `Collect` and the Solidly `Sync`.
const MUTATION_TOPICS: [B256; 5] = [
    b256!("c42079f94a6350d7e6235f29174924f928cc2ac818eb64fed8004e115fbcca67"),
    b256!("7a53080ba414158be7ec69b987b5fb7d07dee101fe85488f0853ae16239d0bde"),
    b256!("0c396cd989a39f4459b5fa1aed6a9a8dcdbc45908acfd67e028cd568da98982c"),
    b256!("70935338e69775456a85ddef226c395fb668b63fa0115f5f20610b388e6ca9c0"),
    b256!("cf2aa50876cdfbb541206f89af0ee78d44a2abf8d328e37fa4917f982149848a"),
];

#[path = "support/aero_rpc.rs"]
mod aero_rpc;
use aero_rpc::{derived_url, eth_url};

struct Window {
    start: u64,
    end: u64,
}

async fn storage_at(
    provider: &RootProvider<AnyNetwork>,
    pool: Address,
    slot: U256,
    block: u64,
) -> Result<U256> {
    Ok(provider
        .get_storage_at(pool, slot)
        .block_id(BlockId::Number(BlockNumberOrTag::Number(block)))
        .await?)
}

/// Compare the cache's quote-surface state for the Aero CL pool against the
/// chain at `block`.
async fn compare_cl(
    provider: &RootProvider<AnyNetwork>,
    cache: &EvmCache,
    pool: Address,
    layout: V3StorageLayout,
    block: u64,
) -> Result<usize> {
    let mut compared = 0;
    let slot0 = storage_at(provider, pool, layout.slot0_slot, block).await?;
    ensure!(
        cache.cached_storage_value(pool, layout.slot0_slot) == Some(slot0),
        "block {block}: slot0 {:?} vs chain {slot0:#x}",
        cache.cached_storage_value(pool, layout.slot0_slot)
    );
    compared += 1;
    let liquidity = storage_at(provider, pool, layout.liquidity_slot, block).await?;
    ensure!(
        cache.cached_storage_value(pool, layout.liquidity_slot) == Some(liquidity),
        "block {block}: liquidity word mismatch"
    );
    compared += 1;
    let staked = storage_at(provider, pool, AERO_CL_STAKED_LIQUIDITY_SLOT, block).await?;
    let cached_staked = cache
        .cached_storage_value(pool, AERO_CL_STAKED_LIQUIDITY_SLOT)
        .ok_or_else(|| anyhow!("staked word must stay resident"))?;
    if cached_staked & WORD_128_MASK != staked & WORD_128_MASK {
        eprintln!(
            "  block {block}: stakedLiquidity drifted (cache {} vs chain {}): gauge stake/unstake emits no pool event; a later swap replay fails closed into repair",
            cached_staked & WORD_128_MASK,
            staked & WORD_128_MASK
        );
    }
    // Bitmap words and word 0 of every initialized tick within ±2 words of
    // the current tick.
    let tick_raw = ((slot0 >> 160_usize) & U256::from(0x00ff_ffff_u32)).to::<u32>();
    let tick = if tick_raw & 0x0080_0000 != 0 {
        (tick_raw | 0xff00_0000) as i32
    } else {
        tick_raw as i32
    };
    let current_word = v3_word_position(tick, layout.tick_spacing);
    for word in (current_word - 2)..=(current_word + 2) {
        let key = v3_tick_bitmap_storage_key_with_base(word, layout.tick_bitmap_base_slot);
        let bitmap = storage_at(provider, pool, key, block).await?;
        ensure!(
            cache.cached_storage_value(pool, key) == Some(bitmap),
            "block {block}: bitmap word {word} mismatch"
        );
        compared += 1;
        for bit in 0..256_u32 {
            if (bitmap >> bit) & U256::from(1) != U256::from(1) {
                continue;
            }
            let tick = (word as i32 * 256 + bit as i32) * layout.tick_spacing;
            let keys = slipstream_tick_info_storage_keys_with_base(tick, layout.ticks_base_slot);
            let word0 = storage_at(provider, pool, keys[0], block).await?;
            ensure!(
                cache.cached_storage_value(pool, keys[0]) == Some(word0),
                "block {block}: tick {tick} word 0 mismatch"
            );
            compared += 1;
        }
    }
    // The current observation and its predecessor.
    let index = ((slot0 >> 184_usize) & U256::from(u16::MAX)).to::<u16>();
    let cardinality = ((slot0 >> 200_usize) & U256::from(u16::MAX))
        .to::<u16>()
        .max(1);
    for entry in [index, (index + cardinality - 1) % cardinality] {
        let slot = AERO_CL_OBSERVATIONS_SLOT + U256::from(entry);
        let observation = storage_at(provider, pool, slot, block).await?;
        ensure!(
            cache.cached_storage_value(pool, slot) == Some(observation),
            "block {block}: observation {entry} mismatch"
        );
        compared += 1;
    }
    Ok(compared)
}

async fn compare_volatile(
    provider: &RootProvider<AnyNetwork>,
    cache: &EvmCache,
    pool: Address,
    block: u64,
) -> Result<usize> {
    let layout = SolidlyStorageLayout::aero_volatile();
    for slot in [layout.reserve0_slot, layout.reserve1_slot] {
        let value = storage_at(provider, pool, slot, block).await?;
        ensure!(
            cache.cached_storage_value(pool, slot) == Some(value),
            "block {block}: reserve slot {slot} mismatch"
        );
    }
    Ok(2)
}

enum Venue {
    Cl(V3StorageLayout),
    Volatile,
}

async fn replay(
    url: &str,
    pool: Address,
    registration: PoolRegistration,
    adapter_registry: AdapterRegistry,
    venue: Venue,
    window: Window,
) -> Result<()> {
    let provider = Arc::new(
        RootProvider::<AnyNetwork>::connect(url)
            .await
            .context("connect RPC url")?,
    );
    let mut cache = EvmCache::at_block(
        Arc::clone(&provider),
        BlockId::Number(BlockNumberOrTag::Number(window.start)),
    )
    .await;
    let chain_id = cache.chain_id();
    let mut registry = adapter_registry;
    let mut pools = vec![registration];
    let outcomes = registry
        .cold_start_many(
            &mut pools,
            &mut cache,
            provider.as_ref(),
            ColdStartPolicy::Eager,
        )
        .await?;
    ensure!(
        matches!(outcomes[0], ColdStartOutcome::Ready(_)),
        "cold start at {} must be ready: {:?}",
        window.start,
        outcomes[0]
    );
    let registration = pools.remove(0);
    if let Venue::Cl(layout) = venue {
        ensure!(ConcentratedLiquidityAdapter::aero_cl_runtime_family(&registration).is_some());
        compare_cl(&provider, &cache, pool, layout, window.start).await?;
    } else {
        compare_volatile(&provider, &cache, pool, window.start).await?;
    }
    // Subscribe the pool's own event sources so the driver routes its logs.
    let sources = registry
        .adapter(registration.protocol())
        .ok_or_else(|| anyhow!("adapter registered"))?
        .event_sources(&registration);
    let registration = registration.with_event_sources(sources);
    registry.register_pool(registration)?;
    let driver = AdapterDriver::new(registry);

    let filter = Filter::new()
        .address(pool)
        .from_block(window.start + 1)
        .to_block(window.end);
    let mut logs = provider.get_logs(&filter).await?;
    logs.sort_by_key(|log| (log.block_number, log.log_index));
    ensure!(
        !logs.is_empty(),
        "the window {}..={} must contain pool events",
        window.start,
        window.end
    );
    let mut exact_events = 0;
    let mut compared_blocks = 0;
    let mut index = 0;
    while index < logs.len() {
        let number = logs[index].block_number.expect("block number");
        let header = provider
            .get_block_by_number(BlockNumberOrTag::Number(number))
            .await?
            .ok_or_else(|| anyhow!("block {number} missing"))?;
        let block_hash = header.header.hash;
        let parent_hash = header.header.parent_hash;
        let timestamp = header.header.timestamp;
        while index < logs.len() && logs[index].block_number == Some(number) {
            let log = &logs[index];
            index += 1;
            let context = AdapterEventContext::for_block(number, block_hash, timestamp)
                .with_chain_id(chain_id)
                .with_parent_hash(parent_hash)
                .with_transaction_hash(log.transaction_hash.expect("tx hash"))
                .with_event_order(
                    log.transaction_index.expect("tx index"),
                    log.log_index.expect("log index"),
                );
            let report = driver
                .apply_log_with_context(&mut cache, &log.inner, &context)
                .map_err(|error| anyhow!("block {number} log {:?}: {error}", log.log_index))?;
            let topic0 = log.inner.topics().first().copied();
            let known_mutation = topic0.is_some_and(|topic| MUTATION_TOPICS.contains(&topic));
            let Some(report) = report else {
                ensure!(
                    !known_mutation,
                    "block {number} log {:?}: a pool mutation was not routed",
                    log.log_index
                );
                continue;
            };
            let kind = report.event.kind;
            ensure!(
                report.event.quality == UpdateQuality::Exact
                    || (matches!(venue, Venue::Volatile)
                        && report.event.quality == UpdateQuality::ExactIfApplied),
                "block {number} log {:?} ({kind:?}) was not exact: {:?} / {:?}",
                log.log_index,
                report.event.quality,
                report.repair
            );
            ensure!(
                report.repair == RepairAction::None,
                "block {number} log {:?} ({kind:?}) requested a repair: {:?}",
                log.log_index,
                report.repair
            );
            ensure!(
                !report.applied.has_skipped(),
                "block {number} log {:?}: an exact write was cold-skipped",
                log.log_index
            );
            if matches!(
                kind,
                AdapterEventKind::Swap
                    | AdapterEventKind::LiquidityAdded
                    | AdapterEventKind::LiquidityRemoved
                    | AdapterEventKind::Sync
            ) {
                exact_events += 1;
            }
            eprintln!("  block {number} log {:?}: {kind:?} exact", log.log_index);
        }
        let compared = match venue {
            Venue::Cl(layout) => compare_cl(&provider, &cache, pool, layout, number).await?,
            Venue::Volatile => compare_volatile(&provider, &cache, pool, number).await?,
        };
        compared_blocks += 1;
        eprintln!("  block {number}: {compared} quote-surface cells match the chain");
    }
    let compared = match venue {
        Venue::Cl(layout) => compare_cl(&provider, &cache, pool, layout, window.end).await?,
        Venue::Volatile => compare_volatile(&provider, &cache, pool, window.end).await?,
    };
    ensure!(exact_events > 0, "no exact pool transition was replayed");
    eprintln!(
        "[{chain_id}] window {}..={} ({} blocks): {exact_events} exact transitions, {compared_blocks} event blocks compared, {compared} cells match at the end",
        window.start,
        window.end,
        window.end - window.start
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires an Ethereum archive RPC (E2E_RPC_URL); run with --ignored"]
async fn ethereum_aero_cl_window_replays_exactly() -> Result<()> {
    let url = eth_url()?;
    let mut registry = AdapterRegistry::new();
    registry.register_adapter(Arc::new(ConcentratedLiquidityAdapter::default()))?;
    let layout = V3StorageLayout::aero_cl(50);
    replay(
        &url,
        ETH_POOL,
        PoolRegistration::new(PoolKey::Slipstream(ETH_POOL))
            .with_state_address(ETH_POOL)
            .with_metadata(ProtocolMetadata::Slipstream(
                V3Metadata::default()
                    .with_token0(BIFI)
                    .with_token1(ETH_WETH)
                    .with_tick_spacing(50)
                    .with_storage_layout(layout)
                    .with_metaquoter(METAQUOTER),
            )),
        registry,
        Venue::Cl(layout),
        Window {
            start: 26_291_760,
            end: 26_291_970,
        },
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires a Base archive RPC (E2E_BASE_RPC_URL / E2E_RPC_URL on Alchemy); run with --ignored"]
async fn base_aero_cl_window_replays_exactly() -> Result<()> {
    let url = derived_url("E2E_BASE_RPC_URL", "base-mainnet")?;
    let mut registry = AdapterRegistry::new();
    registry.register_adapter(Arc::new(ConcentratedLiquidityAdapter::default()))?;
    let layout = V3StorageLayout::aero_cl(100);
    replay(
        &url,
        BASE_POOL,
        PoolRegistration::new(PoolKey::Slipstream(BASE_POOL))
            .with_state_address(BASE_POOL)
            .with_metadata(ProtocolMetadata::Slipstream(
                V3Metadata::default()
                    .with_token0(L2_WETH)
                    .with_token1(MOO_BIFI)
                    .with_tick_spacing(100)
                    .with_storage_layout(layout)
                    .with_metaquoter(METAQUOTER),
            )),
        registry,
        Venue::Cl(layout),
        Window {
            start: 53_203_890,
            end: 53_203_960,
        },
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires an Optimism archive RPC (E2E_OPTIMISM_RPC_URL / E2E_RPC_URL on Alchemy); run with --ignored"]
async fn optimism_aero_volatile_window_replays_exactly() -> Result<()> {
    let url = derived_url("E2E_OPTIMISM_RPC_URL", "opt-mainnet")?;
    let mut registry = AdapterRegistry::new();
    registry.register_adapter(Arc::new(SolidlyV2Adapter::default()))?;
    replay(
        &url,
        OP_POOL,
        PoolRegistration::new(PoolKey::SolidlyV2(OP_POOL))
            .with_state_address(OP_POOL)
            .with_metadata(ProtocolMetadata::SolidlyV2(
                SolidlyV2Metadata::default()
                    .with_token0(L2_WETH)
                    .with_token1(MOO_BIFI)
                    .with_aero_volatile(),
            )),
        registry,
        Venue::Volatile,
        Window {
            start: 158_757_170,
            end: 158_757_240,
        },
    )
    .await
}
