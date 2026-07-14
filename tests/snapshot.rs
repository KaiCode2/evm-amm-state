use std::sync::Arc;

use alloy_primitives::Address;
use anyhow::Result;
use evm_amm_state::adapters::{
    AdapterGeneration, AdapterInstanceId, AdapterKey, AdapterRegistry, AdapterRegistrySnapshot,
    AdapterRegistrySnapshotError, AmmAdapter, AmmOwnershipIndex, CustomPoolKey, PoolGeneration,
    PoolInstanceId, PoolKey, PoolOwnership, PoolRegistration, PoolStateDependencies, ProtocolId,
};

#[cfg(feature = "live-runtime")]
use alloy_consensus::Header as ConsensusHeader;
#[cfg(feature = "live-runtime")]
use alloy_primitives::{Address as HeaderAddress, B256};
#[cfg(feature = "live-runtime")]
use alloy_provider::{RootProvider, network::AnyNetwork};
#[cfg(feature = "live-runtime")]
use alloy_rpc_client::RpcClient;
#[cfg(feature = "live-runtime")]
use alloy_rpc_types_eth::Header as RpcHeader;
#[cfg(feature = "live-runtime")]
use alloy_transport::mock::Asserter;
#[cfg(feature = "live-runtime")]
use evm_amm_state::adapters::{AmmRuntime, AmmRuntimeBaseline, AmmRuntimeConfig};
#[cfg(feature = "live-runtime")]
use evm_fork_cache::cache::{EvmCache, EvmOverlay};

const SNAPSHOT_PROTOCOL: &str = "snapshot-test";
const OTHER_PROTOCOL: &str = "snapshot-other";

struct SnapshotAdapter;

impl AmmAdapter for SnapshotAdapter {
    fn protocol(&self) -> ProtocolId {
        ProtocolId::Custom(SNAPSHOT_PROTOCOL)
    }
}

struct FamilySnapshotAdapter;

impl AmmAdapter for FamilySnapshotAdapter {
    fn protocol(&self) -> ProtocolId {
        ProtocolId::Custom(SNAPSHOT_PROTOCOL)
    }

    fn protocols(&self) -> Vec<ProtocolId> {
        vec![
            ProtocolId::Custom(SNAPSHOT_PROTOCOL),
            ProtocolId::Custom(OTHER_PROTOCOL),
        ]
    }
}

fn pool_key(byte: u8) -> PoolKey {
    PoolKey::Custom(CustomPoolKey::Address {
        protocol: SNAPSHOT_PROTOCOL,
        address: Address::repeat_byte(byte),
    })
}

fn other_pool_key(byte: u8) -> PoolKey {
    PoolKey::Custom(CustomPoolKey::Address {
        protocol: OTHER_PROTOCOL,
        address: Address::repeat_byte(byte),
    })
}

fn registry_with_pool(key: PoolKey) -> Result<AdapterRegistry> {
    let mut registry = AdapterRegistry::new();
    registry.register_adapter(Arc::new(SnapshotAdapter))?;
    registry.register_pool(PoolRegistration::new(key))?;
    Ok(registry)
}

fn adapter_instance() -> AdapterInstanceId {
    AdapterInstanceId::new(
        AdapterKey::new(ProtocolId::Custom(SNAPSHOT_PROTOCOL), []),
        AdapterGeneration::new(0),
    )
}

#[test]
fn checked_snapshot_resolves_only_the_active_pool_generation() -> Result<()> {
    let key = pool_key(0x11);
    let registry = registry_with_pool(key.clone())?;
    let ownership = AmmOwnershipIndex::from_registry(&registry)?;

    let snapshot = AdapterRegistrySnapshot::try_new(&registry, &ownership)?;
    let active = ownership
        .active_pool(&key)
        .cloned()
        .expect("registered pool generation");
    let stale = PoolInstanceId::new(key, PoolGeneration::new(1));

    assert_eq!(
        snapshot.pool(&active).map(|pool| &pool.key),
        Some(active.key())
    );
    assert!(snapshot.pool(&stale).is_none());
    Ok(())
}

#[test]
fn checked_snapshot_rejects_a_registry_pool_without_ownership() -> Result<()> {
    let key = pool_key(0x21);
    let registry = registry_with_pool(key.clone())?;
    let mut ownership = AmmOwnershipIndex::default();
    ownership.insert_adapter(adapter_instance())?;

    assert_eq!(
        AdapterRegistrySnapshot::try_new(&registry, &ownership)
            .err()
            .expect("pool divergence must be rejected"),
        AdapterRegistrySnapshotError::RegistryPoolMissingOwnership(key)
    );
    Ok(())
}

#[test]
fn checked_snapshot_rejects_owned_pool_missing_from_registry() -> Result<()> {
    let key = pool_key(0x31);
    let mut registry = AdapterRegistry::new();
    registry.register_adapter(Arc::new(SnapshotAdapter))?;
    let adapter = adapter_instance();
    let instance = PoolInstanceId::new(key, PoolGeneration::new(0));
    let mut ownership = AmmOwnershipIndex::default();
    ownership.insert_adapter(adapter.clone())?;
    ownership.insert_pool(PoolOwnership::new(
        instance.clone(),
        adapter,
        PoolStateDependencies::default(),
        [],
    )?)?;

    assert_eq!(
        AdapterRegistrySnapshot::try_new(&registry, &ownership)
            .err()
            .expect("pool divergence must be rejected"),
        AdapterRegistrySnapshotError::OwnershipPoolMissingRegistry(instance)
    );
    Ok(())
}

#[test]
fn checked_snapshot_rejects_registry_adapter_without_ownership() -> Result<()> {
    let mut registry = AdapterRegistry::new();
    registry.register_adapter(Arc::new(SnapshotAdapter))?;
    let ownership = AmmOwnershipIndex::default();
    let key = adapter_instance().key().clone();

    assert_eq!(
        AdapterRegistrySnapshot::try_new(&registry, &ownership)
            .err()
            .expect("adapter divergence must be rejected"),
        AdapterRegistrySnapshotError::RegistryAdapterMissingOwnership(key)
    );
    Ok(())
}

#[test]
fn checked_snapshot_rejects_owned_adapter_missing_from_registry() -> Result<()> {
    let registry = AdapterRegistry::new();
    let instance = adapter_instance();
    let mut ownership = AmmOwnershipIndex::default();
    ownership.insert_adapter(instance.clone())?;

    assert_eq!(
        AdapterRegistrySnapshot::try_new(&registry, &ownership)
            .err()
            .expect("adapter divergence must be rejected"),
        AdapterRegistrySnapshotError::OwnershipAdapterMissingRegistry(instance)
    );
    Ok(())
}

#[test]
fn checked_snapshot_rejects_registry_pool_without_an_adapter() -> Result<()> {
    let key = pool_key(0x41);
    let mut registry = AdapterRegistry::new();
    registry.register_pool(PoolRegistration::new(key.clone()))?;
    let adapter = adapter_instance();
    let instance = PoolInstanceId::new(key.clone(), PoolGeneration::new(0));
    let mut ownership = AmmOwnershipIndex::default();
    ownership.insert_adapter(adapter.clone())?;
    ownership.insert_pool(PoolOwnership::new(
        instance,
        adapter,
        PoolStateDependencies::default(),
        [],
    )?)?;

    assert_eq!(
        AdapterRegistrySnapshot::try_new(&registry, &ownership)
            .err()
            .expect("missing pool adapter must be rejected"),
        AdapterRegistrySnapshotError::RegistryPoolMissingAdapter(key)
    );
    Ok(())
}

#[test]
fn checked_snapshot_rejects_pool_owned_by_the_wrong_adapter_family() -> Result<()> {
    let key = pool_key(0x51);
    let registry = registry_with_pool(key.clone())?;
    let owned_adapter = AdapterInstanceId::new(
        AdapterKey::new(
            ProtocolId::Custom(SNAPSHOT_PROTOCOL),
            [ProtocolId::Custom(OTHER_PROTOCOL)],
        ),
        AdapterGeneration::new(0),
    );
    let instance = PoolInstanceId::new(key, PoolGeneration::new(0));
    let mut ownership = AmmOwnershipIndex::default();
    ownership.insert_adapter(owned_adapter.clone())?;
    ownership.insert_pool(PoolOwnership::new(
        instance.clone(),
        owned_adapter.clone(),
        PoolStateDependencies::default(),
        [],
    )?)?;

    assert_eq!(
        AdapterRegistrySnapshot::try_new(&registry, &ownership)
            .err()
            .expect("pool adapter divergence must be rejected"),
        AdapterRegistrySnapshotError::PoolAdapterMismatch {
            pool: Box::new(instance),
            registry: Box::new(adapter_instance().key().clone()),
            ownership: Box::new(owned_adapter),
        }
    );
    Ok(())
}

#[test]
fn checked_snapshot_deduplicates_a_multi_protocol_adapter_family() -> Result<()> {
    let first = pool_key(0x61);
    let second = other_pool_key(0x62);
    let mut registry = AdapterRegistry::new();
    registry.register_adapter(Arc::new(FamilySnapshotAdapter))?;
    registry.register_pool(PoolRegistration::new(first.clone()))?;
    registry.register_pool(PoolRegistration::new(second.clone()))?;
    let ownership = AmmOwnershipIndex::from_registry(&registry)?;

    let snapshot = AdapterRegistrySnapshot::try_new(&registry, &ownership)?;

    assert_eq!(snapshot.adapter_count(), 1);
    assert_eq!(snapshot.pool_count(), 2);
    assert!(
        snapshot
            .pool(ownership.active_pool(&first).expect("first pool"))
            .is_some()
    );
    assert!(
        snapshot
            .pool(ownership.active_pool(&second).expect("second pool"))
            .is_some()
    );
    Ok(())
}

#[cfg(feature = "live-runtime")]
async fn cache_at(header: &RpcHeader) -> EvmCache {
    let provider = RootProvider::<AnyNetwork>::new(RpcClient::mocked(Asserter::new()));
    let mut cache = EvmCache::new(Arc::new(provider)).await;
    cache
        .advance_block(header)
        .expect("test header has complete context");
    cache
}

#[cfg(feature = "live-runtime")]
fn baseline_header(block: u64) -> RpcHeader {
    RpcHeader::new(ConsensusHeader {
        number: block,
        timestamp: 1_700_000_000 + block,
        base_fee_per_gas: Some(7),
        beneficiary: HeaderAddress::repeat_byte(0xcb),
        gas_limit: 30_000_000,
        mix_hash: B256::repeat_byte(0xab),
        ..ConsensusHeader::default()
    })
}

#[cfg(feature = "live-runtime")]
#[tokio::test(flavor = "multi_thread")]
async fn published_cache_arc_builds_independent_worker_overlays() -> Result<()> {
    tokio::task::LocalSet::new()
        .run_until(async {
            let block = 500;
            let header = baseline_header(block);
            let baseline = AmmRuntimeBaseline::from_verified_header(1, header.clone())?;
            let runtime = AmmRuntime::spawn(
                cache_at(&header).await,
                AdapterRegistry::new(),
                baseline,
                AmmRuntimeConfig::default(),
            )?;
            let snapshot = runtime.latest_snapshot();
            let left_cache = snapshot.cache_snapshot();
            let right_cache = snapshot.cache_snapshot();

            let left = std::thread::spawn(move || {
                let overlay = EvmOverlay::new(left_cache, None);
                (overlay.chain_id(), overlay.block_number())
            });
            let right = std::thread::spawn(move || {
                let overlay = EvmOverlay::new(right_cache, None);
                (overlay.chain_id(), overlay.block_number())
            });

            assert_eq!(left.join().expect("left overlay worker"), (1, Some(block)));
            assert_eq!(
                right.join().expect("right overlay worker"),
                (1, Some(block))
            );
            runtime.shutdown().await?;
            Ok(())
        })
        .await
}
