#![cfg(feature = "uniswap-v2")]

//! Offline acceptance tests for representative AMM quote read-set warming.

use std::sync::Arc;

use alloy_primitives::{Address, Bytes, U256, hex};
use alloy_provider::{RootProvider, network::AnyNetwork};
use alloy_rpc_client::RpcClient;
use alloy_transport::mock::Asserter;
use anyhow::Result;
use evm_amm_state::adapters::{
    AdapterRegistry, PoolKey, PoolRegistration, ProtocolMetadata, QuoteReadSetLimits, QuoteWarmup,
    QuoteWarmupError, SimConfig, UniswapV2Adapter, UniswapV2Metadata,
};
use evm_fork_cache::AccountProof;
use evm_fork_cache::cache::EvmCache;
use revm::state::{AccountInfo, Bytecode};

async fn setup_cache() -> EvmCache {
    let client = RpcClient::mocked(Asserter::new());
    let provider = RootProvider::<AnyNetwork>::new(client);
    EvmCache::new(Arc::new(provider)).await
}

fn install_runtime(cache: &mut EvmCache, address: Address, runtime: &str) {
    let code = Bytecode::new_raw(Bytes::from(
        hex::decode(runtime.trim()).expect("runtime hex"),
    ));
    let code_hash = code.hash_slow();
    cache.db_mut().insert_account_info(
        address,
        AccountInfo {
            balance: U256::ZERO,
            nonce: 1,
            code: Some(code),
            code_hash,
            account_id: None,
        },
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn representative_quote_is_recorded_and_replays_without_a_provider() -> Result<()> {
    let pool = Address::repeat_byte(0x11);
    let router = Address::repeat_byte(0x12);
    let token0 = Address::repeat_byte(0x13);
    let token1 = Address::repeat_byte(0x14);
    let amount_out = U256::from(4_242_u64);

    let mut cache = setup_cache().await;
    cache
        .db_mut()
        .insert_account_info(Address::ZERO, AccountInfo::default());
    install_runtime(
        &mut cache,
        router,
        include_str!("fixtures/mock_v2_router_runtime.hex"),
    );
    cache
        .db_mut()
        .insert_account_storage(router, U256::ZERO, amount_out)?;

    let key = PoolKey::UniswapV2(pool);
    let mut registry =
        AdapterRegistry::new().with_sim_config(SimConfig::default().with_v2_router(router));
    registry.register_adapter(Arc::new(UniswapV2Adapter::default()))?;
    registry.register_pool(
        PoolRegistration::new(key.clone()).with_metadata(ProtocolMetadata::UniswapV2(
            UniswapV2Metadata::default()
                .with_token0(token0)
                .with_token1(token1)
                .with_fee_bps(30),
        )),
    )?;
    let warmup = QuoteWarmup::exact_input(key.clone(), token0, token1, U256::from(1_000_u64));

    let report = registry.warm_quote_read_sets(&mut cache, [warmup.clone()])?;

    assert_eq!(report.entries().len(), 1);
    assert_eq!(report.entries()[0].quote().amount_out, amount_out);
    assert!(
        report.entries()[0].provider_reads().accounts.is_empty()
            && report.entries()[0].provider_reads().code_hashes.is_empty()
            && report.entries()[0].provider_reads().slots.is_empty(),
        "the fully seeded fixture must not require provider reads"
    );
    assert_eq!(report.entries()[0].accesses().code_hashes.len(), 1);
    assert!(
        registry
            .quote_read_set(&warmup)
            .expect("stored read set")
            .slots
            .contains(&(router, U256::ZERO))
    );
    registry.validate_quote_read_sets(cache.snapshot(), [&key])?;

    cache.purge_contract_slots(router, &[U256::ZERO]);
    let error = registry
        .validate_quote_read_sets(cache.snapshot(), [&key])
        .expect_err("a learned quote slot was removed");
    assert!(matches!(
        error,
        QuoteWarmupError::IncompleteSnapshot { missing, .. }
            if missing.storage.contains(&(router, U256::ZERO))
    ));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn learned_quotes_refresh_at_the_exact_cache_pin_and_revalidate_offline() -> Result<()> {
    let pool = Address::repeat_byte(0x21);
    let router = Address::repeat_byte(0x22);
    let token0 = Address::repeat_byte(0x23);
    let token1 = Address::repeat_byte(0x24);
    let mut cache = setup_cache().await;
    cache
        .db_mut()
        .insert_account_info(Address::ZERO, AccountInfo::default());
    install_runtime(
        &mut cache,
        router,
        include_str!("fixtures/mock_v2_router_runtime.hex"),
    );
    cache
        .db_mut()
        .insert_account_storage(router, U256::ZERO, U256::from(1))?;

    let key = PoolKey::UniswapV2(pool);
    let warmup = QuoteWarmup::exact_input(key.clone(), token0, token1, U256::from(1_000_u64));
    let mut registry =
        AdapterRegistry::new().with_sim_config(SimConfig::default().with_v2_router(router));
    registry.register_adapter(Arc::new(UniswapV2Adapter::default()))?;
    registry.register_pool(
        PoolRegistration::new(key.clone()).with_metadata(ProtocolMetadata::UniswapV2(
            UniswapV2Metadata::default()
                .with_token0(token0)
                .with_token1(token1)
                .with_fee_bps(30),
        )),
    )?;
    registry.warm_quote_read_sets(&mut cache, [warmup])?;

    let router_code_hash = Bytecode::new_raw(Bytes::from(hex::decode(
        include_str!("fixtures/mock_v2_router_runtime.hex").trim(),
    )?))
    .hash_slow();
    cache.set_account_proof_fetcher(Arc::new(move |requests, _block| {
        requests
            .into_iter()
            .map(|(address, slots)| {
                let code_hash = if address == router {
                    router_code_hash
                } else {
                    revm::primitives::KECCAK_EMPTY
                };
                let values = slots
                    .into_iter()
                    .map(|slot| (slot, U256::from(9_999)))
                    .collect();
                (
                    address,
                    Ok(AccountProof {
                        storage_hash: alloy_primitives::B256::repeat_byte(0x25),
                        balance: U256::ZERO,
                        nonce: u64::from(address == router),
                        code_hash,
                        slots: values,
                    }),
                )
            })
            .collect()
    }));

    let report = registry.hydrate_quote_read_sets(&mut cache);

    assert!(report.is_complete(), "{report:?}");
    assert_eq!(report.warmups, 1);
    assert_eq!(
        cache.cached_storage_value(router, U256::ZERO),
        Some(U256::from(9_999))
    );
    registry.validate_quote_read_sets(cache.snapshot(), [&key])?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn representative_quote_manifest_growth_is_bounded() -> Result<()> {
    let pool = Address::repeat_byte(0x31);
    let router = Address::repeat_byte(0x32);
    let token0 = Address::repeat_byte(0x33);
    let token1 = Address::repeat_byte(0x34);
    let mut cache = setup_cache().await;
    cache
        .db_mut()
        .insert_account_info(Address::ZERO, AccountInfo::default());
    install_runtime(
        &mut cache,
        router,
        include_str!("fixtures/mock_v2_router_runtime.hex"),
    );
    cache
        .db_mut()
        .insert_account_storage(router, U256::ZERO, U256::from(1))?;
    let key = PoolKey::UniswapV2(pool);
    let mut registry = AdapterRegistry::new()
        .with_sim_config(SimConfig::default().with_v2_router(router))
        .with_quote_read_set_limits(QuoteReadSetLimits {
            max_slots: 0,
            ..Default::default()
        });
    registry.register_adapter(Arc::new(UniswapV2Adapter::default()))?;
    registry.register_pool(
        PoolRegistration::new(key.clone()).with_metadata(ProtocolMetadata::UniswapV2(
            UniswapV2Metadata::default()
                .with_token0(token0)
                .with_token1(token1)
                .with_fee_bps(30),
        )),
    )?;

    let error = registry
        .warm_quote_read_sets(
            &mut cache,
            [QuoteWarmup::exact_input(
                key,
                token0,
                token1,
                U256::from(1_000),
            )],
        )
        .expect_err("the captured router slot exceeds the zero-slot bound");

    assert!(matches!(
        error,
        QuoteWarmupError::ReadSetLimit { slots: 1, .. }
    ));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn warming_enables_fail_closed_readiness_for_every_affected_pool() -> Result<()> {
    let router = Address::repeat_byte(0x42);
    let token0 = Address::repeat_byte(0x43);
    let token1 = Address::repeat_byte(0x44);
    let warmed = PoolKey::UniswapV2(Address::repeat_byte(0x45));
    let unwarmed = PoolKey::UniswapV2(Address::repeat_byte(0x46));
    let mut cache = setup_cache().await;
    cache
        .db_mut()
        .insert_account_info(Address::ZERO, AccountInfo::default());
    install_runtime(
        &mut cache,
        router,
        include_str!("fixtures/mock_v2_router_runtime.hex"),
    );
    cache
        .db_mut()
        .insert_account_storage(router, U256::ZERO, U256::from(1))?;
    let metadata = ProtocolMetadata::UniswapV2(
        UniswapV2Metadata::default()
            .with_token0(token0)
            .with_token1(token1)
            .with_fee_bps(30),
    );
    let mut registry =
        AdapterRegistry::new().with_sim_config(SimConfig::default().with_v2_router(router));
    registry.register_adapter(Arc::new(UniswapV2Adapter::default()))?;
    registry
        .register_pool(PoolRegistration::new(warmed.clone()).with_metadata(metadata.clone()))?;
    registry.register_pool(PoolRegistration::new(unwarmed.clone()).with_metadata(metadata))?;
    registry.warm_quote_read_sets(
        &mut cache,
        [QuoteWarmup::exact_input(
            warmed,
            token0,
            token1,
            U256::from(1_000_u64),
        )],
    )?;

    let error = registry
        .validate_quote_read_sets(cache.snapshot(), [&unwarmed])
        .expect_err("an affected pool without a representative quote must fail closed");
    assert!(matches!(
        error,
        QuoteWarmupError::MissingReadSet(pool) if pool == unwarmed
    ));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn code_identity_change_invalidates_every_dependent_quote_manifest() -> Result<()> {
    let pool = Address::repeat_byte(0x51);
    let router = Address::repeat_byte(0x52);
    let token0 = Address::repeat_byte(0x53);
    let token1 = Address::repeat_byte(0x54);
    let mut cache = setup_cache().await;
    cache
        .db_mut()
        .insert_account_info(Address::ZERO, AccountInfo::default());
    install_runtime(
        &mut cache,
        router,
        include_str!("fixtures/mock_v2_router_runtime.hex"),
    );
    cache
        .db_mut()
        .insert_account_storage(router, U256::ZERO, U256::from(1))?;
    let key = PoolKey::UniswapV2(pool);
    let warmup = QuoteWarmup::exact_input(key.clone(), token0, token1, U256::from(1_000_u64));
    let mut registry =
        AdapterRegistry::new().with_sim_config(SimConfig::default().with_v2_router(router));
    registry.register_adapter(Arc::new(UniswapV2Adapter::default()))?;
    registry.register_pool(
        PoolRegistration::new(key.clone()).with_metadata(ProtocolMetadata::UniswapV2(
            UniswapV2Metadata::default()
                .with_token0(token0)
                .with_token1(token1)
                .with_fee_bps(30),
        )),
    )?;
    registry.warm_quote_read_sets(&mut cache, [warmup.clone()])?;
    cache.set_account_proof_fetcher(Arc::new(move |requests, _block| {
        requests
            .into_iter()
            .map(|(address, slots)| {
                (
                    address,
                    Ok(AccountProof {
                        storage_hash: alloy_primitives::B256::repeat_byte(0x55),
                        balance: U256::ZERO,
                        nonce: 1,
                        code_hash: if address == router {
                            alloy_primitives::B256::repeat_byte(0x56)
                        } else {
                            revm::primitives::KECCAK_EMPTY
                        },
                        slots: slots.into_iter().map(|slot| (slot, U256::ZERO)).collect(),
                    }),
                )
            })
            .collect()
    }));

    let report = registry.hydrate_quote_read_sets(&mut cache);

    assert_eq!(report.invalidated, vec![warmup]);
    assert!(!registry.has_quote_read_set(&key));
    assert!(!report.is_complete());
    Ok(())
}
