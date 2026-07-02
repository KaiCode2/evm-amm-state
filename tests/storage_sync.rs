use std::sync::Arc;

use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_provider::RootProvider;
use alloy_provider::network::AnyNetwork;
use alloy_rpc_client::RpcClient;
use alloy_transport::mock::Asserter;
use anyhow::{Result, anyhow};
use evm_amm_state::adapters::storage::{
    SolidlyStorageLayout, V2_RESERVES_SLOT, V2_TOKEN0_SLOT, V2_TOKEN1_SLOT,
};
use evm_amm_state::adapters::storage_sync::{
    CALLDATA_SLOT_LOADER_CODE, StorageSyncEncoding, StorageSyncError, StorageSyncSpec,
    build_calldata_slot_loader_program, build_slot_loader_program, decode_storage_sync,
    slot_loader_calldata, storage_sync_spec_for_pool,
};
use evm_amm_state::adapters::{
    BalancerV2Metadata, CurveMetadata, PoolKey, PoolRegistration, ProtocolId, ProtocolMetadata,
    SolidlyV2Metadata,
};
use evm_fork_cache::cache::EvmCache;
use revm::context::result::ExecutionResult;
use revm::state::{AccountInfo, Bytecode};

const CALLER: Address = Address::ZERO;
const POOL: Address = Address::repeat_byte(0x33);

async fn mock_cache() -> EvmCache {
    let asserter = Asserter::new();
    let client = RpcClient::mocked(asserter);
    let provider = RootProvider::<AnyNetwork>::new(client);
    let mut cache = EvmCache::new(Arc::new(provider)).await;
    cache
        .db_mut()
        .insert_account_info(CALLER, AccountInfo::default());
    cache
}

fn install(cache: &mut EvmCache, addr: Address, code: Bytes, slots: &[(U256, U256)]) {
    let bytecode = Bytecode::new_raw(code);
    let code_hash = bytecode.hash_slow();
    cache.db_mut().insert_account_info(
        addr,
        AccountInfo {
            balance: U256::ZERO,
            nonce: 0,
            code: Some(bytecode),
            code_hash,
            account_id: None,
        },
    );
    cache
        .db_mut()
        .replace_account_storage(addr, Default::default())
        .expect("mark storage local");
    for (slot, value) in slots {
        cache
            .db_mut()
            .insert_account_storage(addr, *slot, *value)
            .expect("seed slot");
    }
}

fn run(cache: &mut EvmCache, to: Address, calldata: Bytes) -> Result<Bytes> {
    match cache.call_raw(CALLER, to, calldata, false)? {
        ExecutionResult::Success { output, .. } => Ok(output.into_data()),
        other => Err(anyhow!("storage sync program did not succeed: {other:?}")),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn baked_slot_loader_reads_requested_slots_in_order() -> Result<()> {
    let slots = [U256::ZERO, U256::from(8), U256::from(999)];
    let mut cache = mock_cache().await;
    install(
        &mut cache,
        POOL,
        build_slot_loader_program(&slots),
        &[
            (U256::ZERO, U256::from(11)),
            (U256::from(8), U256::from(88)),
        ],
    );

    let output = run(&mut cache, POOL, Bytes::new())?;
    let spec = StorageSyncSpec::new(POOL, slots);
    let snapshot = decode_storage_sync(&spec, &output)?;

    assert_eq!(
        snapshot.entries,
        vec![
            (U256::ZERO, U256::from(11)),
            (U256::from(8), U256::from(88)),
            (U256::from(999), U256::ZERO),
        ]
    );

    let mut warmed = mock_cache().await;
    assert_eq!(snapshot.inject(&mut warmed), 3);
    assert_eq!(
        warmed.cached_storage_value(POOL, U256::from(8)),
        Some(U256::from(88))
    );
    assert_eq!(
        warmed.cached_storage_value(POOL, U256::from(999)),
        Some(U256::ZERO)
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn calldata_slot_loader_reads_discovered_slot_lists() -> Result<()> {
    assert!(!CALLDATA_SLOT_LOADER_CODE.contains(&0x5f));

    let slots = vec![U256::from(3), U256::from(7), U256::from(10)];
    let mut cache = mock_cache().await;
    install(
        &mut cache,
        POOL,
        build_calldata_slot_loader_program(),
        &[
            (U256::from(3), U256::from(30)),
            (U256::from(10), U256::from(100)),
        ],
    );

    let output = run(&mut cache, POOL, slot_loader_calldata(&slots))?;
    let spec = StorageSyncSpec::new(POOL, slots).with_encoding(StorageSyncEncoding::CalldataSlots);
    let snapshot = decode_storage_sync(&spec, &output)?;

    assert_eq!(
        snapshot.entries,
        vec![
            (U256::from(3), U256::from(30)),
            (U256::from(7), U256::ZERO),
            (U256::from(10), U256::from(100)),
        ]
    );
    Ok(())
}

#[test]
fn pool_storage_sync_specs_cover_supported_flat_slot_protocols() {
    let v2 = PoolRegistration::new(PoolKey::UniswapV2(POOL));
    let spec = storage_sync_spec_for_pool(&v2).expect("v2 spec");
    assert_eq!(spec.target, POOL);
    assert_eq!(
        spec.slots,
        vec![V2_TOKEN0_SLOT, V2_TOKEN1_SLOT, V2_RESERVES_SLOT]
    );
    assert_eq!(spec.encoding, StorageSyncEncoding::BakedSlots);

    let solidly_layout =
        SolidlyStorageLayout::new(U256::from(20), U256::from(21), U256::from(6), U256::from(7));
    let solidly =
        PoolRegistration::new(PoolKey::SolidlyV2(POOL)).with_metadata(ProtocolMetadata::SolidlyV2(
            SolidlyV2Metadata::default().with_storage_layout(solidly_layout),
        ));
    let spec = storage_sync_spec_for_pool(&solidly).expect("solidly spec");
    assert_eq!(
        spec.slots,
        vec![
            solidly_layout.reserve0_slot,
            solidly_layout.reserve1_slot,
            solidly_layout.token0_slot,
            solidly_layout.token1_slot,
        ]
    );
    assert_eq!(spec.encoding, StorageSyncEncoding::BakedSlots);

    let vault = Address::repeat_byte(0x44);
    let balancer = PoolRegistration::new(PoolKey::BalancerV2(B256::repeat_byte(0x77)))
        .with_metadata(ProtocolMetadata::BalancerV2(
            BalancerV2Metadata::default()
                .with_vault(vault)
                .with_balance_slots([U256::from(9), U256::from(3), U256::from(9)]),
        ));
    let spec = storage_sync_spec_for_pool(&balancer).expect("balancer spec");
    assert_eq!(spec.target, vault);
    assert_eq!(spec.slots, vec![U256::from(3), U256::from(9)]);
    assert_eq!(spec.encoding, StorageSyncEncoding::CalldataSlots);

    let curve = PoolRegistration::new(PoolKey::Curve(POOL)).with_metadata(ProtocolMetadata::Curve(
        CurveMetadata::default().with_discovered_slots([
            U256::from(5),
            U256::from(2),
            U256::from(5),
        ]),
    ));
    let spec = storage_sync_spec_for_pool(&curve).expect("curve spec");
    assert_eq!(spec.target, POOL);
    assert_eq!(spec.slots, vec![U256::from(2), U256::from(5)]);
    assert_eq!(spec.encoding, StorageSyncEncoding::CalldataSlots);
}

#[test]
fn pool_storage_sync_spec_rejects_unavailable_layouts() {
    let v3 = PoolRegistration::new(PoolKey::UniswapV3(POOL));
    assert_eq!(
        storage_sync_spec_for_pool(&v3),
        Err(StorageSyncError::UnsupportedProtocol(ProtocolId::UniswapV3))
    );

    let balancer = PoolRegistration::new(PoolKey::BalancerV2(B256::repeat_byte(0x77)))
        .with_metadata(ProtocolMetadata::BalancerV2(
            BalancerV2Metadata::default().with_vault(Address::repeat_byte(0x44)),
        ));
    assert_eq!(
        storage_sync_spec_for_pool(&balancer),
        Err(StorageSyncError::EmptyReadSet("Balancer V2 vault balance"))
    );

    let duplicate_layout =
        SolidlyStorageLayout::new(U256::from(20), U256::from(20), U256::from(6), U256::from(7));
    let solidly =
        PoolRegistration::new(PoolKey::SolidlyV2(POOL)).with_metadata(ProtocolMetadata::SolidlyV2(
            SolidlyV2Metadata::default().with_storage_layout(duplicate_layout),
        ));
    assert_eq!(
        storage_sync_spec_for_pool(&solidly),
        Err(StorageSyncError::InvalidLayout("Solidly V2 storage"))
    );
}

#[test]
fn decode_rejects_malformed_output() {
    let spec = StorageSyncSpec::new(POOL, [U256::from(1), U256::from(2)]);
    assert_eq!(
        decode_storage_sync(&spec, &[0u8; 32]),
        Err(StorageSyncError::Malformed(
            "output has 32 bytes, expected 64".to_string()
        ))
    );
}
