use super::*;
use crate::adapters::{AdapterRegistry, AdapterRegistrySnapshot, AmmOwnershipIndex};
use alloy_consensus::Header as ConsensusHeader;
use alloy_primitives::U256;
use alloy_provider::{RootProvider, network::AnyNetwork};
use alloy_rpc_client::RpcClient;
use alloy_transport::mock::Asserter;
use evm_fork_cache::cache::{EvmCache, EvmOverlay};
use revm::{Database, state::AccountInfo};

const ACCOUNT: Address = Address::repeat_byte(0x11);

fn source() -> SourceIdentity {
    SourceIdentity {
        chain_id: 1,
        scope: B256::repeat_byte(2),
        package: B256::repeat_byte(3),
        model: 5,
    }
}
fn header(parent: Option<&Header>, value: u64) -> Header {
    Header::new(ConsensusHeader {
        parent_hash: parent.map_or(B256::ZERO, |p| p.hash),
        number: parent.map_or(100, |p| p.number + 1),
        timestamp: parent.map_or(1000, |p| p.timestamp + 2),
        base_fee_per_gas: Some(1),
        gas_limit: 30_000_000,
        state_root: B256::from(U256::from(value)),
        ..Default::default()
    })
}
async fn cache(header: &Header, value: u64) -> Arc<EvmSnapshot> {
    cache_with(header, value, |_| {}).await
}
async fn cache_with(
    header: &Header,
    value: u64,
    mutate: impl FnOnce(&mut EvmCache),
) -> Arc<EvmSnapshot> {
    let provider = RootProvider::<AnyNetwork>::new(RpcClient::mocked(Asserter::new()));
    let mut cache = EvmCache::builder(Arc::new(provider))
        .chain_id(1)
        .build()
        .await;
    cache.advance_block(header).unwrap();
    cache.set_block(alloy_eips::BlockId::hash(header.hash));
    cache.set_block_context(Some(header.number), header.base_fee_per_gas);
    cache.set_coinbase(Some(header.beneficiary));
    cache.set_prevrandao(Some(header.mix_hash));
    cache.set_block_gas_limit(Some(header.gas_limit));
    cache.set_timestamp(Some(header.timestamp));
    cache.db_mut().insert_account_info(
        ACCOUNT,
        AccountInfo {
            nonce: value,
            balance: U256::from(value),
            ..Default::default()
        },
    );
    cache
        .db_mut()
        .insert_account_storage(ACCOUNT, U256::ZERO, U256::from(value))
        .unwrap();
    mutate(&mut cache);
    cache.snapshot()
}
fn baseline(header: &Header, cache: Arc<EvmSnapshot>) -> AmmStateSnapshot {
    let registry =
        AdapterRegistrySnapshot::try_new(&AdapterRegistry::new(), &AmmOwnershipIndex::default())
            .unwrap();
    AmmStateSnapshot::new(
        AmmRuntimeId::allocate(),
        AmmStateVersion::initial(),
        AmmStatePoint::post_block(1, header.number, header.hash),
        1,
        cache,
        Arc::new(registry),
        Arc::new(BTreeMap::new()),
    )
}
async fn owner() -> (CanonicalStateOwner, Header) {
    let h = header(None, 1);
    let seed = baseline(&h, cache(&h, 1).await);
    let mut required = seed.cache().resident_read_set();
    required.block_numbers.clear();
    let owner =
        CanonicalStateOwner::bootstrap(&seed, source(), h.clone(), required, 4, 16, 90).unwrap();
    assert_ne!(owner.current().snapshot().runtime_id(), seed.runtime_id());
    (owner, h)
}
fn delivery(header: Header, state: Arc<EvmSnapshot>) -> StateDelivery {
    StateDelivery {
        source: source(),
        header,
        state,
        irreversible: 90,
    }
}

#[tokio::test]
async fn complete_states_advance_and_rewind_without_event_derivation() {
    let (mut owner, seed) = owner().await;
    let first = owner.current().clone();
    let a = header(Some(&seed), 2);
    let snapshot = cache(&a, 2).await;
    let commit = owner.advance(delivery(a.clone(), snapshot)).unwrap();
    assert_eq!(commit.snapshot().version().get(), 1);
    assert_eq!(
        commit.snapshot().cache().storage_value(ACCOUNT, U256::ZERO),
        Some(U256::from(2))
    );
    let mut overlay = EvmOverlay::new(commit.snapshot().cache_snapshot(), None);
    assert_eq!(overlay.basic(ACCOUNT).unwrap().unwrap().nonce, 2);
    assert_eq!(
        first.snapshot().cache().storage_value(ACCOUNT, U256::ZERO),
        Some(U256::from(1))
    );
    assert!(commit.header().logs().is_none());
    let rewind = owner.rewind(seed.number, seed.hash).unwrap();
    assert_eq!(rewind.snapshot().version().get(), 2);
    assert_eq!(
        rewind.snapshot().cache().storage_value(ACCOUNT, U256::ZERO),
        Some(U256::from(1))
    );
    let b = header(Some(&seed), 3);
    let snapshot = cache(&b, 3).await;
    assert_eq!(
        owner
            .advance(delivery(b, snapshot))
            .unwrap()
            .snapshot()
            .version()
            .get(),
        3
    );
    assert_eq!(
        owner.rewind(a.number, a.hash).err(),
        Some(StateSourceError::UnknownUndo)
    );
}

#[tokio::test]
async fn rejection_is_atomic_for_hash_parent_context_identity_and_finality() {
    let (mut owner, seed) = owner().await;
    let child = header(Some(&seed), 2);
    let state = cache(&child, 2).await;
    let initial = owner.current().snapshot().clone();
    let mut bad = delivery(child.clone(), state.clone());
    bad.header.inner.gas_used += 1;
    assert_eq!(owner.advance(bad).err(), Some(StateSourceError::Header));
    let mut bad = delivery(child.clone(), state.clone());
    bad.source.package = B256::ZERO;
    assert_eq!(owner.advance(bad).err(), Some(StateSourceError::Identity));
    let mut bad = delivery(child.clone(), state.clone());
    bad.header.inner.parent_hash = B256::ZERO;
    assert_eq!(owner.advance(bad).err(), Some(StateSourceError::Parent));
    let bad = delivery(child.clone(), initial.cache_snapshot());
    assert_eq!(owner.advance(bad).err(), Some(StateSourceError::Header));
    let mut bad = delivery(child, state);
    bad.irreversible = 89;
    assert_eq!(owner.advance(bad).err(), Some(StateSourceError::Finality));
    assert!(Arc::ptr_eq(owner.current().snapshot(), &initial));
    assert_eq!(
        owner.rewind(89, seed.hash).err(),
        Some(StateSourceError::Finality)
    );
}

#[tokio::test]
async fn completeness_code_and_dynamic_storage_bounds_remain_enforced() {
    let (mut owner, seed) = owner().await;
    let child = header(Some(&seed), 2);
    let mut state = cache(&child, 2).await;
    owner.required.slots.insert((ACCOUNT, U256::from(99)));
    assert_eq!(
        owner.advance(delivery(child.clone(), state.clone())).err(),
        Some(StateSourceError::Incomplete)
    );
    owner.required.slots.remove(&(ACCOUNT, U256::from(99)));
    owner.code.insert(ACCOUNT, B256::repeat_byte(7));
    assert_eq!(
        owner.advance(delivery(child.clone(), state.clone())).err(),
        Some(StateSourceError::CodeChanged)
    );
    owner
        .code
        .insert(ACCOUNT, state.account_code_hash(ACCOUNT).unwrap());
    owner.max_slots = 0;
    assert_eq!(
        owner.advance(delivery(child.clone(), state.clone())).err(),
        Some(StateSourceError::Bounds)
    );
    owner.max_slots = 4;
    state = cache(&child, 3).await;
    owner.advance(delivery(child, state)).unwrap();
}

#[tokio::test]
async fn retained_history_is_bounded_and_versions_do_not_rewind() {
    let (mut owner, mut h) = owner().await;
    let first = h.clone();
    for value in 2..=25 {
        h = header(Some(&h), value);
        let state = cache(&h, value).await;
        owner.advance(delivery(h.clone(), state)).unwrap();
    }
    assert_eq!(owner.history.len(), 16);
    assert_eq!(
        owner.rewind(first.number, first.hash).err(),
        Some(StateSourceError::UnknownUndo)
    );
    let held = owner.history[3].header.header().clone();
    let version = owner.current().snapshot().version().get();
    assert_eq!(
        owner
            .rewind(held.number, held.hash)
            .unwrap()
            .snapshot()
            .version()
            .get(),
        version + 1
    );
}

#[tokio::test]
async fn unrelated_code_and_missing_storage_cannot_expand_or_shrink_coverage() {
    let (mut owner, seed) = owner().await;
    let h = header(Some(&seed), 2);
    let state = cache_with(&h, 2, |cache| {
        let code = revm::bytecode::Bytecode::new_raw(alloy_primitives::Bytes::from_static(&[
            0x60, 0x01, 0x00,
        ]));
        cache.with_blockchain_db_mut(|db| {
            db.accounts().write().insert(
                ACCOUNT,
                AccountInfo {
                    code_hash: code.hash_slow(),
                    code: Some(code),
                    ..Default::default()
                },
            );
        });
    })
    .await;
    assert_eq!(
        owner.advance(delivery(h.clone(), state)).err(),
        Some(StateSourceError::Bounds)
    );
    let state = cache_with(&h, 2, |cache| {
        cache
            .db_mut()
            .cache
            .accounts
            .get_mut(&ACCOUNT)
            .unwrap()
            .storage
            .clear();
    })
    .await;
    assert_eq!(
        owner.advance(delivery(h, state)).err(),
        Some(StateSourceError::Incomplete)
    );
    assert_eq!(owner.current().snapshot().point().block_hash(), seed.hash);
}
