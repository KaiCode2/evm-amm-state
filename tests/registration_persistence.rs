use std::sync::Arc;

use alloy_primitives::{Address, B256, Bytes, U256};
use anyhow::Result;
use evm_amm_state::adapters::{
    AdapterRegistry, AdapterRegistrySnapshot, AmmAdapter, AmmOwnershipIndex,
    AmmRegistrationArchive, AmmRegistrationPersistenceError, BalancerTokenBalance,
    BalancerV2Adapter, BalancerV2Metadata, ConcentratedLiquidityAdapter, CurveAdapter,
    CurveMetadata, CurveVariant, CustomPoolKey, EventSource, PoolKey, PoolRegistration, PoolStatus,
    ProtocolId, ProtocolMetadata, SolidlyStorageLayout, SolidlyV2Adapter, SolidlyV2Metadata,
    UniswapV2Adapter, UniswapV2Metadata, V3Metadata, V3StorageLayout,
};

const PAYLOAD_OFFSET: usize = 44;

fn temp_path(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "evm_amm_state_registration_persistence_{tag}_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp directory");
    dir.join("registrations.bin")
}

fn snapshot_for_all(
    registrations: impl IntoIterator<Item = PoolRegistration>,
) -> Result<AdapterRegistrySnapshot> {
    let mut registry = AdapterRegistry::new();
    registry.register_adapter(Arc::new(UniswapV2Adapter::default()))?;
    registry.register_adapter(Arc::new(ConcentratedLiquidityAdapter::default()))?;
    registry.register_adapter(Arc::new(SolidlyV2Adapter::default()))?;
    registry.register_adapter(Arc::new(BalancerV2Adapter::default()))?;
    registry.register_adapter(Arc::new(CurveAdapter::default()))?;
    for registration in registrations {
        registry.register_pool(registration)?;
    }
    let ownership = AmmOwnershipIndex::from_registry(&registry)?;
    Ok(AdapterRegistrySnapshot::try_new(&registry, &ownership)?)
}

fn snapshot_for(
    registrations: impl IntoIterator<Item = PoolRegistration>,
) -> Result<AdapterRegistrySnapshot> {
    let mut registry = AdapterRegistry::new();
    registry.register_adapter(Arc::new(CurveAdapter::default()))?;
    for registration in registrations {
        registry.register_pool(registration)?;
    }
    let ownership = AmmOwnershipIndex::from_registry(&registry)?;
    Ok(AdapterRegistrySnapshot::try_new(&registry, &ownership)?)
}

fn saved_curve_archive(tag: &str, chain_id: u64) -> Result<std::path::PathBuf> {
    let pool = Address::repeat_byte(0xd1);
    let snapshot = snapshot_for([PoolRegistration::new(PoolKey::Curve(pool)).with_metadata(
        ProtocolMetadata::Curve(
            CurveMetadata::default()
                .with_coins([Address::repeat_byte(0xd2), Address::repeat_byte(0xd3)])
                .with_discovered_slots([U256::from(1)]),
        ),
    )])?;
    let path = temp_path(tag);
    AmmRegistrationArchive::capture(chain_id, &snapshot)?.save(&path)?;
    Ok(path)
}

#[test]
fn every_built_in_metadata_and_read_set_shape_round_trips() -> Result<()> {
    let a = |byte| Address::repeat_byte(byte);
    let v3_layout = V3StorageLayout::new(
        U256::from(1),
        U256::from(2),
        U256::from(3),
        U256::from(4),
        60,
    );
    let solidly_layout =
        SolidlyStorageLayout::new(U256::from(5), U256::from(6), U256::from(7), U256::from(8));
    let v3_metadata = || {
        V3Metadata::default()
            .with_token0(a(0x31))
            .with_token1(a(0x32))
            .with_fee(500)
            .with_tick_spacing(60)
            .with_factory(a(0x33))
            .with_quoter(a(0x34))
            .with_storage_layout(v3_layout)
            .with_warm_word_radius(3)
    };
    let registrations = vec![
        PoolRegistration::new(PoolKey::UniswapV2(a(0x01)))
            .with_metadata(ProtocolMetadata::UniswapV2(
                UniswapV2Metadata::default()
                    .with_token0(a(0x11))
                    .with_token1(a(0x12))
                    .with_fee_bps(30),
            ))
            .with_status(PoolStatus::Ready),
        PoolRegistration::new(PoolKey::UniswapV3(a(0x02)))
            .with_metadata(ProtocolMetadata::UniswapV3(v3_metadata()))
            .with_status(PoolStatus::Cold),
        PoolRegistration::new(PoolKey::PancakeV3(a(0x03)))
            .with_metadata(ProtocolMetadata::PancakeV3(v3_metadata()))
            .with_status(PoolStatus::Degraded),
        PoolRegistration::new(PoolKey::Slipstream(a(0x04)))
            .with_metadata(ProtocolMetadata::Slipstream(v3_metadata())),
        PoolRegistration::new(PoolKey::SolidlyV2(a(0x05))).with_metadata(
            ProtocolMetadata::SolidlyV2(
                SolidlyV2Metadata::default()
                    .with_token0(a(0x41))
                    .with_token1(a(0x42))
                    .with_stable(true)
                    .with_storage_layout(solidly_layout),
            ),
        ),
        PoolRegistration::new(PoolKey::BalancerV2(B256::repeat_byte(0x06))).with_metadata(
            ProtocolMetadata::BalancerV2(
                BalancerV2Metadata::default()
                    .with_vault(a(0x51))
                    .with_pool_address(a(0x52))
                    .with_tokens([a(0x53), a(0x54)])
                    .with_balance_slots([U256::from(13), U256::from(12)])
                    .with_token_cash([
                        BalancerTokenBalance::new(a(0x53), U256::from(12), false),
                        BalancerTokenBalance::new(a(0x54), U256::from(12), true),
                    ]),
            ),
        ),
        PoolRegistration::new(PoolKey::Curve(a(0x07))).with_metadata(ProtocolMetadata::Curve(
            CurveMetadata::default()
                .with_coins([a(0x61), a(0x62)])
                .with_discovered_slots([U256::from(15), U256::from(14)])
                .with_variant(CurveVariant::CryptoSwapNG)
                .with_code_seed(Bytes::from(vec![0x60, 0x00])),
        )),
    ];
    let snapshot = snapshot_for_all(registrations)?;
    let path = temp_path("all_built_ins");

    AmmRegistrationArchive::capture(8453, &snapshot)?.save(&path)?;
    let loaded = AmmRegistrationArchive::load(&path, 8453)?;

    assert_eq!(loaded.registrations().len(), 7);
    assert!(
        loaded
            .registrations()
            .iter()
            .all(|registration| registration.status == PoolStatus::Pending)
    );
    let balancer = loaded
        .registrations()
        .iter()
        .find(|registration| matches!(registration.key, PoolKey::BalancerV2(_)))
        .expect("Balancer registration");
    match &balancer.metadata {
        ProtocolMetadata::BalancerV2(metadata) => {
            assert_eq!(metadata.balance_slots, vec![U256::from(12), U256::from(13)]);
            assert_eq!(metadata.token_cash.len(), 2);
        }
        other => panic!("expected Balancer metadata, got {other:?}"),
    }
    let curve = loaded
        .registrations()
        .iter()
        .find(|registration| matches!(registration.key, PoolKey::Curve(_)))
        .expect("Curve registration");
    match &curve.metadata {
        ProtocolMetadata::Curve(metadata) => {
            assert_eq!(
                metadata.discovered_slots,
                vec![U256::from(14), U256::from(15)]
            );
            assert_eq!(
                metadata.code_seed.as_ref().map(|bytes| bytes.as_ref()),
                Some(&[0x60, 0x00][..])
            );
        }
        other => panic!("expected Curve metadata, got {other:?}"),
    }
    Ok(())
}

#[test]
fn built_in_registration_and_read_set_round_trip_as_pending() -> Result<()> {
    let pool = Address::repeat_byte(0x11);
    let coin0 = Address::repeat_byte(0x21);
    let coin1 = Address::repeat_byte(0x22);
    let registration = PoolRegistration::new(PoolKey::Curve(pool))
        .with_state_address(pool)
        .with_event_source(EventSource::direct(pool, vec![]))
        .with_metadata(ProtocolMetadata::Curve(
            CurveMetadata::default()
                .with_coins([coin0, coin1])
                .with_discovered_slots([U256::from(9), U256::from(3), U256::from(9)])
                .with_variant(CurveVariant::CryptoSwap),
        ))
        .with_status(PoolStatus::Ready);
    let snapshot = snapshot_for([registration])?;
    let path = temp_path("round_trip");

    AmmRegistrationArchive::capture(1, &snapshot)?.save(&path)?;
    let loaded = AmmRegistrationArchive::load(&path, 1)?;

    assert_eq!(loaded.chain_id(), 1);
    assert_eq!(loaded.registrations().len(), 1);
    let restored = &loaded.registrations()[0];
    assert_eq!(restored.key, PoolKey::Curve(pool));
    assert_eq!(restored.status, PoolStatus::Pending);
    assert_eq!(restored.state_addresses, vec![pool]);
    match &restored.metadata {
        ProtocolMetadata::Curve(metadata) => {
            assert_eq!(metadata.coins, vec![coin0, coin1]);
            assert_eq!(
                metadata.discovered_slots,
                vec![U256::from(3), U256::from(9)]
            );
            assert_eq!(metadata.variant, CurveVariant::CryptoSwap);
        }
        other => panic!("expected Curve metadata, got {other:?}"),
    }
    Ok(())
}

#[test]
fn equivalent_snapshots_save_identical_bytes() -> Result<()> {
    let registration = |byte, slots: Vec<U256>| {
        let pool = Address::repeat_byte(byte);
        PoolRegistration::new(PoolKey::Curve(pool))
            .with_state_addresses([pool, pool])
            .with_event_source(EventSource::direct(
                pool,
                vec![
                    B256::repeat_byte(2),
                    B256::repeat_byte(1),
                    B256::repeat_byte(2),
                ],
            ))
            .with_metadata(ProtocolMetadata::Curve(
                CurveMetadata::default()
                    .with_coins([Address::repeat_byte(0xa1), Address::repeat_byte(0xa2)])
                    .with_discovered_slots(slots),
            ))
    };
    let first = snapshot_for([
        registration(0x72, vec![U256::from(8), U256::from(7)]),
        registration(0x71, vec![U256::from(6), U256::from(5)]),
    ])?;
    let second = snapshot_for([
        registration(0x71, vec![U256::from(5), U256::from(6), U256::from(5)]),
        registration(0x72, vec![U256::from(7), U256::from(8), U256::from(8)]),
    ])?;
    let first_path = temp_path("deterministic_first");
    let second_path = temp_path("deterministic_second");

    AmmRegistrationArchive::capture(10, &first)?.save(&first_path)?;
    AmmRegistrationArchive::capture(10, &second)?.save(&second_path)?;

    assert_eq!(std::fs::read(first_path)?, std::fs::read(second_path)?);
    Ok(())
}

#[test]
fn custom_registration_is_rejected_explicitly() -> Result<()> {
    const CUSTOM_PROTOCOL: &str = "persistence-test";
    struct CustomAdapter;
    impl AmmAdapter for CustomAdapter {
        fn protocol(&self) -> ProtocolId {
            ProtocolId::Custom(CUSTOM_PROTOCOL)
        }
    }

    let key = PoolKey::Custom(CustomPoolKey::Address {
        protocol: CUSTOM_PROTOCOL,
        address: Address::repeat_byte(0xc1),
    });
    let mut registry = AdapterRegistry::new();
    registry.register_adapter(Arc::new(CustomAdapter))?;
    registry.register_pool(
        PoolRegistration::new(key.clone())
            .with_metadata(ProtocolMetadata::Custom(Arc::new("opaque"))),
    )?;
    let ownership = AmmOwnershipIndex::from_registry(&registry)?;
    let snapshot = AdapterRegistrySnapshot::try_new(&registry, &ownership)?;

    let error = AmmRegistrationArchive::capture(1, &snapshot).unwrap_err();

    assert!(matches!(
        error,
        AmmRegistrationPersistenceError::UnsupportedCustom(pool) if pool == key
    ));
    Ok(())
}

#[test]
fn load_rejects_an_archive_from_another_chain() -> Result<()> {
    let path = saved_curve_archive("chain_mismatch", 1)?;

    let error = AmmRegistrationArchive::load(&path, 10).unwrap_err();

    assert!(matches!(
        error,
        AmmRegistrationPersistenceError::ChainMismatch {
            expected: 10,
            actual: 1
        }
    ));
    Ok(())
}

#[test]
fn load_distinguishes_incompatible_version_from_corruption() -> Result<()> {
    let magic_path = saved_curve_archive("magic_mismatch", 1)?;
    let mut magic_bytes = std::fs::read(&magic_path)?;
    magic_bytes[0] ^= 0xff;
    std::fs::write(&magic_path, magic_bytes)?;
    let magic_error = AmmRegistrationArchive::load(&magic_path, 1).unwrap_err();
    assert!(matches!(
        magic_error,
        AmmRegistrationPersistenceError::InvalidMagic
    ));

    let version_path = saved_curve_archive("version_mismatch", 1)?;
    let mut version_bytes = std::fs::read(&version_path)?;
    version_bytes[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
    std::fs::write(&version_path, version_bytes)?;

    let version_error = AmmRegistrationArchive::load(&version_path, 1).unwrap_err();
    assert!(matches!(
        version_error,
        AmmRegistrationPersistenceError::IncompatibleVersion {
            expected: 1,
            actual: u32::MAX
        }
    ));

    let corrupt_path = saved_curve_archive("corrupt_payload", 1)?;
    let mut corrupt_bytes = std::fs::read(&corrupt_path)?;
    corrupt_bytes.truncate(PAYLOAD_OFFSET + 1);
    std::fs::write(&corrupt_path, corrupt_bytes)?;

    let corrupt_error = AmmRegistrationArchive::load(&corrupt_path, 1).unwrap_err();
    assert!(matches!(
        corrupt_error,
        AmmRegistrationPersistenceError::ChecksumMismatch
    ));
    Ok(())
}

#[test]
fn checksum_rejects_a_valid_json_hint_mutation_before_decode() -> Result<()> {
    let path = saved_curve_archive("checksum_mutation", 1)?;
    let bytes = std::fs::read(&path)?;
    let mut payload: serde_json::Value = serde_json::from_slice(&bytes[PAYLOAD_OFFSET..])?;
    payload["chain_id"] = serde_json::Value::from(2);
    let mut mutated = bytes[..PAYLOAD_OFFSET].to_vec();
    mutated.extend(serde_json::to_vec(&payload)?);
    std::fs::write(&path, mutated)?;

    let error = AmmRegistrationArchive::load(&path, 1).unwrap_err();

    assert!(matches!(
        error,
        AmmRegistrationPersistenceError::ChecksumMismatch
    ));
    Ok(())
}

#[test]
fn load_rejects_duplicate_pool_records() -> Result<()> {
    let path = saved_curve_archive("duplicate_pool", 1)?;
    let bytes = std::fs::read(&path)?;
    let mut payload: serde_json::Value = serde_json::from_slice(&bytes[PAYLOAD_OFFSET..])?;
    let registrations = payload["registrations"]
        .as_array_mut()
        .expect("registrations array");
    registrations.push(registrations[0].clone());
    let payload = serde_json::to_vec(&payload)?;
    let mut forged = bytes[..12].to_vec();
    forged.extend(alloy_primitives::keccak256(&payload).as_slice());
    forged.extend(payload);
    std::fs::write(&path, forged)?;

    let error = AmmRegistrationArchive::load(&path, 1).unwrap_err();

    assert!(matches!(
        error,
        AmmRegistrationPersistenceError::DuplicatePool(PoolKey::Curve(_))
    ));
    Ok(())
}

#[test]
fn load_rejects_files_beyond_the_decode_limit_before_parsing() -> Result<()> {
    let path = temp_path("file_limit");
    let file = std::fs::File::create(&path)?;
    file.set_len(64 * 1024 * 1024 + 1)?;

    let error = AmmRegistrationArchive::load(&path, 1).unwrap_err();

    assert!(matches!(
        error,
        AmmRegistrationPersistenceError::LimitExceeded {
            field: "file bytes",
            limit: 67_108_864,
            actual: 67_108_865
        }
    ));
    Ok(())
}

#[cfg(unix)]
#[test]
fn failed_pre_rename_save_preserves_the_previous_valid_archive() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let path = saved_curve_archive("atomic_preservation", 1)?;
    let original = std::fs::read(&path)?;
    let replacement_pool = Address::repeat_byte(0xe1);
    let replacement = snapshot_for([PoolRegistration::new(PoolKey::Curve(replacement_pool))
        .with_metadata(ProtocolMetadata::Curve(
            CurveMetadata::default()
                .with_coins([Address::repeat_byte(0xe2), Address::repeat_byte(0xe3)]),
        ))])?;
    let parent = path.parent().expect("archive parent");
    let original_permissions = std::fs::metadata(parent)?.permissions();
    std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o555))?;

    let save_result = AmmRegistrationArchive::capture(1, &replacement)?.save(&path);

    std::fs::set_permissions(parent, original_permissions)?;
    assert!(matches!(
        save_result,
        Err(AmmRegistrationPersistenceError::Io { .. })
    ));
    assert_eq!(std::fs::read(&path)?, original);
    assert_eq!(
        AmmRegistrationArchive::load(&path, 1)?.registrations()[0].key,
        PoolKey::Curve(Address::repeat_byte(0xd1))
    );
    Ok(())
}

#[test]
fn oversized_capture_is_rejected_before_replacing_an_old_archive() -> Result<()> {
    let path = saved_curve_archive("oversized_save", 1)?;
    let original = std::fs::read(&path)?;
    let pool = Address::repeat_byte(0xf1);
    let oversized = snapshot_for([PoolRegistration::new(PoolKey::Curve(pool)).with_metadata(
        ProtocolMetadata::Curve(
            CurveMetadata::default()
                .with_coins([Address::repeat_byte(0xf2), Address::repeat_byte(0xf3)])
                .with_code_seed(Bytes::from(vec![0; 1024 * 1024 + 1])),
        ),
    )])?;

    let result =
        AmmRegistrationArchive::capture(1, &oversized).and_then(|archive| archive.save(&path));

    assert!(matches!(
        result,
        Err(AmmRegistrationPersistenceError::LimitExceeded {
            field: "Curve code seed bytes",
            limit: 1_048_576,
            actual: 1_048_577
        })
    ));
    assert_eq!(std::fs::read(&path)?, original);
    assert!(AmmRegistrationArchive::load(&path, 1).is_ok());
    Ok(())
}
