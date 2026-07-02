//! End-to-end pipeline tests on the adapters path: register → cold-start
//! (snapshot) → reactive event → cache mutation, composed on a SHARED
//! `EvmCache` + registry. The existing `adapter_reactive.rs` covers reactive
//! apply in isolation and `cold_start_adoption.rs` covers cold-start in
//! isolation; this file pins that they compose — the gap the readiness audit
//! flagged. It also includes full register→cold-start→react→simulate pipeline
//! tests for V2 and V3 (Balancer's full chain lives in `adapter_swap_sim.rs`).

use std::collections::HashMap;
use std::sync::Arc;

use alloy_eips::BlockId;
use alloy_network::Ethereum;
use alloy_primitives::{Address, B256, Bytes, Log as PrimitiveLog, U256, hex, keccak256};
use alloy_provider::{RootProvider, network::AnyNetwork};
use alloy_rpc_client::RpcClient;
use alloy_rpc_types_eth::Log as RpcLog;
use alloy_transport::mock::Asserter;
use anyhow::{Result, anyhow};
use evm_amm_state::adapters::storage::{
    V2_RESERVES_SLOT, V2_TOKEN0_SLOT, V2_TOKEN1_SLOT, V3_LIQUIDITY_SLOT, V3_SLOT0_SLOT,
    V3StorageLayout,
};
use evm_amm_state::adapters::{
    AdapterRegistry, AmmAdapter, AmmReactiveHandler, ColdStartOutcome, ColdStartPolicy,
    ConcentratedLiquidityAdapter, PoolKey, PoolRegistration, PoolStatus, ProtocolMetadata,
    SimConfig, UniswapV2Adapter, UniswapV2Metadata, V3Metadata,
};
use evm_fork_cache::cache::{EvmCache, StorageBatchFetchFn};
use evm_fork_cache::reactive::{
    BlockRef, ChainStatus, InputSource, ReactiveConfig, ReactiveContext, ReactiveInput,
    ReactiveInputBatch, ReactiveInputRecord, ReactiveRuntime,
};
use revm::state::{AccountInfo, Bytecode};

// --- harness (mirrors tests/adapter_reactive.rs conventions) ---

fn block_hash(block_number: u64) -> B256 {
    B256::repeat_byte(block_number as u8)
}

fn rpc_log(address: Address, topics: Vec<B256>, data: Vec<u8>, block_number: u64) -> RpcLog {
    RpcLog {
        inner: PrimitiveLog::new_unchecked(address, topics, Bytes::from(data)),
        block_hash: Some(block_hash(block_number)),
        block_number: Some(block_number),
        block_timestamp: Some(1_700_000_000 + block_number),
        transaction_hash: Some(B256::repeat_byte(1)),
        transaction_index: Some(0),
        log_index: Some(0),
        removed: false,
    }
}

fn included_context(block_number: u64) -> ReactiveContext {
    let block = BlockRef {
        number: block_number,
        hash: block_hash(block_number),
        parent_hash: Some(block_hash(block_number.saturating_sub(1))),
        timestamp: Some(1_700_000_000 + block_number),
    };
    ReactiveContext {
        chain_id: Some(1),
        source: InputSource::Synthetic,
        chain_status: ChainStatus::Included {
            block: block.clone(),
            confirmations: 0,
        },
        block: Some(block),
        transaction_index: Some(0),
        log_index: Some(0),
    }
}

fn batch(log: RpcLog, block_number: u64) -> ReactiveInputBatch<Ethereum> {
    ReactiveInputBatch::new(vec![ReactiveInputRecord::new(
        ReactiveInput::Log(log),
        included_context(block_number),
    )])
}

async fn setup_cache() -> Result<EvmCache> {
    let asserter = Asserter::new();
    let client = RpcClient::mocked(asserter);
    let provider = RootProvider::<AnyNetwork>::new(client);
    Ok(EvmCache::new(Arc::new(provider)).await)
}

fn stub_fetcher(values: HashMap<(Address, U256), U256>) -> StorageBatchFetchFn {
    Arc::new(
        move |requests: Vec<(Address, U256)>, _block: Option<BlockId>| {
            requests
                .into_iter()
                .map(|(address, slot)| {
                    let value = values.get(&(address, slot)).copied().unwrap_or_default();
                    (address, slot, Ok(value))
                })
                .collect()
        },
    )
}

fn v2_sync_topic() -> B256 {
    keccak256("Sync(uint112,uint112)")
}

fn token_word(address: Address) -> U256 {
    U256::from_be_slice(address.as_slice())
}

fn abi_words(values: impl IntoIterator<Item = U256>) -> Vec<u8> {
    values
        .into_iter()
        .flat_map(|v| v.to_be_bytes::<32>().to_vec())
        .collect()
}

fn low_mask(bits: usize) -> U256 {
    (U256::from(1) << bits) - U256::from(1)
}

/// Cold-start warms a V2 pool's reserves slot to its on-chain value, then a
/// reactive `Sync` event mutates that SAME warmed slot in the SAME cache.
#[tokio::test(flavor = "multi_thread")]
async fn v2_cold_start_then_reactive_sync_updates_warmed_reserves() -> Result<()> {
    let pool = Address::repeat_byte(0x91);
    let token0 = Address::repeat_byte(0xa0);
    let token1 = Address::repeat_byte(0xa1);

    // On-chain reserves the cold-start snapshot warms (timestamp in the top 32 bits).
    let cs_reserve0 = U256::from(1_000_u64);
    let cs_reserve1 = U256::from(2_000_u64);
    let cs_timestamp = U256::from(0x1234_u64);
    let cs_slot = cs_reserve0 | (cs_reserve1 << 112) | (cs_timestamp << 224);

    let adapter = Arc::new(UniswapV2Adapter::default());
    let mut registry = AdapterRegistry::new();
    registry.register_adapter(adapter.clone()).unwrap();

    let mut cache = setup_cache().await?;
    cache.set_storage_batch_fetcher(stub_fetcher(HashMap::from([
        ((pool, V2_RESERVES_SLOT), cs_slot),
        ((pool, V2_TOKEN0_SLOT), token_word(token0)),
        ((pool, V2_TOKEN1_SLOT), token_word(token1)),
    ])));

    // 1) Cold-start (snapshot) on an external registration.
    let mut registration = PoolRegistration::new(PoolKey::UniswapV2(pool)).with_state_address(pool);
    let outcome = registry.cold_start(&mut registration, &mut cache, ColdStartPolicy::Eager)?;
    assert!(
        matches!(outcome, ColdStartOutcome::Ready(_)),
        "cold-start should reach Ready, got {outcome:?}"
    );
    assert_eq!(registration.status, PoolStatus::Ready);
    assert_eq!(
        cache.cached_storage_value(pool, V2_RESERVES_SLOT),
        Some(cs_slot),
        "cold-start must warm the reserves slot to the on-chain value"
    );

    // 2) Register the cold-started pool for reactive routing and ingest a Sync
    //    carrying NEW reserves on the SAME cache + registry.
    let sources = adapter.event_sources(&registration);
    registry
        .register_pool(registration.with_event_sources(sources))
        .unwrap();

    let new_reserve0 = U256::from(1_500_u64);
    let new_reserve1 = U256::from(2_500_u64);
    let log = rpc_log(
        pool,
        vec![v2_sync_topic()],
        abi_words([new_reserve0, new_reserve1]),
        12,
    );

    let mut runtime = ReactiveRuntime::<Ethereum>::new(ReactiveConfig::default());
    runtime.register_handler(Arc::new(AmmReactiveHandler::new(registry)))?;
    let report = runtime.ingest_batch(&mut cache, batch(log, 12))?;
    assert_eq!(report.applied.len(), 1, "the Sync must apply exactly once");

    // 3) The cold-start-warmed slot now reflects the reactive event: low 224 bits
    //    overwritten with the new reserves, the timestamp high bits preserved.
    let raw = cache
        .cached_storage_value(pool, V2_RESERVES_SLOT)
        .expect("reserves slot is warm");
    assert_eq!(
        raw & low_mask(112),
        new_reserve0,
        "reserve0 updated by Sync"
    );
    assert_eq!(
        (raw >> 112) & low_mask(112),
        new_reserve1,
        "reserve1 updated by Sync"
    );
    assert_eq!(
        raw >> 224,
        cs_timestamp,
        "the masked Sync update preserves the cold-start timestamp bits"
    );
    Ok(())
}

// --- Full pipeline (register → cold-start → react → simulate), offline ---
//
// These exercise all three legs in one flow as CI-runnable regression coverage.
// The mock quote contract returns a seeded value (not a reserves-derived
// computation), so these prove the chain WIRES and runs end-to-end without
// error; the state-vs-quote correctness is proven separately by the RPC-parity
// and live-WebSocket tests. (Balancer's full chain — cold-start → Swap refresh →
// re-simulate — already lives in `tests/adapter_swap_sim.rs`.)

fn install_default_account(cache: &mut EvmCache, addr: Address) {
    cache
        .db_mut()
        .insert_account_info(addr, AccountInfo::default());
}

/// Install raw runtime bytecode (a compiled mock quote fixture) at `addr`.
fn install_mock_runtime(cache: &mut EvmCache, addr: Address, runtime: &str) {
    let code = Bytecode::new_raw(Bytes::from(
        hex::decode(runtime.trim()).expect("valid mock runtime hex"),
    ));
    let code_hash = code.hash_slow();
    cache.db_mut().insert_account_info(
        addr,
        AccountInfo {
            balance: U256::ZERO,
            nonce: 1,
            code: Some(code),
            code_hash,
            account_id: None,
        },
    );
}

fn topic_address(address: Address) -> B256 {
    let mut bytes = [0u8; 32];
    bytes[12..].copy_from_slice(address.as_slice());
    B256::from(bytes)
}

fn v3_swap_topic() -> B256 {
    keccak256("Swap(address,address,int256,int256,uint160,uint128,int24)")
}

#[tokio::test(flavor = "multi_thread")]
async fn v2_full_pipeline_cold_start_react_simulate() -> Result<()> {
    let pool = Address::repeat_byte(0x92);
    let router = Address::repeat_byte(0xb1);
    let token0 = Address::repeat_byte(0xa0);
    let token1 = Address::repeat_byte(0xa1);
    let quote_out = U256::from(4_242_u64);

    let mut cache = setup_cache().await?;
    install_default_account(&mut cache, Address::ZERO);
    // Mock router: getAmountsOut returns sload(0); seed it.
    install_mock_runtime(
        &mut cache,
        router,
        include_str!("fixtures/mock_v2_router_runtime.hex"),
    );
    cache
        .db_mut()
        .insert_account_storage(router, U256::ZERO, quote_out)?;
    let cs_slot = U256::from(1_000_u64) | (U256::from(2_000_u64) << 112);
    cache.set_storage_batch_fetcher(stub_fetcher(HashMap::from([
        ((pool, V2_RESERVES_SLOT), cs_slot),
        ((pool, V2_TOKEN0_SLOT), token_word(token0)),
        ((pool, V2_TOKEN1_SLOT), token_word(token1)),
    ])));

    let adapter = UniswapV2Adapter::default();
    let mut registry = AdapterRegistry::new();
    registry.register_adapter(Arc::new(UniswapV2Adapter::default()))?;
    let mut registration = PoolRegistration::new(PoolKey::UniswapV2(pool))
        .with_state_address(pool)
        .with_metadata(ProtocolMetadata::UniswapV2(
            UniswapV2Metadata::default()
                .with_token0(token0)
                .with_token1(token1)
                .with_fee_bps(30),
        ));

    // 1) cold-start
    assert!(matches!(
        registry.cold_start(&mut registration, &mut cache, ColdStartPolicy::Eager)?,
        ColdStartOutcome::Ready(_)
    ));

    // 2) react: a Sync updates the warmed reserves slot
    let sources = adapter.event_sources(&registration);
    registry.register_pool(registration.clone().with_event_sources(sources))?;
    let mut runtime = ReactiveRuntime::<Ethereum>::new(ReactiveConfig::default());
    runtime.register_handler(Arc::new(AmmReactiveHandler::new(registry)))?;
    let new_r0 = U256::from(1_500_u64);
    let new_r1 = U256::from(2_500_u64);
    let log = rpc_log(pool, vec![v2_sync_topic()], abi_words([new_r0, new_r1]), 12);
    runtime.ingest_batch(&mut cache, batch(log, 12))?;
    let raw = cache
        .cached_storage_value(pool, V2_RESERVES_SLOT)
        .expect("reserves warm");
    assert_eq!(raw & low_mask(112), new_r0, "react leg updated reserve0");

    // 3) simulate against the post-event state
    let config = SimConfig::default().with_v2_router(router);
    let quote = adapter
        .simulate_swap(
            &registration,
            &mut cache,
            token0,
            token1,
            U256::from(1_000_u64),
            &config,
        )
        .map_err(|e| anyhow!("v2 sim failed: {e}"))?;
    assert_eq!(quote.amount_out, quote_out, "simulate leg returned a quote");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn v3_full_pipeline_cold_start_react_simulate() -> Result<()> {
    let pool = Address::repeat_byte(0x93);
    let quoter = Address::repeat_byte(0xb2);
    let token0 = Address::repeat_byte(0xa2);
    let token1 = Address::repeat_byte(0xa3);
    let quote_out = U256::from(9_999_u64);

    let mut cache = setup_cache().await?;
    install_default_account(&mut cache, Address::ZERO);
    install_mock_runtime(
        &mut cache,
        quoter,
        include_str!("fixtures/mock_v3_quoter_runtime.hex"),
    );
    cache
        .db_mut()
        .insert_account_storage(quoter, U256::ZERO, quote_out)?;
    // cold-start warms slot0 (non-zero -> Ready) + liquidity.
    cache.set_storage_batch_fetcher(stub_fetcher(HashMap::from([
        ((pool, V3_SLOT0_SLOT), U256::from(123_456_u64)),
        ((pool, V3_LIQUIDITY_SLOT), U256::from(67_890_u64)),
    ])));

    let adapter = ConcentratedLiquidityAdapter::default();
    let mut registry = AdapterRegistry::new();
    registry.register_adapter(Arc::new(ConcentratedLiquidityAdapter::default()))?;
    let mut registration = PoolRegistration::new(PoolKey::UniswapV3(pool))
        .with_state_address(pool)
        .with_metadata(ProtocolMetadata::UniswapV3(
            V3Metadata::default()
                .with_token0(token0)
                .with_token1(token1)
                .with_fee(500)
                .with_tick_spacing(10)
                .with_storage_layout(V3StorageLayout::uniswap(10)),
        ));

    // 1) cold-start
    assert!(matches!(
        registry.cold_start(&mut registration, &mut cache, ColdStartPolicy::Eager)?,
        ColdStartOutcome::Ready(_)
    ));

    // 2) react: a Swap updates slot0 (sqrtPrice/tick) + liquidity
    let sources = adapter.event_sources(&registration);
    registry.register_pool(registration.clone().with_event_sources(sources))?;
    let mut runtime = ReactiveRuntime::<Ethereum>::new(ReactiveConfig::default());
    runtime.register_handler(Arc::new(AmmReactiveHandler::new(registry)))?;
    let new_sqrt = U256::from(54_321_u64);
    let new_liq = U256::from(11_111_u64);
    let new_tick = U256::from(7_u64);
    let log = rpc_log(
        pool,
        vec![
            v3_swap_topic(),
            topic_address(Address::repeat_byte(0x01)),
            topic_address(Address::repeat_byte(0x02)),
        ],
        abi_words([U256::ZERO, U256::ZERO, new_sqrt, new_liq, new_tick]),
        12,
    );
    runtime.ingest_batch(&mut cache, batch(log, 12))?;
    let raw_slot0 = cache
        .cached_storage_value(pool, V3_SLOT0_SLOT)
        .expect("slot0 warm");
    assert_eq!(
        raw_slot0 & low_mask(160),
        new_sqrt,
        "react leg updated sqrtPrice"
    );
    assert_eq!(
        cache.cached_storage_value(pool, V3_LIQUIDITY_SLOT),
        Some(new_liq),
        "react leg updated liquidity"
    );

    // 3) simulate against the post-event state
    let config = SimConfig::default().with_v3_quoter(quoter);
    let quote = adapter
        .simulate_swap(
            &registration,
            &mut cache,
            token0,
            token1,
            U256::from(1_000_u64),
            &config,
        )
        .map_err(|e| anyhow!("v3 sim failed: {e}"))?;
    assert_eq!(quote.amount_out, quote_out, "simulate leg returned a quote");
    Ok(())
}
