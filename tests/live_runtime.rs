#![cfg(feature = "live-runtime")]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use alloy_consensus::Header as ConsensusHeader;
use alloy_json_rpc::{RequestPacket, ResponsePacket};
use alloy_network::Ethereum;
use alloy_primitives::{Address, B256, Bytes, Log as PrimitiveLog, U256, keccak256};
use alloy_provider::{ProviderBuilder, RootProvider, network::AnyNetwork};
use alloy_rpc_client::RpcClient;
use alloy_rpc_types_eth::{EIP1186AccountProofResponse, Header as RpcHeader, Log as RpcLog};
use alloy_transport::mock::{Asserter, MockTransport};
use alloy_transport::{TransportError, TransportFut};
use anyhow::Result;
use evm_amm_state::adapters::storage::{V2_RESERVES_SLOT, V2_TOKEN0_SLOT, V2_TOKEN1_SLOT};
use evm_amm_state::adapters::{
    AdapterEvent, AdapterEventError, AdapterEventKind, AdapterEventResult, AdapterRegistry,
    AmmAdapter, AmmCanonicalBatch, AmmCanonicalBatchError, AmmColdStartOptions,
    AmmColdStartWorkerConfig, AmmDiscoveryOptions, AmmEvictionPolicy,
    AmmFactoryWatcherRegistration, AmmObserverError, AmmPoolChangeKind, AmmPreparedPoolState,
    AmmPreparedStorage, AmmRuntime, AmmRuntimeBaseline, AmmRuntimeCommandError, AmmRuntimeConfig,
    AmmRuntimeEventKind, AmmRuntimeHealth, AmmRuntimeSubmitError, AmmStatePoint, AmmStateVersion,
    AmmSubscriberDriverConfig, AmmSubscriberDriverError, AmmSubscriberDriverState, ColdStartPolicy,
    CreationLogContext, CustomPoolKey, DeferredWork, DiscoveredPool, DiscoveryError,
    DiscoveryOwnerKey, EventSource, FactoryConfig, OwnerRuntimeState, PoolDiscovery, PoolFactory,
    PoolGeneration, PoolInstanceId, PoolKey, PoolRegistration, PoolStateDependencies, PoolStatus,
    ProtocolId, ProtocolMetadata, RepairAction, RuntimeOwnerId, StateSlot,
    StateUpdate as AdapterStateUpdate, StateView, TokenEdgeDiscoveryRequest, UniswapV2Adapter,
    UniswapV2Metadata, UpdateQuality, uniswap_v2_pair_runtime_code_hash,
};

#[test]
fn discovery_options_preserve_candidate_limit() {
    let options = AmmDiscoveryOptions::default().with_max_candidates(7);
    assert_eq!(options.max_candidates(), Some(7));
}
use evm_fork_cache::StateUpdate as ForkStateUpdate;
use evm_fork_cache::bulk_storage::pack_slots_calldata;
use evm_fork_cache::cache::{EvmCache, EvmOverlay};
use evm_fork_cache::reactive::{
    AlloySubscriber, BlockRef, ChainStatus, InputSource, ReactiveContext, ReactiveInput,
    ReactiveInputBatch, ReactiveInputRecord, SubscriberConfig, SubscriberMode,
};
use revm::Database;
use tower::Service;

#[derive(Clone)]
struct GatedMockTransport {
    inner: MockTransport,
    permits: Arc<tokio::sync::Semaphore>,
    requests: Arc<Mutex<Vec<(String, String)>>>,
    gated_methods: Arc<Vec<&'static str>>,
}

impl GatedMockTransport {
    fn new(asserter: Asserter) -> Self {
        Self::gating(asserter, vec!["eth_call"])
    }

    fn gating(asserter: Asserter, gated_methods: Vec<&'static str>) -> Self {
        Self {
            inner: MockTransport::new(asserter),
            permits: Arc::new(tokio::sync::Semaphore::new(0)),
            requests: Arc::new(Mutex::new(Vec::new())),
            gated_methods: Arc::new(gated_methods),
        }
    }

    fn release_one(&self) {
        self.permits.add_permits(1);
    }

    fn requests(&self) -> Vec<(String, String)> {
        self.requests.lock().expect("request log lock").clone()
    }
}

impl Service<RequestPacket> for GatedMockTransport {
    type Response = ResponsePacket;
    type Error = TransportError;
    type Future = TransportFut<'static>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: RequestPacket) -> Self::Future {
        let should_gate = request
            .method_names()
            .any(|method| self.gated_methods.contains(&method));
        self.requests
            .lock()
            .expect("request log lock")
            .extend(request.requests().iter().map(|request| {
                (
                    request.method().to_owned(),
                    request
                        .params()
                        .map_or_else(String::new, |params| params.get().to_owned()),
                )
            }));
        let permits = Arc::clone(&self.permits);
        let mut inner = self.inner.clone();
        Box::pin(async move {
            if should_gate {
                permits
                    .acquire_owned()
                    .await
                    .expect("gate remains open")
                    .forget();
            }
            inner.call(request).await
        })
    }
}

async fn setup_cache() -> EvmCache {
    let provider = RootProvider::<AnyNetwork>::new(RpcClient::mocked(Asserter::new()));
    EvmCache::new(Arc::new(provider)).await
}

fn block_hash(block_number: u64) -> B256 {
    canonical_header(block_number).hash
}

fn canonical_header(block_number: u64) -> RpcHeader {
    RpcHeader::new(ConsensusHeader {
        parent_hash: if block_number == 0 {
            B256::ZERO
        } else {
            block_hash(block_number - 1)
        },
        number: block_number,
        timestamp: 1_700_000_000 + block_number,
        base_fee_per_gas: Some(100 + block_number),
        beneficiary: Address::repeat_byte(0xcb),
        gas_limit: 30_000_000,
        mix_hash: B256::repeat_byte(0xab),
        ..ConsensusHeader::default()
    })
}

fn canonical_child_header(parent: &RpcHeader) -> RpcHeader {
    let block_number = parent
        .inner
        .number
        .checked_add(1)
        .expect("test block range");
    RpcHeader::new(ConsensusHeader {
        parent_hash: parent.hash,
        number: block_number,
        timestamp: 1_700_000_000 + block_number,
        base_fee_per_gas: Some(100 + block_number),
        beneficiary: Address::repeat_byte(0xcb),
        gas_limit: 30_000_000,
        mix_hash: B256::repeat_byte(0xab),
        ..ConsensusHeader::default()
    })
}

fn runtime_baseline(block_number: u64) -> AmmRuntimeBaseline {
    AmmRuntimeBaseline::from_verified_header(1, canonical_header(block_number))
        .expect("test header is hash sealed")
}

fn align_cache(cache: &mut EvmCache, block_number: u64) {
    cache
        .advance_block(&canonical_header(block_number))
        .expect("test header has a complete block context");
}

struct TestWriteAdapter {
    protocol: &'static str,
    emitter: Address,
    topic: B256,
    target: StateSlot,
}

struct TestFailingAdapter {
    protocol: &'static str,
    emitter: Address,
    topic: B256,
}

struct TestRepairAdapter {
    protocol: &'static str,
    emitter: Address,
    topic: B256,
    target: StateSlot,
}

struct EmptyDiscoveryAdapter;

impl AmmAdapter for EmptyDiscoveryAdapter {
    fn protocol(&self) -> ProtocolId {
        ProtocolId::Custom("runtime-factory-index")
    }
}

struct CountingCreationFactory {
    factory: Address,
    topic: B256,
    decodes: Arc<AtomicUsize>,
}

impl PoolFactory for CountingCreationFactory {
    fn protocol(&self) -> ProtocolId {
        ProtocolId::Custom("runtime-factory-index")
    }

    fn factory_address(&self) -> Address {
        self.factory
    }

    fn creation_sources(&self) -> Vec<EventSource> {
        vec![EventSource::adapter_defined(self.factory, vec![self.topic])]
    }

    fn decode_creation(
        &self,
        _log: &PrimitiveLog,
        _context: CreationLogContext,
    ) -> Result<Option<DiscoveredPool>, DiscoveryError> {
        self.decodes.fetch_add(1, Ordering::SeqCst);
        Ok(None)
    }
}

impl AmmAdapter for TestRepairAdapter {
    fn protocol(&self) -> ProtocolId {
        ProtocolId::Custom(self.protocol)
    }

    fn event_sources(&self, _pool: &PoolRegistration) -> Vec<EventSource> {
        vec![EventSource::direct(self.emitter, vec![self.topic])]
    }

    fn state_dependencies(&self, _pool: &PoolRegistration) -> PoolStateDependencies {
        PoolStateDependencies::default()
            .with_associated_addresses([self.target.address()])
            .with_slots([self.target])
    }

    fn decode_event(
        &self,
        pool: &PoolRegistration,
        log: &PrimitiveLog,
        _view: &dyn StateView,
    ) -> AdapterEventResult {
        AdapterEventResult::event(
            AdapterEvent::new(
                pool.key.clone(),
                log.address,
                self.topic,
                AdapterEventKind::Unknown,
                UpdateQuality::RequiresRepair,
            )
            .with_repair(RepairAction::VerifySlots(vec![(
                self.target.address(),
                self.target.slot(),
            )])),
        )
    }
}

impl AmmAdapter for TestFailingAdapter {
    fn protocol(&self) -> ProtocolId {
        ProtocolId::Custom(self.protocol)
    }

    fn event_sources(&self, _pool: &PoolRegistration) -> Vec<EventSource> {
        vec![EventSource::direct(self.emitter, vec![self.topic])]
    }

    fn decode_event(
        &self,
        _pool: &PoolRegistration,
        _log: &PrimitiveLog,
        _view: &dyn StateView,
    ) -> AdapterEventResult {
        AdapterEventResult::error(AdapterEventError::MalformedLog("forced runtime failure"))
    }
}

impl AmmAdapter for TestWriteAdapter {
    fn protocol(&self) -> ProtocolId {
        ProtocolId::Custom(self.protocol)
    }

    fn event_sources(&self, _pool: &PoolRegistration) -> Vec<EventSource> {
        vec![EventSource::direct(self.emitter, vec![self.topic])]
    }

    fn state_dependencies(&self, _pool: &PoolRegistration) -> PoolStateDependencies {
        PoolStateDependencies::default()
            .with_associated_addresses([self.target.address()])
            .with_slots([self.target])
    }

    fn decode_event(
        &self,
        pool: &PoolRegistration,
        log: &PrimitiveLog,
        _view: &dyn StateView,
    ) -> AdapterEventResult {
        let value = U256::from(
            pool.key
                .address()
                .expect("test pool is address keyed")
                .as_slice()[19],
        );
        AdapterEventResult::event(
            AdapterEvent::new(
                pool.key.clone(),
                log.address,
                self.topic,
                AdapterEventKind::Unknown,
                UpdateQuality::Exact,
            )
            .with_updates([AdapterStateUpdate::slot(
                self.target.address(),
                self.target.slot(),
                value,
            )]),
        )
    }
}

fn custom_registration(protocol: &'static str, address: Address) -> PoolRegistration {
    PoolRegistration::new(PoolKey::Custom(CustomPoolKey::Address {
        protocol,
        address,
    }))
    .with_status(PoolStatus::Ready)
}

fn complete_v2_registration(address: Address) -> PoolRegistration {
    PoolRegistration::new(PoolKey::UniswapV2(address))
        .with_state_address(address)
        .with_metadata(ProtocolMetadata::UniswapV2(
            UniswapV2Metadata::default()
                .with_token0(Address::repeat_byte(0xa0))
                .with_token1(Address::repeat_byte(0xa1))
                .with_fee_bps(30),
        ))
}

fn encoded_words(words: impl IntoIterator<Item = U256>) -> Bytes {
    let mut encoded = Vec::new();
    for word in words {
        encoded.extend_from_slice(&word.to_be_bytes::<32>());
    }
    encoded.into()
}

fn v2_account_proof(address: Address) -> EIP1186AccountProofResponse {
    EIP1186AccountProofResponse {
        address,
        balance: U256::ZERO,
        code_hash: uniswap_v2_pair_runtime_code_hash(),
        nonce: 1,
        storage_hash: B256::repeat_byte(0x77),
        account_proof: Vec::new(),
        storage_proof: Vec::new(),
    }
}

fn canonical_log_batch(
    block_number: u64,
    logs: impl IntoIterator<Item = (Address, B256, u64)>,
) -> AmmCanonicalBatch {
    let block = BlockRef {
        number: block_number,
        hash: block_hash(block_number),
        parent_hash: Some(block_hash(block_number.saturating_sub(1))),
        timestamp: Some(1_700_000_000 + block_number),
    };
    let records = ReactiveInputBatch::new(
        logs.into_iter()
            .map(|(emitter, topic, log_index)| {
                ReactiveInputRecord::new(
                    ReactiveInput::Log(RpcLog {
                        inner: PrimitiveLog::new_unchecked(emitter, vec![topic], Bytes::new()),
                        block_hash: Some(block.hash),
                        block_number: Some(block.number),
                        transaction_hash: Some(B256::repeat_byte((log_index + 1) as u8)),
                        transaction_index: Some(0),
                        log_index: Some(log_index),
                        ..RpcLog::default()
                    }),
                    ReactiveContext {
                        chain_id: Some(1),
                        source: InputSource::Synthetic,
                        chain_status: ChainStatus::Included {
                            block: block.clone(),
                            confirmations: 0,
                        },
                        block: Some(block.clone()),
                        transaction_index: Some(0),
                        log_index: Some(log_index),
                    },
                )
            })
            .collect(),
    );
    AmmCanonicalBatch::from_verified_block(1, canonical_header(block_number), 0, records)
        .expect("test batch is block coherent")
}

fn canonical_primitive_log_batch(
    block_number: u64,
    interest_revision: u64,
    log: PrimitiveLog,
) -> AmmCanonicalBatch {
    let block = BlockRef {
        number: block_number,
        hash: block_hash(block_number),
        parent_hash: Some(block_hash(block_number.saturating_sub(1))),
        timestamp: Some(1_700_000_000 + block_number),
    };
    let record = ReactiveInputRecord::new(
        ReactiveInput::Log(RpcLog {
            inner: log,
            block_hash: Some(block.hash),
            block_number: Some(block.number),
            transaction_hash: Some(B256::repeat_byte(0xe1)),
            transaction_index: Some(0),
            log_index: Some(0),
            ..RpcLog::default()
        }),
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
        },
    );
    AmmCanonicalBatch::from_verified_block(
        1,
        canonical_header(block_number),
        interest_revision,
        ReactiveInputBatch::new(vec![record]),
    )
    .expect("factory log batch is block coherent")
}

#[tokio::test(flavor = "multi_thread")]
async fn observer_lag_is_explicit_and_latest_status_remains_recoverable() -> Result<()> {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut cache = setup_cache().await;
            align_cache(&mut cache, 500);
            let runtime = AmmRuntime::spawn(
                cache,
                AdapterRegistry::new(),
                runtime_baseline(500),
                AmmRuntimeConfig::default().with_observer_capacity(2),
            )?;
            let mut observer = runtime.subscribe_events();

            for block in 501..=504 {
                runtime.ingest_batch(empty_canonical_batch(block)).await?;
            }

            assert!(matches!(
                observer.next_event().await,
                Err(AmmObserverError::Lagged { skipped }) if skipped > 0
            ));
            let status = runtime.latest_status();
            assert_eq!(status.sequence(), 4);
            assert_eq!(status.state_version(), AmmStateVersion::new(4));
            assert_eq!(status.health(), AmmRuntimeHealth::Healthy);

            runtime.shutdown().await?;
            Ok(())
        })
        .await
}

fn raw_canonical_batch(block_number: u64) -> ReactiveInputBatch<Ethereum> {
    let block = BlockRef {
        number: block_number,
        hash: block_hash(block_number),
        parent_hash: Some(block_hash(block_number.saturating_sub(1))),
        timestamp: Some(1_700_000_000 + block_number),
    };
    ReactiveInputBatch::new(vec![ReactiveInputRecord::new(
        ReactiveInput::Log(RpcLog {
            block_hash: Some(block.hash),
            block_number: Some(block.number),
            transaction_hash: Some(B256::repeat_byte(0x44)),
            transaction_index: Some(0),
            log_index: Some(0),
            ..RpcLog::default()
        }),
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
        },
    )])
}

fn empty_canonical_batch(block_number: u64) -> AmmCanonicalBatch {
    empty_canonical_batch_from_header(canonical_header(block_number))
}

fn empty_canonical_batch_from_header(header: RpcHeader) -> AmmCanonicalBatch {
    AmmCanonicalBatch::from_verified_block(1, header, 0, ReactiveInputBatch::new(Vec::new()))
        .expect("test batch is block coherent")
}

fn alternate_empty_canonical_batch(block_number: u64) -> AmmCanonicalBatch {
    let mut inner = canonical_header(block_number).inner;
    inner.extra_data = Bytes::from_static(b"alternate");
    AmmCanonicalBatch::from_verified_block(
        1,
        RpcHeader::new(inner),
        0,
        ReactiveInputBatch::new(Vec::new()),
    )
    .expect("alternate test header is sealed")
}

#[tokio::test(flavor = "multi_thread")]
async fn runtime_spawn_publishes_an_immutable_initial_snapshot_and_shuts_down() -> Result<()> {
    tokio::task::LocalSet::new()
        .run_until(async {
            let address = Address::repeat_byte(0x11);
            let slot = U256::from(7);
            let value = U256::from(99);
            let point = AmmStatePoint::post_block(1, 500, block_hash(500));
            let mut cache = setup_cache().await;
            align_cache(&mut cache, 500);
            cache.apply_updates(&[ForkStateUpdate::slot(address, slot, value)]);

            let runtime = AmmRuntime::spawn(
                cache,
                AdapterRegistry::new(),
                runtime_baseline(500),
                AmmRuntimeConfig::default(),
            )?;
            let snapshot = runtime.latest_snapshot();
            let mut observer = runtime.subscribe_events();

            assert_eq!(snapshot.version(), AmmStateVersion::initial());
            assert_eq!(snapshot.point(), point);
            assert_eq!(snapshot.cache().storage_value(address, slot), Some(value));
            assert_eq!(snapshot.registry().pool_count(), 0);
            assert!(snapshot.pool_revisions().is_empty());

            runtime.shutdown().await?;
            assert!(matches!(
                observer.next_event().await?.kind(),
                AmmRuntimeEventKind::HealthChanged {
                    to: AmmRuntimeHealth::ShuttingDown,
                    ..
                }
            ));
            assert!(matches!(
                observer.next_event().await,
                Err(AmmObserverError::Closed)
            ));
            runtime.shutdown().await?;
            Ok(())
        })
        .await
}

#[tokio::test(flavor = "multi_thread")]
async fn alloy_driver_attaches_paused_then_stops_without_stranding_the_actor() -> Result<()> {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut cache = setup_cache().await;
            align_cache(&mut cache, 500);
            let runtime = AmmRuntime::spawn(
                cache,
                AdapterRegistry::new(),
                runtime_baseline(500),
                AmmRuntimeConfig::default(),
            )?;
            let provider = ProviderBuilder::new().connect_mocked_client(Asserter::new());
            let subscriber =
                AlloySubscriber::new(provider, SubscriberMode::Auto, SubscriberConfig::default());
            let driver = runtime
                .attach_alloy_subscriber(subscriber, AmmSubscriberDriverConfig::default())
                .await?;
            let mut driver_state = driver.subscribe_state();
            tokio::time::timeout(std::time::Duration::from_millis(100), async {
                while !matches!(&*driver_state.borrow(), AmmSubscriberDriverState::Failed(_)) {
                    driver_state
                        .changed()
                        .await
                        .expect("driver state remains observable");
                }
            })
            .await?;
            let mut status = runtime.subscribe_status();
            tokio::time::timeout(std::time::Duration::from_millis(100), async {
                while status.borrow().health() != AmmRuntimeHealth::Untrusted {
                    status
                        .changed()
                        .await
                        .expect("runtime status remains observable");
                }
            })
            .await?;
            assert!(matches!(
                driver.shutdown().await,
                Err(AmmSubscriberDriverError::Closed)
            ));
            runtime.shutdown().await?;
            Ok(())
        })
        .await
}

#[tokio::test(flavor = "multi_thread")]
async fn alloy_driver_rejects_polling_before_mutating_runtime_attachment() -> Result<()> {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut cache = setup_cache().await;
            align_cache(&mut cache, 500);
            let runtime = AmmRuntime::spawn(
                cache,
                AdapterRegistry::new(),
                runtime_baseline(500),
                AmmRuntimeConfig::default(),
            )?;
            let provider = ProviderBuilder::new().connect_mocked_client(Asserter::new());
            let subscriber = AlloySubscriber::new(
                provider,
                SubscriberMode::Polling,
                SubscriberConfig::default(),
            );
            assert!(matches!(
                runtime
                    .attach_alloy_subscriber(subscriber, AmmSubscriberDriverConfig::default())
                    .await,
                Err(AmmSubscriberDriverError::UnsupportedMode)
            ));
            runtime.shutdown().await?;
            Ok(())
        })
        .await
}

#[tokio::test(flavor = "multi_thread")]
async fn published_chain_reorg_rolls_forward_to_the_replacement_hash() -> Result<()> {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut cache = setup_cache().await;
            align_cache(&mut cache, 500);
            let runtime = AmmRuntime::spawn(
                cache,
                AdapterRegistry::new(),
                runtime_baseline(500),
                AmmRuntimeConfig::default(),
            )?;
            let mut observer = runtime.subscribe_events();
            runtime.ingest_batch(empty_canonical_batch(501)).await?;
            let replacement = alternate_empty_canonical_batch(501);
            let replacement_hash = replacement.block().hash;
            let changes = runtime.ingest_batch(replacement).await?;

            assert_eq!(changes.version(), AmmStateVersion::new(2));
            assert_eq!(changes.point().block_number(), 501);
            assert_eq!(changes.point().block_hash(), replacement_hash);
            assert!(matches!(
                changes.incidents(),
                [evm_amm_state::adapters::AmmStateIncident::Reorg { dropped }] if dropped.len() == 1
            ));
            assert!(matches!(
                observer.next_event().await?.kind(),
                AmmRuntimeEventKind::StateCommitted { .. }
            ));
            assert!(matches!(
                observer.next_event().await?.kind(),
                AmmRuntimeEventKind::Reorg { dropped } if dropped.len() == 1
            ));
            assert!(matches!(
                observer.next_event().await?.kind(),
                AmmRuntimeEventKind::StateCommitted { point, .. } if point.block_hash() == replacement_hash
            ));
            runtime.shutdown().await?;
            Ok(())
        })
        .await
}

#[tokio::test(flavor = "multi_thread")]
async fn change_subscription_starts_after_its_snapshot_and_observes_publication_order() -> Result<()>
{
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut cache = setup_cache().await;
            align_cache(&mut cache, 500);
            let runtime = AmmRuntime::spawn(
                cache,
                AdapterRegistry::new(),
                runtime_baseline(500),
                AmmRuntimeConfig::default(),
            )?;
            let mut snapshots = runtime.subscribe_snapshots();
            let mut statuses = runtime.subscribe_status();
            let mut subscription = runtime.subscribe_changes().await?;
            assert_eq!(
                subscription.snapshot().version(),
                AmmStateVersion::initial()
            );

            let committed = runtime.ingest_batch(empty_canonical_batch(501)).await?;
            snapshots.changed().await?;
            statuses.changed().await?;
            let delivered = subscription.next_commit().await.expect("change delivery");

            assert_eq!(committed.version(), AmmStateVersion::new(1));
            assert_eq!(delivered.changes().version(), AmmStateVersion::new(1));
            assert_eq!(
                delivered.snapshot().version(),
                delivered.changes().version()
            );
            assert_eq!(delivered.changes().point().block_number(), 501);
            let delivered_overlay = EvmOverlay::new(delivered.snapshot().cache_snapshot(), None);
            assert_eq!(delivered_overlay.block_number(), Some(501));
            assert_eq!(delivered_overlay.basefee(), Some(601));
            assert_eq!(delivered_overlay.timestamp(), Some(1_700_000_501));
            assert_eq!(
                runtime.latest_snapshot().version(),
                delivered.snapshot().version()
            );
            assert_eq!(
                runtime.latest_snapshot().point(),
                delivered.snapshot().point()
            );
            assert_eq!(snapshots.borrow().version(), AmmStateVersion::new(1));
            assert_eq!(statuses.borrow().state_version(), AmmStateVersion::new(1));

            runtime.shutdown().await?;
            Ok(())
        })
        .await
}

#[tokio::test(flavor = "multi_thread")]
async fn zero_change_canonical_commit_reuses_the_immutable_revision_index() -> Result<()> {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut cache = setup_cache().await;
            align_cache(&mut cache, 500);
            let runtime = AmmRuntime::spawn(
                cache,
                AdapterRegistry::new(),
                runtime_baseline(500),
                AmmRuntimeConfig::default(),
            )?;
            let before = runtime.latest_snapshot().pool_revisions_snapshot();

            runtime.ingest_batch(empty_canonical_batch(501)).await?;

            let after = runtime.latest_snapshot().pool_revisions_snapshot();
            assert!(
                Arc::ptr_eq(&before, &after),
                "a zero-change block must not copy the complete revision index"
            );
            runtime.shutdown().await?;
            Ok(())
        })
        .await
}

#[tokio::test(flavor = "multi_thread")]
async fn runtime_rejects_a_state_point_the_cache_context_does_not_represent() -> Result<()> {
    tokio::task::LocalSet::new()
        .run_until(async {
            let cache = setup_cache().await;
            let error = match AmmRuntime::spawn(
                cache,
                AdapterRegistry::new(),
                runtime_baseline(500),
                AmmRuntimeConfig::default(),
            ) {
                Err(error) => error,
                Ok(_) => panic!("an unpinned cache must not be published as post-block 500"),
            };

            assert!(matches!(
                error,
                evm_amm_state::adapters::AmmRuntimeSpawnError::BaselineBlockMismatch {
                    expected: 500,
                    actual: None,
                }
            ));
            Ok(())
        })
        .await
}

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_preempts_a_commit_waiting_on_critical_backpressure() -> Result<()> {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut cache = setup_cache().await;
            align_cache(&mut cache, 500);
            let runtime = AmmRuntime::spawn(
                cache,
                AdapterRegistry::new(),
                runtime_baseline(500),
                AmmRuntimeConfig::default().with_critical_change_capacity(1),
            )?;
            let _stalled_consumer = runtime.subscribe_changes().await?;
            runtime.ingest_batch(empty_canonical_batch(501)).await?;

            let blocked_handle = runtime.clone();
            let blocked = tokio::task::spawn_local(async move {
                blocked_handle
                    .ingest_batch(empty_canonical_batch(502))
                    .await
            });
            tokio::task::yield_now().await;

            tokio::time::timeout(std::time::Duration::from_millis(100), runtime.shutdown())
                .await
                .expect("shutdown must not wait for critical consumer capacity")?;
            assert!(matches!(
                blocked.await?,
                Err(evm_amm_state::adapters::AmmRuntimeCommandError::Closed)
            ));
            Ok(())
        })
        .await
}

#[tokio::test(flavor = "multi_thread")]
async fn prepared_pool_install_and_exact_removal_publish_atomic_topology_snapshots() -> Result<()> {
    tokio::task::LocalSet::new()
        .run_until(async {
            let point = AmmStatePoint::post_block(1, 500, block_hash(500));
            let mut cache = setup_cache().await;
            align_cache(&mut cache, 500);
            let target = StateSlot::new(Address::repeat_byte(0x61), U256::from(8));
            cache.apply_updates(&[ForkStateUpdate::slot(
                target.address(),
                target.slot(),
                U256::from(99),
            )]);
            let adapter = Arc::new(TestWriteAdapter {
                protocol: "runtime-prepared",
                emitter: Address::repeat_byte(0x62),
                topic: B256::repeat_byte(0x63),
                target,
            });
            let mut registry = AdapterRegistry::new();
            registry.register_adapter(adapter)?;
            let runtime = AmmRuntime::spawn(
                cache,
                registry,
                runtime_baseline(500),
                AmmRuntimeConfig::default(),
            )?;
            let baseline = runtime.latest_snapshot();
            let mut observer = runtime.subscribe_events();
            let registration = custom_registration("runtime-prepared", Address::repeat_byte(0x61));

            let added = runtime
                .install_prepared_pools(vec![registration], point)
                .await?;
            let instance = added.pool_changes()[0].pool().clone();
            let accepted_event =
                tokio::time::timeout(std::time::Duration::from_millis(50), observer.next_event())
                    .await??;
            let added_commit_event =
                tokio::time::timeout(std::time::Duration::from_millis(50), observer.next_event())
                    .await??;

            assert_eq!(added.pool_changes()[0].kind(), AmmPoolChangeKind::Added);
            assert_eq!(runtime.interest_revision(), 1);
            assert_eq!(runtime.latest_snapshot().interest_revision(), 1);
            assert!(matches!(
                accepted_event.kind(),
                AmmRuntimeEventKind::RegistrationAccepted { pool } if pool == &instance
            ));
            assert!(matches!(
                added_commit_event.kind(),
                AmmRuntimeEventKind::StateCommitted { version, .. }
                    if *version == AmmStateVersion::new(1)
            ));
            assert_eq!(runtime.latest_snapshot().version(), AmmStateVersion::new(1));
            assert_eq!(runtime.latest_snapshot().registry().pool_count(), 1);
            assert_eq!(
                runtime.latest_snapshot().pool_revision(&instance),
                Some(evm_amm_state::adapters::PoolStateRevision::new(0))
            );
            assert_eq!(baseline.registry().pool_count(), 0);

            let removed = runtime
                .remove_pool(instance.clone(), AmmEvictionPolicy::Retain)
                .await?;
            let removed_event =
                tokio::time::timeout(std::time::Duration::from_millis(50), observer.next_event())
                    .await??;
            let removed_commit_event =
                tokio::time::timeout(std::time::Duration::from_millis(50), observer.next_event())
                    .await??;
            assert_eq!(removed.pool_changes()[0].kind(), AmmPoolChangeKind::Removed);
            assert_eq!(runtime.interest_revision(), 2);
            assert_eq!(runtime.latest_snapshot().interest_revision(), 2);
            assert!(matches!(
                removed_event.kind(),
                AmmRuntimeEventKind::RegistrationRemoved { pool } if pool == &instance
            ));
            assert!(matches!(
                removed_commit_event.kind(),
                AmmRuntimeEventKind::StateCommitted { version, .. }
                    if *version == AmmStateVersion::new(2)
            ));
            assert_eq!(runtime.latest_status().sequence(), 4);
            assert_eq!(runtime.latest_snapshot().version(), AmmStateVersion::new(2));
            assert_eq!(runtime.latest_snapshot().registry().pool_count(), 0);
            assert_eq!(runtime.latest_snapshot().pool_revision(&instance), None);
            assert_eq!(baseline.registry().pool_count(), 0);

            runtime.shutdown().await?;
            Ok(())
        })
        .await
}

#[tokio::test(flavor = "multi_thread")]
async fn adapter_lifecycle_is_dynamic_generation_fenced_and_published() -> Result<()> {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut cache = setup_cache().await;
            align_cache(&mut cache, 500);
            let runtime = AmmRuntime::spawn(
                cache,
                AdapterRegistry::new(),
                runtime_baseline(500),
                AmmRuntimeConfig::default(),
            )?;
            let mut observer = runtime.subscribe_events();
            let adapter = Arc::new(TestWriteAdapter {
                protocol: "runtime-dynamic-adapter",
                emitter: Address::repeat_byte(0xa1),
                topic: B256::repeat_byte(0xa2),
                target: StateSlot::new(Address::repeat_byte(0xa3), U256::from(4)),
            });

            let first = runtime.add_adapter(adapter.clone()).await?;
            assert_eq!(first.generation().get(), 0);
            assert_eq!(runtime.latest_snapshot().version(), AmmStateVersion::new(1));
            assert_eq!(runtime.latest_snapshot().registry().adapter_count(), 1);
            assert!(matches!(
                observer.next_event().await?.kind(),
                AmmRuntimeEventKind::AdapterRegistrationAccepted { adapter } if adapter == &first
            ));
            assert!(matches!(
                observer.next_event().await?.kind(),
                AmmRuntimeEventKind::StateCommitted { version, .. }
                    if *version == AmmStateVersion::new(1)
            ));

            runtime.remove_adapter(first.clone()).await?;
            assert_eq!(runtime.latest_snapshot().version(), AmmStateVersion::new(2));
            assert_eq!(runtime.latest_snapshot().registry().adapter_count(), 0);
            assert!(matches!(
                observer.next_event().await?.kind(),
                AmmRuntimeEventKind::AdapterRegistrationRemoved { adapter } if adapter == &first
            ));
            assert!(matches!(
                observer.next_event().await?.kind(),
                AmmRuntimeEventKind::StateCommitted { version, .. }
                    if *version == AmmStateVersion::new(2)
            ));

            let second = runtime.add_adapter(adapter).await?;
            assert_eq!(second.key(), first.key());
            assert_eq!(second.generation().get(), 1);
            assert!(runtime.remove_adapter(first).await.is_err());
            assert_eq!(runtime.latest_snapshot().registry().adapter_count(), 1);

            runtime.shutdown().await?;
            Ok(())
        })
        .await
}

#[tokio::test(flavor = "multi_thread")]
async fn adapter_cascade_removes_its_pools_in_one_published_commit() -> Result<()> {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut cache = setup_cache().await;
            align_cache(&mut cache, 500);
            let target = StateSlot::new(Address::repeat_byte(0xb1), U256::from(7));
            cache.apply_updates(&[ForkStateUpdate::slot(
                target.address(),
                target.slot(),
                U256::from(11),
            )]);
            let adapter = Arc::new(TestWriteAdapter {
                protocol: "runtime-adapter-cascade",
                emitter: Address::repeat_byte(0xb2),
                topic: B256::repeat_byte(0xb3),
                target,
            });
            let mut registry = AdapterRegistry::new();
            registry.register_adapter(adapter)?;
            registry.register_pool(custom_registration(
                "runtime-adapter-cascade",
                target.address(),
            ))?;
            let runtime = AmmRuntime::spawn(
                cache,
                registry,
                runtime_baseline(500),
                AmmRuntimeConfig::default(),
            )?;
            let adapter_instance = runtime
                .latest_snapshot()
                .registry()
                .adapters()
                .next()
                .expect("initial adapter generation")
                .1
                .clone();
            let pool_instance = runtime
                .latest_snapshot()
                .registry()
                .pools()
                .next()
                .expect("initial pool generation")
                .1
                .clone();
            let mut observer = runtime.subscribe_events();

            let removed = runtime
                .remove_adapter_cascade(adapter_instance.clone(), AmmEvictionPolicy::Retain)
                .await?;
            assert_eq!(removed.version(), AmmStateVersion::new(1));
            assert_eq!(removed.pool_changes().len(), 1);
            assert_eq!(removed.pool_changes()[0].pool(), &pool_instance);
            assert_eq!(removed.pool_changes()[0].kind(), AmmPoolChangeKind::Removed);
            assert_eq!(runtime.latest_snapshot().registry().pool_count(), 0);
            assert_eq!(runtime.latest_snapshot().registry().adapter_count(), 0);
            assert_eq!(runtime.interest_revision(), 1);

            assert!(matches!(
                observer.next_event().await?.kind(),
                AmmRuntimeEventKind::RegistrationRemoved { pool } if pool == &pool_instance
            ));
            assert!(matches!(
                observer.next_event().await?.kind(),
                AmmRuntimeEventKind::AdapterRegistrationRemoved { adapter }
                    if adapter == &adapter_instance
            ));
            assert!(matches!(
                observer.next_event().await?.kind(),
                AmmRuntimeEventKind::StateCommitted { version, .. }
                    if *version == AmmStateVersion::new(1)
            ));

            runtime.shutdown().await?;
            Ok(())
        })
        .await
}

#[tokio::test(flavor = "multi_thread")]
async fn factory_watcher_lifecycle_is_generation_fenced_and_recoverable() -> Result<()> {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut cache = setup_cache().await;
            align_cache(&mut cache, 500);
            let mut registry = AdapterRegistry::new();
            registry.register_adapter(Arc::new(UniswapV2Adapter::default()))?;
            let discovery = Arc::new(PoolDiscovery::for_registry(
                &registry,
                FactoryConfig::default().with_uniswap_v2_factory(Address::repeat_byte(0xc1)),
            ));
            let runtime = AmmRuntime::spawn(
                cache,
                registry,
                runtime_baseline(500),
                AmmRuntimeConfig::default(),
            )?;
            let adapter = runtime
                .latest_snapshot()
                .registry()
                .adapters()
                .next()
                .expect("V2 adapter generation")
                .1
                .clone();
            let key = DiscoveryOwnerKey::new("mainnet-v2-factory");
            let mut observer = runtime.subscribe_events();

            let first = runtime
                .add_factory_watcher(AmmFactoryWatcherRegistration::new(
                    key.clone(),
                    adapter.clone(),
                    Arc::clone(&discovery),
                ))
                .await?;
            assert_eq!(first.generation().get(), 0);
            assert_eq!(runtime.interest_revision(), 1);
            assert_eq!(
                runtime.latest_status().discovery_state(&first),
                Some(OwnerRuntimeState::Active)
            );
            assert!(matches!(
                observer.next_event().await?.kind(),
                AmmRuntimeEventKind::DiscoveryRegistrationAccepted { owner } if owner == &first
            ));
            assert!(matches!(
                observer.next_event().await?.kind(),
                AmmRuntimeEventKind::StateCommitted { version, .. }
                    if *version == AmmStateVersion::new(1)
            ));
            assert!(runtime.remove_adapter(adapter.clone()).await.is_err());
            assert_eq!(runtime.latest_snapshot().registry().adapter_count(), 1);

            runtime.remove_factory_watcher(first.clone()).await?;
            assert_eq!(runtime.interest_revision(), 2);
            assert_eq!(
                runtime.latest_status().discovery_state(&first),
                Some(OwnerRuntimeState::Removed)
            );
            assert!(matches!(
                observer.next_event().await?.kind(),
                AmmRuntimeEventKind::DiscoveryRegistrationRemoved { owner } if owner == &first
            ));
            assert!(matches!(
                observer.next_event().await?.kind(),
                AmmRuntimeEventKind::StateCommitted { version, .. }
                    if *version == AmmStateVersion::new(2)
            ));

            let second = runtime
                .add_factory_watcher(AmmFactoryWatcherRegistration::new(
                    key,
                    adapter.clone(),
                    discovery,
                ))
                .await?;
            assert_eq!(second.generation().get(), 1);
            assert!(runtime.remove_factory_watcher(first).await.is_err());
            assert_eq!(
                runtime.latest_status().discovery_state(&second),
                Some(OwnerRuntimeState::Active)
            );

            runtime
                .remove_adapter_cascade(adapter, AmmEvictionPolicy::Retain)
                .await?;
            assert_eq!(
                runtime.latest_status().discovery_state(&second),
                Some(OwnerRuntimeState::Removed)
            );
            assert_eq!(runtime.latest_snapshot().registry().adapter_count(), 0);

            runtime.shutdown().await?;
            Ok(())
        })
        .await
}

#[tokio::test(flavor = "multi_thread")]
async fn factory_watcher_dispatch_skips_unrelated_decoders_and_updates_on_remove() -> Result<()> {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut cache = setup_cache().await;
            align_cache(&mut cache, 500);
            let mut registry = AdapterRegistry::new();
            registry.register_adapter(Arc::new(EmptyDiscoveryAdapter))?;
            let runtime = AmmRuntime::spawn(
                cache,
                registry,
                runtime_baseline(500),
                AmmRuntimeConfig::default(),
            )?;
            let adapter = runtime
                .latest_snapshot()
                .registry()
                .adapters()
                .next()
                .expect("custom adapter generation")
                .1
                .clone();
            let factory_a = Address::repeat_byte(0xa1);
            let factory_b = Address::repeat_byte(0xb1);
            let topic_a = B256::repeat_byte(0xa2);
            let topic_b = B256::repeat_byte(0xb2);
            let decodes_a = Arc::new(AtomicUsize::new(0));
            let decodes_b = Arc::new(AtomicUsize::new(0));
            let watcher_a = runtime
                .add_factory_watcher(AmmFactoryWatcherRegistration::new(
                    DiscoveryOwnerKey::new("indexed-factory-a"),
                    adapter.clone(),
                    Arc::new(PoolDiscovery::new([Box::new(CountingCreationFactory {
                        factory: factory_a,
                        topic: topic_a,
                        decodes: Arc::clone(&decodes_a),
                    })
                        as Box<dyn PoolFactory>])),
                ))
                .await?;
            let watcher_b = runtime
                .add_factory_watcher(AmmFactoryWatcherRegistration::new(
                    DiscoveryOwnerKey::new("indexed-factory-b"),
                    adapter.clone(),
                    Arc::new(PoolDiscovery::new([Box::new(CountingCreationFactory {
                        factory: factory_b,
                        topic: topic_b,
                        decodes: Arc::clone(&decodes_b),
                    })
                        as Box<dyn PoolFactory>])),
                ))
                .await?;

            runtime
                .ingest_batch(canonical_primitive_log_batch(
                    501,
                    runtime.interest_revision(),
                    PrimitiveLog::new_unchecked(
                        Address::repeat_byte(0xcc),
                        vec![B256::repeat_byte(0xcd)],
                        Bytes::new(),
                    ),
                ))
                .await?;
            assert_eq!(decodes_a.load(Ordering::SeqCst), 0);
            assert_eq!(decodes_b.load(Ordering::SeqCst), 0);

            runtime
                .ingest_batch(canonical_primitive_log_batch(
                    502,
                    runtime.interest_revision(),
                    PrimitiveLog::new_unchecked(factory_a, vec![topic_a], Bytes::new()),
                ))
                .await?;
            assert_eq!(decodes_a.load(Ordering::SeqCst), 1);
            assert_eq!(decodes_b.load(Ordering::SeqCst), 0);

            runtime.remove_factory_watcher(watcher_a).await?;
            runtime
                .ingest_batch(canonical_primitive_log_batch(
                    503,
                    runtime.interest_revision(),
                    PrimitiveLog::new_unchecked(factory_a, vec![topic_a], Bytes::new()),
                ))
                .await?;
            assert_eq!(decodes_a.load(Ordering::SeqCst), 1);
            assert_eq!(decodes_b.load(Ordering::SeqCst), 0);

            runtime
                .ingest_batch(canonical_primitive_log_batch(
                    504,
                    runtime.interest_revision(),
                    PrimitiveLog::new_unchecked(factory_b, vec![topic_b], Bytes::new()),
                ))
                .await?;
            assert_eq!(decodes_a.load(Ordering::SeqCst), 1);
            assert_eq!(decodes_b.load(Ordering::SeqCst), 1);

            runtime.remove_factory_watcher(watcher_b).await?;
            runtime.remove_adapter(adapter).await?;
            runtime.shutdown().await?;
            Ok(())
        })
        .await
}

#[tokio::test(flavor = "multi_thread")]
async fn connector_discovery_streams_each_found_pool_into_cold_start() -> Result<()> {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut cache = setup_cache().await;
            align_cache(&mut cache, 500);
            let factory = Address::repeat_byte(0xd1);
            let token = Address::repeat_byte(0xd2);
            let connector = Address::repeat_byte(0xd3);
            let pool = Address::repeat_byte(0xd4);
            let mut registry = AdapterRegistry::new();
            registry.register_adapter(Arc::new(UniswapV2Adapter::default()))?;
            let discovery = Arc::new(PoolDiscovery::for_registry(
                &registry,
                FactoryConfig::default().with_uniswap_v2_factory(factory),
            ));
            let runtime = AmmRuntime::spawn(
                cache,
                registry,
                runtime_baseline(500),
                AmmRuntimeConfig::default(),
            )?;
            let adapter = runtime
                .latest_snapshot()
                .registry()
                .adapters()
                .next()
                .expect("V2 adapter generation")
                .1
                .clone();
            let owner = runtime
                .add_factory_watcher(AmmFactoryWatcherRegistration::new(
                    DiscoveryOwnerKey::new("connector-v2"),
                    adapter.clone(),
                    discovery,
                ))
                .await?;

            let assertions = Asserter::new();
            assertions.push_success(&pack_slots_calldata(&[U256::from_be_slice(
                pool.as_slice(),
            )]));
            assertions.push_success(&encoded_words([
                U256::from_be_slice(token.as_slice()),
                U256::from_be_slice(connector.as_slice()),
                U256::from(55) | (U256::from(66) << 112),
            ]));
            assertions.push_success(&v2_account_proof(pool));
            let provider = RootProvider::<AnyNetwork>::new(RpcClient::mocked(assertions));
            let worker = runtime
                .attach_cold_start_worker(
                    provider,
                    AmmColdStartWorkerConfig::default().with_queue_capacity(1),
                )
                .await?;
            let scheduled = runtime
                .queue_token_discovery(
                    owner.clone(),
                    TokenEdgeDiscoveryRequest::new(token, [connector])
                        .with_protocol(ProtocolId::UniswapV2),
                    AmmDiscoveryOptions::default(),
                )
                .await?;
            assert_eq!(
                scheduled.work().owner(),
                &RuntimeOwnerId::Discovery(scheduled.owner().clone())
            );

            tokio::time::timeout(std::time::Duration::from_secs(1), async {
                while runtime.latest_snapshot().registry().pool_count() == 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await?;
            let snapshot = runtime.latest_snapshot();
            assert!(
                snapshot
                    .registry()
                    .pool_instance(&PoolKey::UniswapV2(pool))
                    .is_some()
            );
            assert_eq!(snapshot.registry().pool_count(), 1);
            let instance = snapshot
                .registry()
                .pool_instance(&PoolKey::UniswapV2(pool))
                .expect("discovered pool generation")
                .clone();
            runtime
                .remove_pool(instance, AmmEvictionPolicy::Retain)
                .await?;
            runtime.remove_factory_watcher(owner).await?;
            runtime.remove_adapter(adapter).await?;
            let removed = runtime.latest_snapshot();
            assert_eq!(removed.registry().pool_count(), 0);
            assert_eq!(removed.registry().adapter_count(), 0);

            worker.shutdown();
            runtime.shutdown().await?;
            Ok(())
        })
        .await
}

#[tokio::test(flavor = "multi_thread")]
async fn orphaned_query_registration_stays_available_until_revalidation_rejects_it() -> Result<()> {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut cache = setup_cache().await;
            align_cache(&mut cache, 500);
            let factory = Address::repeat_byte(0xd9);
            let token = Address::repeat_byte(0xda);
            let connector = Address::repeat_byte(0xdb);
            let pool = Address::repeat_byte(0xdc);
            let mut registry = AdapterRegistry::new();
            registry.register_adapter(Arc::new(UniswapV2Adapter::default()))?;
            let discovery = Arc::new(PoolDiscovery::for_registry(
                &registry,
                FactoryConfig::default().with_uniswap_v2_factory(factory),
            ));
            let runtime = AmmRuntime::spawn(
                cache,
                registry,
                runtime_baseline(500),
                AmmRuntimeConfig::default(),
            )?;
            let adapter = runtime
                .latest_snapshot()
                .registry()
                .adapters()
                .next()
                .expect("V2 adapter generation")
                .1
                .clone();
            let owner = runtime
                .add_factory_watcher(AmmFactoryWatcherRegistration::new(
                    DiscoveryOwnerKey::new("revalidate-query-v2"),
                    adapter.clone(),
                    discovery,
                ))
                .await?;
            runtime
                .ingest_batch(AmmCanonicalBatch::from_verified_block(
                    1,
                    canonical_header(501),
                    runtime.interest_revision(),
                    ReactiveInputBatch::new(Vec::new()),
                )?)
                .await?;

            let assertions = Asserter::new();
            assertions.push_success(&pack_slots_calldata(&[U256::from_be_slice(
                pool.as_slice(),
            )]));
            assertions.push_success(&encoded_words([
                U256::from_be_slice(token.as_slice()),
                U256::from_be_slice(connector.as_slice()),
                U256::from(12) | (U256::from(13) << 112),
            ]));
            assertions.push_success(&v2_account_proof(pool));
            assertions.push_success(&pack_slots_calldata(&[U256::ZERO]));
            assertions.push_success(&pack_slots_calldata(&[U256::from_be_slice(
                pool.as_slice(),
            )]));
            assertions.push_success(&encoded_words([
                U256::from_be_slice(token.as_slice()),
                U256::from_be_slice(connector.as_slice()),
                U256::from(14) | (U256::from(15) << 112),
            ]));
            assertions.push_success(&v2_account_proof(pool));
            let transport = GatedMockTransport::gating(assertions, vec!["eth_getStorageAt"]);
            transport.release_one();
            let provider = RootProvider::<AnyNetwork>::new(RpcClient::new(transport.clone(), true));
            let worker = runtime
                .attach_cold_start_worker(
                    provider,
                    AmmColdStartWorkerConfig::default().with_queue_capacity(1),
                )
                .await?;
            runtime
                .queue_token_discovery(
                    owner.clone(),
                    TokenEdgeDiscoveryRequest::new(token, [connector])
                        .with_protocol(ProtocolId::UniswapV2),
                    AmmDiscoveryOptions::default(),
                )
                .await?;
            tokio::time::timeout(std::time::Duration::from_secs(1), async {
                while runtime
                    .latest_snapshot()
                    .registry()
                    .pool_instance(&PoolKey::UniswapV2(pool))
                    .is_none()
                {
                    tokio::task::yield_now().await;
                }
            })
            .await?;
            let first_instance = runtime
                .latest_snapshot()
                .registry()
                .pool_instance(&PoolKey::UniswapV2(pool))
                .expect("first discovered generation")
                .clone();

            let mut replacement = canonical_header(501).inner;
            replacement.extra_data = Bytes::from_static(b"orphan-query");
            runtime
                .ingest_batch(AmmCanonicalBatch::from_verified_block(
                    1,
                    RpcHeader::new(replacement),
                    runtime.interest_revision(),
                    ReactiveInputBatch::new(Vec::new()),
                )?)
                .await?;
            let revalidation_started =
                tokio::time::timeout(std::time::Duration::from_millis(250), async {
                    while transport.requests().len() < 4 {
                        tokio::task::yield_now().await;
                    }
                })
                .await;
            assert!(
                revalidation_started.is_ok(),
                "requests: {:?}, status: {:?}",
                transport.requests(),
                runtime.latest_status()
            );
            assert!(
                runtime
                    .latest_snapshot()
                    .registry()
                    .pool_instance(&PoolKey::UniswapV2(pool))
                    .is_some(),
                "orphaned query evidence stays usable while its authoritative recheck is pending"
            );

            transport.release_one();
            let removed = tokio::time::timeout(std::time::Duration::from_secs(1), async {
                while runtime
                    .latest_snapshot()
                    .registry()
                    .pool_instance(&PoolKey::UniswapV2(pool))
                    .is_some()
                {
                    tokio::task::yield_now().await;
                }
            })
            .await;
            assert!(
                removed.is_ok(),
                "requests: {:?}, status: {:?}, pool_count: {}",
                transport.requests(),
                runtime.latest_status(),
                runtime.latest_snapshot().registry().pool_count()
            );

            transport.release_one();
            runtime
                .queue_token_discovery(
                    owner.clone(),
                    TokenEdgeDiscoveryRequest::new(token, [connector])
                        .with_protocol(ProtocolId::UniswapV2),
                    AmmDiscoveryOptions::default(),
                )
                .await?;
            tokio::time::timeout(std::time::Duration::from_secs(1), async {
                while runtime
                    .latest_snapshot()
                    .registry()
                    .pool_instance(&PoolKey::UniswapV2(pool))
                    .is_none()
                {
                    tokio::task::yield_now().await;
                }
            })
            .await?;
            let replacement = runtime
                .latest_snapshot()
                .registry()
                .pool_instance(&PoolKey::UniswapV2(pool))
                .expect("canonical rediscovery publishes a replacement generation")
                .clone();
            assert_ne!(replacement, first_instance);
            assert_eq!(replacement.generation().get(), 1);

            runtime
                .remove_pool(replacement, AmmEvictionPolicy::Retain)
                .await?;
            runtime.remove_factory_watcher(owner).await?;
            runtime.remove_adapter(adapter).await?;

            worker.shutdown();
            runtime.shutdown().await?;
            Ok(())
        })
        .await
}

#[tokio::test(flavor = "multi_thread")]
async fn duplicate_query_evidence_survives_one_watcher_removal_and_fences_the_last() -> Result<()> {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut cache = setup_cache().await;
            align_cache(&mut cache, 500);
            let factory = Address::repeat_byte(0xdd);
            let token = Address::repeat_byte(0xde);
            let connector = Address::repeat_byte(0xdf);
            let pool = Address::repeat_byte(0xe0);
            let mut registry = AdapterRegistry::new();
            registry.register_adapter(Arc::new(UniswapV2Adapter::default()))?;
            let discovery = Arc::new(PoolDiscovery::for_registry(
                &registry,
                FactoryConfig::default().with_uniswap_v2_factory(factory),
            ));
            let runtime = AmmRuntime::spawn(
                cache,
                registry,
                runtime_baseline(500),
                AmmRuntimeConfig::default(),
            )?;
            let adapter = runtime
                .latest_snapshot()
                .registry()
                .adapters()
                .next()
                .expect("V2 adapter generation")
                .1
                .clone();
            let owner_a = runtime
                .add_factory_watcher(AmmFactoryWatcherRegistration::new(
                    DiscoveryOwnerKey::new("duplicate-query-a"),
                    adapter.clone(),
                    Arc::clone(&discovery),
                ))
                .await?;
            let owner_b = runtime
                .add_factory_watcher(AmmFactoryWatcherRegistration::new(
                    DiscoveryOwnerKey::new("duplicate-query-b"),
                    adapter.clone(),
                    discovery,
                ))
                .await?;

            let assertions = Asserter::new();
            assertions.push_success(&pack_slots_calldata(&[U256::from_be_slice(
                pool.as_slice(),
            )]));
            assertions.push_success(&encoded_words([
                U256::from_be_slice(token.as_slice()),
                U256::from_be_slice(connector.as_slice()),
                U256::from(21) | (U256::from(22) << 112),
            ]));
            assertions.push_success(&v2_account_proof(pool));
            assertions.push_success(&pack_slots_calldata(&[U256::from_be_slice(
                pool.as_slice(),
            )]));
            let provider = RootProvider::<AnyNetwork>::new(RpcClient::mocked(assertions));
            let worker = runtime
                .attach_cold_start_worker(
                    provider,
                    AmmColdStartWorkerConfig::default().with_queue_capacity(1),
                )
                .await?;
            runtime
                .queue_token_discovery(
                    owner_a.clone(),
                    TokenEdgeDiscoveryRequest::new(token, [connector])
                        .with_protocol(ProtocolId::UniswapV2),
                    AmmDiscoveryOptions::default(),
                )
                .await?;
            tokio::time::timeout(std::time::Duration::from_secs(1), async {
                while runtime
                    .latest_snapshot()
                    .registry()
                    .pool_instance(&PoolKey::UniswapV2(pool))
                    .is_none()
                {
                    tokio::task::yield_now().await;
                }
            })
            .await?;
            let duplicate = runtime
                .queue_token_discovery(
                    owner_b.clone(),
                    TokenEdgeDiscoveryRequest::new(token, [connector])
                        .with_protocol(ProtocolId::UniswapV2),
                    AmmDiscoveryOptions::default(),
                )
                .await?;
            tokio::time::timeout(std::time::Duration::from_secs(1), async {
                while runtime
                    .latest_status()
                    .active_work(duplicate.work())
                    .is_some()
                {
                    tokio::task::yield_now().await;
                }
            })
            .await?;

            runtime.remove_factory_watcher(owner_a).await?;
            assert!(
                runtime
                    .latest_snapshot()
                    .registry()
                    .pool_instance(&PoolKey::UniswapV2(pool))
                    .is_some(),
                "independent query evidence keeps the pool registered"
            );
            assert!(matches!(
                runtime.remove_factory_watcher(owner_b.clone()).await,
                Err(AmmRuntimeCommandError::DiscoveryOwnerInUse { owner, pools })
                    if owner.as_ref() == &owner_b && pools.len() == 1
            ));
            assert_eq!(
                runtime.latest_status().discovery_state(&owner_b),
                Some(OwnerRuntimeState::Active)
            );

            let instance = runtime
                .latest_snapshot()
                .registry()
                .pool_instance(&PoolKey::UniswapV2(pool))
                .expect("pool remains active")
                .clone();
            runtime
                .remove_pool(instance, AmmEvictionPolicy::Retain)
                .await?;
            runtime.remove_factory_watcher(owner_b).await?;
            runtime.remove_adapter(adapter).await?;
            worker.shutdown();
            runtime.shutdown().await?;
            Ok(())
        })
        .await
}

#[tokio::test(flavor = "multi_thread")]
async fn watcher_removal_cancels_a_handed_off_pool_hydration() -> Result<()> {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut cache = setup_cache().await;
            align_cache(&mut cache, 500);
            let factory = Address::repeat_byte(0xd5);
            let token = Address::repeat_byte(0xd6);
            let connector = Address::repeat_byte(0xd7);
            let pool = Address::repeat_byte(0xd8);
            let mut registry = AdapterRegistry::new();
            registry.register_adapter(Arc::new(UniswapV2Adapter::default()))?;
            let discovery = Arc::new(PoolDiscovery::for_registry(
                &registry,
                FactoryConfig::default().with_uniswap_v2_factory(factory),
            ));
            let runtime = AmmRuntime::spawn(
                cache,
                registry,
                runtime_baseline(500),
                AmmRuntimeConfig::default(),
            )?;
            let adapter = runtime
                .latest_snapshot()
                .registry()
                .adapters()
                .next()
                .expect("V2 adapter generation")
                .1
                .clone();
            let owner = runtime
                .add_factory_watcher(AmmFactoryWatcherRegistration::new(
                    DiscoveryOwnerKey::new("cancel-successor-v2"),
                    adapter,
                    discovery,
                ))
                .await?;
            let assertions = Asserter::new();
            assertions.push_success(&pack_slots_calldata(&[U256::from_be_slice(
                pool.as_slice(),
            )]));
            assertions.push_success(&encoded_words([
                U256::from_be_slice(token.as_slice()),
                U256::from_be_slice(connector.as_slice()),
                U256::from(9) | (U256::from(10) << 112),
            ]));
            assertions.push_success(&v2_account_proof(pool));
            let transport = GatedMockTransport::new(assertions);
            let provider = RootProvider::<AnyNetwork>::new(RpcClient::new(transport.clone(), true));
            let worker = runtime
                .attach_cold_start_worker(
                    provider,
                    AmmColdStartWorkerConfig::default().with_queue_capacity(1),
                )
                .await?;
            runtime
                .queue_token_discovery(
                    owner.clone(),
                    TokenEdgeDiscoveryRequest::new(token, [connector])
                        .with_protocol(ProtocolId::UniswapV2),
                    AmmDiscoveryOptions::default(),
                )
                .await?;
            let handed_off = tokio::time::timeout(std::time::Duration::from_millis(250), async {
                while transport
                    .requests()
                    .iter()
                    .filter(|(method, _)| method == "eth_call")
                    .count()
                    < 1
                {
                    tokio::task::yield_now().await;
                }
            })
            .await;
            assert!(handed_off.is_ok(), "requests: {:?}", transport.requests());

            runtime.remove_factory_watcher(owner).await?;
            transport.release_one();
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            assert!(
                runtime
                    .latest_snapshot()
                    .registry()
                    .pool_instance(&PoolKey::UniswapV2(pool))
                    .is_none()
            );

            worker.shutdown();
            runtime.shutdown().await?;
            Ok(())
        })
        .await
}

#[tokio::test(flavor = "multi_thread")]
async fn factory_creation_event_queues_the_pool_after_its_block_commits() -> Result<()> {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut cache = setup_cache().await;
            align_cache(&mut cache, 500);
            let factory = Address::repeat_byte(0xe2);
            let token0 = Address::repeat_byte(0xe3);
            let token1 = Address::repeat_byte(0xe4);
            let pool = Address::repeat_byte(0xe5);
            let mut registry = AdapterRegistry::new();
            registry.register_adapter(Arc::new(UniswapV2Adapter::default()))?;
            let discovery = Arc::new(PoolDiscovery::for_registry(
                &registry,
                FactoryConfig::default().with_uniswap_v2_factory(factory),
            ));
            let runtime = AmmRuntime::spawn(
                cache,
                registry,
                runtime_baseline(500),
                AmmRuntimeConfig::default(),
            )?;
            let adapter = runtime
                .latest_snapshot()
                .registry()
                .adapters()
                .next()
                .expect("V2 adapter generation")
                .1
                .clone();
            runtime
                .add_factory_watcher(AmmFactoryWatcherRegistration::new(
                    DiscoveryOwnerKey::new("factory-event-v2"),
                    adapter,
                    discovery,
                ))
                .await?;

            let assertions = Asserter::new();
            assertions.push_success(&encoded_words([
                U256::from_be_slice(token0.as_slice()),
                U256::from_be_slice(token1.as_slice()),
                U256::from(77) | (U256::from(88) << 112),
            ]));
            assertions.push_success(&v2_account_proof(pool));
            let transport = GatedMockTransport::new(assertions);
            transport.release_one();
            transport.release_one();
            let provider = RootProvider::<AnyNetwork>::new(RpcClient::new(transport.clone(), true));
            let worker = runtime
                .attach_cold_start_worker(provider, AmmColdStartWorkerConfig::default())
                .await?;

            let address_topic = |address: Address| {
                let mut word = [0_u8; 32];
                word[12..].copy_from_slice(address.as_slice());
                B256::from(word)
            };
            let mut data = [0_u8; 64];
            data[12..32].copy_from_slice(pool.as_slice());
            data[63] = 1;
            let log = PrimitiveLog::new_unchecked(
                factory,
                vec![
                    keccak256("PairCreated(address,address,address,uint256)"),
                    address_topic(token0),
                    address_topic(token1),
                ],
                Bytes::copy_from_slice(&data),
            );
            runtime
                .ingest_batch(canonical_primitive_log_batch(501, 1, log))
                .await?;
            assert_eq!(runtime.latest_snapshot().point().block_number(), 501);

            tokio::time::timeout(std::time::Duration::from_secs(1), async {
                while runtime.latest_snapshot().registry().pool_count() == 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await?;
            assert!(
                runtime
                    .latest_snapshot()
                    .registry()
                    .pool_instance(&PoolKey::UniswapV2(pool))
                    .is_some()
            );

            let mut replacement_header = canonical_header(501).inner;
            replacement_header.extra_data = Bytes::from_static(b"orphan-factory-log");
            let replacement = AmmCanonicalBatch::from_verified_block(
                1,
                RpcHeader::new(replacement_header),
                runtime.interest_revision(),
                ReactiveInputBatch::new(Vec::new()),
            )?;
            runtime.ingest_batch(replacement).await?;
            assert!(
                runtime
                    .latest_snapshot()
                    .registry()
                    .pool_instance(&PoolKey::UniswapV2(pool))
                    .is_none(),
                "factory-only evidence is removed when its block is orphaned"
            );

            worker.shutdown();
            runtime.shutdown().await?;
            Ok(())
        })
        .await
}

#[tokio::test(flavor = "multi_thread")]
async fn orphaned_factory_candidate_cannot_queue_after_late_worker_attachment() -> Result<()> {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut cache = setup_cache().await;
            align_cache(&mut cache, 500);
            let factory = Address::repeat_byte(0xf2);
            let token0 = Address::repeat_byte(0xf3);
            let token1 = Address::repeat_byte(0xf4);
            let pool = Address::repeat_byte(0xf5);
            let mut registry = AdapterRegistry::new();
            registry.register_adapter(Arc::new(UniswapV2Adapter::default()))?;
            let discovery = Arc::new(PoolDiscovery::for_registry(
                &registry,
                FactoryConfig::default().with_uniswap_v2_factory(factory),
            ));
            let runtime = AmmRuntime::spawn(
                cache,
                registry,
                runtime_baseline(500),
                AmmRuntimeConfig::default(),
            )?;
            let adapter = runtime
                .latest_snapshot()
                .registry()
                .adapters()
                .next()
                .expect("V2 adapter generation")
                .1
                .clone();
            runtime
                .add_factory_watcher(AmmFactoryWatcherRegistration::new(
                    DiscoveryOwnerKey::new("orphaned-pending-factory-event"),
                    adapter,
                    discovery,
                ))
                .await?;

            let address_topic = |address: Address| {
                let mut word = [0_u8; 32];
                word[12..].copy_from_slice(address.as_slice());
                B256::from(word)
            };
            let mut data = [0_u8; 64];
            data[12..32].copy_from_slice(pool.as_slice());
            data[63] = 1;
            let log = PrimitiveLog::new_unchecked(
                factory,
                vec![
                    keccak256("PairCreated(address,address,address,uint256)"),
                    address_topic(token0),
                    address_topic(token1),
                ],
                Bytes::copy_from_slice(&data),
            );
            runtime
                .ingest_batch(canonical_primitive_log_batch(
                    501,
                    runtime.interest_revision(),
                    log,
                ))
                .await?;
            assert_eq!(runtime.latest_snapshot().registry().pool_count(), 0);
            assert_eq!(runtime.latest_status().active_work_items().count(), 0);

            let mut replacement_header = canonical_header(501).inner;
            replacement_header.extra_data = Bytes::from_static(b"orphan-pending-factory-log");
            runtime
                .ingest_batch(AmmCanonicalBatch::from_verified_block(
                    1,
                    RpcHeader::new(replacement_header),
                    runtime.interest_revision(),
                    ReactiveInputBatch::new(Vec::new()),
                )?)
                .await?;

            let provider = RootProvider::<AnyNetwork>::new(RpcClient::mocked(Asserter::new()));
            let worker = runtime
                .attach_cold_start_worker(provider, AmmColdStartWorkerConfig::default())
                .await?;
            assert_eq!(
                runtime.latest_status().active_work_items().count(),
                0,
                "an orphaned factory log must not remain queueable after its hash is dropped"
            );
            assert_eq!(runtime.latest_snapshot().registry().pool_count(), 0);

            worker.shutdown();
            runtime.shutdown().await?;
            Ok(())
        })
        .await
}

#[tokio::test(flavor = "multi_thread")]
async fn reorg_cancels_factory_hydration_supported_only_by_the_dropped_log() -> Result<()> {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut cache = setup_cache().await;
            align_cache(&mut cache, 500);
            let factory = Address::repeat_byte(0xc2);
            let token0 = Address::repeat_byte(0xc3);
            let token1 = Address::repeat_byte(0xc4);
            let pool = Address::repeat_byte(0xc5);
            let mut registry = AdapterRegistry::new();
            registry.register_adapter(Arc::new(UniswapV2Adapter::default()))?;
            let discovery = Arc::new(PoolDiscovery::for_registry(
                &registry,
                FactoryConfig::default().with_uniswap_v2_factory(factory),
            ));
            let runtime = AmmRuntime::spawn(
                cache,
                registry,
                runtime_baseline(500),
                AmmRuntimeConfig::default(),
            )?;
            let adapter = runtime
                .latest_snapshot()
                .registry()
                .adapters()
                .next()
                .expect("V2 adapter generation")
                .1
                .clone();
            runtime
                .add_factory_watcher(AmmFactoryWatcherRegistration::new(
                    DiscoveryOwnerKey::new("orphaned-scheduled-factory-event"),
                    adapter,
                    discovery,
                ))
                .await?;

            let assertions = Asserter::new();
            assertions.push_success(&encoded_words([
                U256::from_be_slice(token0.as_slice()),
                U256::from_be_slice(token1.as_slice()),
                U256::from(77) | (U256::from(88) << 112),
            ]));
            assertions.push_success(&v2_account_proof(pool));
            let transport = GatedMockTransport::new(assertions);
            let provider = RootProvider::<AnyNetwork>::new(RpcClient::new(transport.clone(), true));
            let worker = runtime
                .attach_cold_start_worker(provider, AmmColdStartWorkerConfig::default())
                .await?;

            let address_topic = |address: Address| {
                let mut word = [0_u8; 32];
                word[12..].copy_from_slice(address.as_slice());
                B256::from(word)
            };
            let mut data = [0_u8; 64];
            data[12..32].copy_from_slice(pool.as_slice());
            data[63] = 1;
            let log = PrimitiveLog::new_unchecked(
                factory,
                vec![
                    keccak256("PairCreated(address,address,address,uint256)"),
                    address_topic(token0),
                    address_topic(token1),
                ],
                Bytes::copy_from_slice(&data),
            );
            runtime
                .ingest_batch(canonical_primitive_log_batch(
                    501,
                    runtime.interest_revision(),
                    log,
                ))
                .await?;
            assert_eq!(runtime.latest_status().active_work_items().count(), 1);

            let mut replacement_header = canonical_header(501).inner;
            replacement_header.extra_data = Bytes::from_static(b"orphan-scheduled-factory-log");
            runtime
                .ingest_batch(AmmCanonicalBatch::from_verified_block(
                    1,
                    RpcHeader::new(replacement_header),
                    runtime.interest_revision(),
                    ReactiveInputBatch::new(Vec::new()),
                )?)
                .await?;

            assert_eq!(
                runtime.latest_status().active_work_items().count(),
                0,
                "work whose only provenance was dropped must be cancelled at the reorg fence"
            );
            assert_eq!(runtime.latest_snapshot().registry().pool_count(), 0);

            worker.shutdown();
            runtime.shutdown().await?;
            Ok(())
        })
        .await
}

#[tokio::test(flavor = "multi_thread")]
async fn reorg_keeps_scheduled_hydration_with_independent_stable_evidence() -> Result<()> {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut cache = setup_cache().await;
            align_cache(&mut cache, 500);
            let factory = Address::repeat_byte(0xb2);
            let token0 = Address::repeat_byte(0xb3);
            let token1 = Address::repeat_byte(0xb4);
            let pool = Address::repeat_byte(0xb5);
            let mut registry = AdapterRegistry::new();
            registry.register_adapter(Arc::new(UniswapV2Adapter::default()))?;
            let discovery = Arc::new(PoolDiscovery::for_registry(
                &registry,
                FactoryConfig::default().with_uniswap_v2_factory(factory),
            ));
            let runtime = AmmRuntime::spawn(
                cache,
                registry,
                runtime_baseline(500),
                AmmRuntimeConfig::default(),
            )?;
            let adapter = runtime
                .latest_snapshot()
                .registry()
                .adapters()
                .next()
                .expect("V2 adapter generation")
                .1
                .clone();
            runtime
                .add_factory_watcher(AmmFactoryWatcherRegistration::new(
                    DiscoveryOwnerKey::new("independently-supported-factory-event"),
                    adapter,
                    discovery,
                ))
                .await?;

            let assertions = Asserter::new();
            assertions.push_success(&encoded_words([
                U256::from_be_slice(token0.as_slice()),
                U256::from_be_slice(token1.as_slice()),
                U256::from(77) | (U256::from(88) << 112),
            ]));
            assertions.push_success(&v2_account_proof(pool));
            let transport = GatedMockTransport::new(assertions);
            let provider = RootProvider::<AnyNetwork>::new(RpcClient::new(transport.clone(), true));
            let worker = runtime
                .attach_cold_start_worker(provider, AmmColdStartWorkerConfig::default())
                .await?;
            runtime
                .queue_cold_start(
                    vec![complete_v2_registration(pool)],
                    AmmColdStartOptions::default(),
                )
                .await?;

            let address_topic = |address: Address| {
                let mut word = [0_u8; 32];
                word[12..].copy_from_slice(address.as_slice());
                B256::from(word)
            };
            let mut data = [0_u8; 64];
            data[12..32].copy_from_slice(pool.as_slice());
            data[63] = 1;
            let log = PrimitiveLog::new_unchecked(
                factory,
                vec![
                    keccak256("PairCreated(address,address,address,uint256)"),
                    address_topic(token0),
                    address_topic(token1),
                ],
                Bytes::copy_from_slice(&data),
            );
            runtime
                .ingest_batch(canonical_primitive_log_batch(
                    501,
                    runtime.interest_revision(),
                    log,
                ))
                .await?;
            assert_eq!(runtime.latest_status().active_work_items().count(), 1);

            let mut replacement_header = canonical_header(501).inner;
            replacement_header.extra_data = Bytes::from_static(b"retain-independent-support");
            runtime
                .ingest_batch(AmmCanonicalBatch::from_verified_block(
                    1,
                    RpcHeader::new(replacement_header),
                    runtime.interest_revision(),
                    ReactiveInputBatch::new(Vec::new()),
                )?)
                .await?;

            assert_eq!(
                runtime.latest_status().active_work_items().count(),
                1,
                "dropping one factory log cannot cancel independently supported work"
            );

            worker.shutdown();
            runtime.shutdown().await?;
            Ok(())
        })
        .await
}

#[tokio::test(flavor = "multi_thread")]
async fn hash_pinned_prepared_state_applies_fetched_values_with_pool_publication() -> Result<()> {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut cache = setup_cache().await;
            align_cache(&mut cache, 500);
            let target = StateSlot::new(Address::repeat_byte(0x81), U256::from(9));
            let adapter = Arc::new(TestWriteAdapter {
                protocol: "runtime-progressive",
                emitter: Address::repeat_byte(0x82),
                topic: B256::repeat_byte(0x83),
                target,
            });
            let mut registry = AdapterRegistry::new();
            registry.register_adapter(adapter)?;
            let runtime = AmmRuntime::spawn(
                cache,
                registry,
                runtime_baseline(500),
                AmmRuntimeConfig::default(),
            )?;
            let baseline = runtime.latest_snapshot().point();
            let registration =
                custom_registration("runtime-progressive", Address::repeat_byte(0x81));
            let prepared = AmmPreparedPoolState::new(
                registration,
                baseline,
                [AmmPreparedStorage::new(
                    target.address(),
                    target.slot(),
                    U256::from(777),
                )],
            )?;

            let changes = runtime.commit_prepared_pool(prepared).await?;
            let instance = changes.pool_changes()[0].pool();
            let snapshot = runtime.latest_snapshot();
            assert_eq!(
                snapshot.registry().pool(instance).unwrap().status,
                PoolStatus::Ready
            );
            assert_eq!(
                snapshot
                    .cache()
                    .storage_value(target.address(), target.slot()),
                Some(U256::from(777))
            );
            assert_eq!(snapshot.point(), baseline);
            runtime.shutdown().await?;
            Ok(())
        })
        .await
}

#[tokio::test(flavor = "multi_thread")]
async fn prepared_state_from_a_replaced_hash_is_rejected_without_mutation() -> Result<()> {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut cache = setup_cache().await;
            align_cache(&mut cache, 500);
            let target = StateSlot::new(Address::repeat_byte(0x84), U256::from(10));
            let adapter = Arc::new(TestWriteAdapter {
                protocol: "runtime-stale-progressive",
                emitter: Address::repeat_byte(0x85),
                topic: B256::repeat_byte(0x86),
                target,
            });
            let mut registry = AdapterRegistry::new();
            registry.register_adapter(adapter)?;
            let runtime = AmmRuntime::spawn(
                cache,
                registry,
                runtime_baseline(500),
                AmmRuntimeConfig::default(),
            )?;
            runtime.ingest_batch(empty_canonical_batch(501)).await?;
            let dropped = runtime.latest_snapshot().point();
            let prepared = AmmPreparedPoolState::new(
                custom_registration("runtime-stale-progressive", Address::repeat_byte(0x84)),
                dropped,
                [AmmPreparedStorage::new(
                    target.address(),
                    target.slot(),
                    U256::from(888),
                )],
            )?;
            runtime
                .ingest_batch(alternate_empty_canonical_batch(501))
                .await?;
            let replacement = runtime.latest_snapshot();

            assert!(matches!(
                runtime.commit_prepared_pool(prepared).await,
                Err(evm_amm_state::adapters::AmmRuntimeCommandError::StaleBaseline { .. })
            ));
            assert_eq!(runtime.latest_snapshot().version(), replacement.version());
            assert_eq!(runtime.latest_snapshot().registry().pool_count(), 0);
            assert_eq!(
                runtime
                    .latest_snapshot()
                    .cache()
                    .storage_value(target.address(), target.slot()),
                None
            );
            runtime.shutdown().await?;
            Ok(())
        })
        .await
}

#[tokio::test(flavor = "multi_thread")]
async fn queued_one_shot_cold_start_returns_immediately_and_publishes_in_background() -> Result<()>
{
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut cache = setup_cache().await;
            align_cache(&mut cache, 500);
            let mut registry = AdapterRegistry::new();
            registry.register_adapter(Arc::new(UniswapV2Adapter::default()))?;
            let runtime = AmmRuntime::spawn(
                cache,
                registry,
                runtime_baseline(500),
                AmmRuntimeConfig::default(),
            )?;
            let pool = Address::repeat_byte(0x91);
            let provider_assertions = Asserter::new();
            provider_assertions.push_success(&encoded_words([
                U256::from_be_slice(Address::repeat_byte(0xa2).as_slice()),
                U256::from_be_slice(Address::repeat_byte(0xa3).as_slice()),
                U256::from(11) | (U256::from(22) << 112),
            ]));
            provider_assertions.push_success(&v2_account_proof(pool));
            let transport = GatedMockTransport::new(provider_assertions);
            let provider = RootProvider::<AnyNetwork>::new(RpcClient::new(transport.clone(), true));
            let _worker = runtime
                .attach_cold_start_worker(provider, AmmColdStartWorkerConfig::default())
                .await?;
            let mut snapshots = runtime.subscribe_snapshots();
            let mut observer = runtime.subscribe_events();

            let queued = runtime
                .queue_cold_start(
                    vec![complete_v2_registration(pool)],
                    AmmColdStartOptions::default(),
                )
                .await?;
            assert_eq!(queued.len(), 1);
            assert_eq!(runtime.latest_snapshot().registry().pool_count(), 0);
            transport.release_one();

            let published = tokio::time::timeout(std::time::Duration::from_millis(250), async {
                while snapshots.borrow().registry().pool_count() == 0 {
                    snapshots.changed().await.expect("runtime remains alive");
                }
            })
            .await;
            if published.is_err() {
                let mut seen = Vec::new();
                while let Ok(Ok(event)) =
                    tokio::time::timeout(std::time::Duration::from_millis(1), observer.next_event())
                        .await
                {
                    seen.push(format!("{:?}", event.kind()));
                }
                anyhow::bail!("pool was not published; events: {seen:?}");
            }
            let snapshot = runtime.latest_snapshot();
            let instance = snapshot
                .registry()
                .pool_instance(&PoolKey::UniswapV2(pool))
                .expect("pool published independently");
            assert_eq!(instance, queued[0].pool());
            assert_eq!(
                snapshot.cache().storage_value(pool, V2_TOKEN0_SLOT),
                Some(U256::from_be_slice(Address::repeat_byte(0xa2).as_slice()))
            );
            assert_eq!(
                snapshot.cache().storage_value(pool, V2_TOKEN1_SLOT),
                Some(U256::from_be_slice(Address::repeat_byte(0xa3).as_slice()))
            );
            assert_eq!(
                snapshot.cache().storage_value(pool, V2_RESERVES_SLOT),
                Some(U256::from(11) | (U256::from(22) << 112))
            );
            let published = snapshot
                .registry()
                .pool(instance)
                .expect("published registration");
            let ProtocolMetadata::UniswapV2(metadata) = &published.metadata else {
                panic!("published V2 metadata");
            };
            assert_eq!(metadata.token0, Some(Address::repeat_byte(0xa2)));
            assert_eq!(metadata.token1, Some(Address::repeat_byte(0xa3)));
            assert_eq!(metadata.fee_bps, Some(30));
            let account = EvmOverlay::new(snapshot.cache_snapshot(), None)
                .basic(pool)?
                .expect("verified pair code is published with the pool");
            assert_eq!(account.code_hash, uniswap_v2_pair_runtime_code_hash());
            assert!(account.code.is_some_and(|code| !code.is_empty()));

            let mut events = Vec::new();
            tokio::time::timeout(std::time::Duration::from_millis(250), async {
                loop {
                    let event = observer.next_event().await.expect("observer remains open");
                    let terminal = matches!(
                        event.kind(),
                        AmmRuntimeEventKind::WorkCompleted { work }
                            if work == queued[0].work()
                    );
                    events.push(event);
                    if terminal {
                        break;
                    }
                }
            })
            .await?;
            assert!(
                events.windows(2).all(|pair| {
                    pair[1].sequence() == pair[0].sequence().checked_add(1).unwrap()
                })
            );
            let position = |predicate: fn(&AmmRuntimeEventKind) -> bool| {
                events
                    .iter()
                    .position(|event| predicate(event.kind()))
                    .expect("expected progressive cold-start event")
            };
            let queued_at = position(|kind| matches!(kind, AmmRuntimeEventKind::WorkQueued { .. }));
            let started_at =
                position(|kind| matches!(kind, AmmRuntimeEventKind::ColdStartRoundStarted { .. }));
            let committed_at =
                position(|kind| matches!(kind, AmmRuntimeEventKind::StateCommitted { .. }));
            let completed_at =
                position(|kind| matches!(kind, AmmRuntimeEventKind::WorkCompleted { .. }));
            assert!(queued_at < started_at);
            assert!(started_at < committed_at);
            assert!(committed_at < completed_at);
            let status = runtime.latest_status();
            assert_eq!(
                status.pool_state(instance),
                Some(evm_amm_state::adapters::PoolRuntimeState::Searchable)
            );
            assert!(status.active_work(queued[0].work()).is_none());
            assert_eq!(
                status.queue_depth(evm_amm_state::adapters::AmmWorkClass::Bootstrap),
                0
            );
            runtime.shutdown().await?;
            Ok(())
        })
        .await
}

#[tokio::test(flavor = "multi_thread")]
async fn resumable_fallback_merges_incomplete_v2_metadata_and_publishes() -> Result<()> {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut cache = setup_cache().await;
            align_cache(&mut cache, 500);
            let mut registry = AdapterRegistry::new();
            registry.register_adapter(Arc::new(UniswapV2Adapter::default()))?;
            let runtime = AmmRuntime::spawn(
                cache,
                registry,
                runtime_baseline(500),
                AmmRuntimeConfig::default(),
            )?;
            let pool = Address::repeat_byte(0x94);
            let token0 = Address::repeat_byte(0xb0);
            let token1 = Address::repeat_byte(0xb1);
            let assertions = Asserter::new();
            assertions.push_success(&v2_account_proof(pool));
            assertions.push_success(&encoded_words([
                U256::from_be_slice(token0.as_slice()),
                U256::from_be_slice(token1.as_slice()),
                U256::from(77) | (U256::from(88) << 112),
            ]));
            let provider = RootProvider::<AnyNetwork>::new(RpcClient::mocked(assertions));
            let _worker = runtime
                .attach_cold_start_worker(provider, AmmColdStartWorkerConfig::default())
                .await?;
            let mut snapshots = runtime.subscribe_snapshots();
            let registration = PoolRegistration::new(PoolKey::UniswapV2(pool)).with_metadata(
                ProtocolMetadata::UniswapV2(UniswapV2Metadata::default().with_fee_bps(25)),
            );
            let scheduled = runtime
                .queue_cold_start(vec![registration], AmmColdStartOptions::default())
                .await?;

            tokio::time::timeout(std::time::Duration::from_millis(250), async {
                while snapshots.borrow().registry().pool_count() == 0 {
                    snapshots.changed().await.expect("runtime remains alive");
                }
            })
            .await?;
            let snapshot = runtime.latest_snapshot();
            let published = snapshot
                .registry()
                .pool(scheduled[0].pool())
                .expect("fallback pool published");
            let ProtocolMetadata::UniswapV2(metadata) = &published.metadata else {
                panic!("published V2 metadata");
            };
            assert_eq!(metadata.token0, Some(token0));
            assert_eq!(metadata.token1, Some(token1));
            assert_eq!(metadata.fee_bps, Some(25));
            assert_eq!(
                snapshot.cache().storage_value(pool, V2_RESERVES_SLOT),
                Some(U256::from(77) | (U256::from(88) << 112))
            );
            runtime.shutdown().await?;
            Ok(())
        })
        .await
}

#[tokio::test(flavor = "multi_thread")]
async fn blocked_cold_start_does_not_block_canonical_ingest_and_stale_result_fails() -> Result<()> {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut cache = setup_cache().await;
            align_cache(&mut cache, 500);
            let mut registry = AdapterRegistry::new();
            registry.register_adapter(Arc::new(UniswapV2Adapter::default()))?;
            let runtime = AmmRuntime::spawn(
                cache,
                registry,
                runtime_baseline(500),
                AmmRuntimeConfig::default(),
            )?;
            let pool = Address::repeat_byte(0x92);
            let assertions = Asserter::new();
            assertions.push_success(&encoded_words([
                U256::from_be_slice(Address::repeat_byte(0xa0).as_slice()),
                U256::from_be_slice(Address::repeat_byte(0xa1).as_slice()),
                U256::from(33) | (U256::from(44) << 112),
            ]));
            assertions.push_success(&v2_account_proof(pool));
            let transport = GatedMockTransport::new(assertions.clone());
            let provider = RootProvider::<AnyNetwork>::new(RpcClient::new(transport.clone(), true));
            let worker = runtime
                .attach_cold_start_worker(
                    provider,
                    AmmColdStartWorkerConfig::default().with_max_concurrency(1),
                )
                .await?;
            let queued = runtime
                .queue_cold_start(
                    vec![complete_v2_registration(pool)],
                    AmmColdStartOptions::default(),
                )
                .await?;

            tokio::time::timeout(std::time::Duration::from_millis(250), async {
                while !transport
                    .requests()
                    .iter()
                    .any(|(method, _)| method == "eth_call")
                {
                    tokio::task::yield_now().await;
                }
            })
            .await?;
            tokio::time::timeout(
                std::time::Duration::from_millis(50),
                runtime.ingest_batch(empty_canonical_batch(501)),
            )
            .await??;
            assert_eq!(runtime.latest_snapshot().point().block_number(), 501);

            transport.release_one();
            let mut status = runtime.subscribe_status();
            tokio::time::timeout(std::time::Duration::from_millis(250), async {
                while status.borrow().pool_state(queued[0].pool())
                    != Some(evm_amm_state::adapters::PoolRuntimeState::Failed)
                {
                    status.changed().await.expect("runtime remains alive");
                }
            })
            .await?;
            assert_eq!(runtime.latest_snapshot().registry().pool_count(), 0);
            assert!(
                runtime
                    .latest_status()
                    .active_work(queued[0].work())
                    .is_none()
            );
            let call = transport
                .requests()
                .into_iter()
                .find(|(method, _)| method == "eth_call")
                .expect("hash-pinned storage request");
            assert!(call.1.contains(&format!("{:#x}", block_hash(500))));
            assert!(call.1.contains("\"requireCanonical\":true"));
            let proof = transport
                .requests()
                .into_iter()
                .find(|(method, _)| method == "eth_getProof")
                .expect("hash-pinned account proof request");
            assert!(proof.1.contains(&format!("{:#x}", block_hash(500))));
            assert!(proof.1.contains("\"requireCanonical\":true"));

            assertions.push_success(&encoded_words([
                U256::from_be_slice(Address::repeat_byte(0xa0).as_slice()),
                U256::from_be_slice(Address::repeat_byte(0xa1).as_slice()),
                U256::from(33) | (U256::from(44) << 112),
            ]));
            assertions.push_success(&v2_account_proof(pool));
            let retry = runtime
                .queue_cold_start(
                    vec![complete_v2_registration(pool)],
                    AmmColdStartOptions::default(),
                )
                .await?;
            assert_eq!(retry[0].pool().generation(), PoolGeneration::new(1));
            transport.release_one();
            let mut snapshots = runtime.subscribe_snapshots();
            tokio::time::timeout(std::time::Duration::from_millis(250), async {
                while snapshots.borrow().registry().pool_count() == 0 {
                    snapshots.changed().await.expect("runtime remains alive");
                }
            })
            .await?;
            assert_eq!(
                runtime
                    .latest_snapshot()
                    .registry()
                    .pool_instance(&PoolKey::UniswapV2(pool)),
                Some(retry[0].pool())
            );
            let requests = transport.requests();
            assert!(requests.iter().any(|(method, params)| {
                method == "eth_call" && params.contains(&format!("{:#x}", block_hash(501)))
            }));
            assert!(requests.iter().any(|(method, params)| {
                method == "eth_getProof" && params.contains(&format!("{:#x}", block_hash(501)))
            }));

            worker.shutdown();
            runtime.shutdown().await?;
            Ok(())
        })
        .await
}

#[tokio::test(flavor = "multi_thread")]
async fn live_batch_round_trip_p99_stays_below_gate_during_blocked_bootstrap() -> Result<()> {
    const WARMUP_SAMPLES: u64 = 32;
    const MEASURED_SAMPLES: u64 = 1_000;

    tokio::task::LocalSet::new()
        .run_until(async {
            let mut cache = setup_cache().await;
            align_cache(&mut cache, 500);
            let mut registry = AdapterRegistry::new();
            registry.register_adapter(Arc::new(UniswapV2Adapter::default()))?;
            let runtime = AmmRuntime::spawn(
                cache,
                registry,
                runtime_baseline(500),
                AmmRuntimeConfig::default(),
            )?;
            let pool = Address::repeat_byte(0x94);
            let assertions = Asserter::new();
            assertions.push_success(&encoded_words([
                U256::from_be_slice(Address::repeat_byte(0xa0).as_slice()),
                U256::from_be_slice(Address::repeat_byte(0xa1).as_slice()),
                U256::from(33) | (U256::from(44) << 112),
            ]));
            assertions.push_success(&v2_account_proof(pool));
            let transport = GatedMockTransport::new(assertions);
            let provider = RootProvider::<AnyNetwork>::new(RpcClient::new(transport.clone(), true));
            let worker = runtime
                .attach_cold_start_worker(
                    provider,
                    AmmColdStartWorkerConfig::default().with_max_concurrency(1),
                )
                .await?;
            let queued = runtime
                .queue_cold_start(
                    vec![complete_v2_registration(pool)],
                    AmmColdStartOptions::default(),
                )
                .await?;
            tokio::time::timeout(std::time::Duration::from_millis(250), async {
                while !transport
                    .requests()
                    .iter()
                    .any(|(method, _)| method == "eth_call")
                {
                    tokio::task::yield_now().await;
                }
            })
            .await?;
            assert!(
                runtime
                    .latest_status()
                    .active_work(queued[0].work())
                    .is_some(),
                "bootstrap provider work must remain blocked while canonical latency is sampled"
            );

            let mut header = canonical_header(500);
            for _ in 0..WARMUP_SAMPLES {
                header = canonical_child_header(&header);
                let batch = empty_canonical_batch_from_header(header.clone());
                runtime.ingest_batch(batch).await?;
            }

            let mut samples = Vec::with_capacity(MEASURED_SAMPLES as usize);
            for _ in 0..MEASURED_SAMPLES {
                // Batch construction is intentionally outside the timer: the
                // frozen gate measures queue + actor round-trip, not fixture
                // header generation or the independently blocked provider.
                header = canonical_child_header(&header);
                let batch = empty_canonical_batch_from_header(header.clone());
                let started = std::time::Instant::now();
                runtime.ingest_batch(batch).await?;
                samples.push(started.elapsed());
            }
            samples.sort_unstable();
            let p99_index = ((samples.len() * 99).div_ceil(100)).saturating_sub(1);
            let p99 = samples[p99_index];
            eprintln!(
                "live batch queue + actor round-trip p99 during blocked bootstrap: {p99:?} ({} samples)",
                samples.len()
            );
            assert!(
                p99 < std::time::Duration::from_millis(50),
                "live batch queue delay exceeded the 50ms offline gate during blocked bootstrap: {p99:?}"
            );
            assert!(
                runtime
                    .latest_status()
                    .active_work(queued[0].work())
                    .is_some(),
                "the bootstrap RPC must still be blocked after the provider-free measurement"
            );

            transport.release_one();
            let mut status = runtime.subscribe_status();
            tokio::time::timeout(std::time::Duration::from_millis(250), async {
                while status.borrow().active_work(queued[0].work()).is_some() {
                    status.changed().await.expect("runtime remains alive");
                }
            })
            .await?;
            worker.shutdown();
            runtime.shutdown().await?;
            Ok(())
        })
        .await
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelled_cold_start_tombstones_generation_and_rejects_late_result() -> Result<()> {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut cache = setup_cache().await;
            align_cache(&mut cache, 500);
            let mut registry = AdapterRegistry::new();
            registry.register_adapter(Arc::new(UniswapV2Adapter::default()))?;
            let runtime = AmmRuntime::spawn(
                cache,
                registry,
                runtime_baseline(500),
                AmmRuntimeConfig::default(),
            )?;
            let pool = Address::repeat_byte(0x93);
            let assertions = Asserter::new();
            assertions.push_success(&encoded_words([
                U256::from_be_slice(Address::repeat_byte(0xa0).as_slice()),
                U256::from_be_slice(Address::repeat_byte(0xa1).as_slice()),
                U256::from(55) | (U256::from(66) << 112),
            ]));
            assertions.push_success(&v2_account_proof(pool));
            let transport = GatedMockTransport::new(assertions);
            let provider = RootProvider::<AnyNetwork>::new(RpcClient::new(transport.clone(), true));
            let worker = runtime
                .attach_cold_start_worker(provider, AmmColdStartWorkerConfig::default())
                .await?;
            let first = runtime
                .queue_cold_start(
                    vec![complete_v2_registration(pool)],
                    AmmColdStartOptions::default(),
                )
                .await?;
            tokio::time::timeout(std::time::Duration::from_millis(250), async {
                while !transport
                    .requests()
                    .iter()
                    .any(|(method, _)| method == "eth_call")
                {
                    tokio::task::yield_now().await;
                }
            })
            .await?;

            runtime.cancel_work(first[0].work().clone()).await?;
            assert_eq!(
                runtime.latest_status().pool_state(first[0].pool()),
                Some(evm_amm_state::adapters::PoolRuntimeState::Removed)
            );
            transport.release_one();
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            assert_eq!(runtime.latest_snapshot().registry().pool_count(), 0);
            assert!(
                runtime
                    .latest_snapshot()
                    .cache()
                    .storage_value(pool, V2_RESERVES_SLOT)
                    .is_none()
            );

            worker.shutdown();
            runtime.shutdown().await?;
            Ok(())
        })
        .await
}

#[tokio::test(flavor = "multi_thread")]
async fn adapter_removal_cancels_pending_cold_start_before_replacement_generation() -> Result<()> {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut cache = setup_cache().await;
            align_cache(&mut cache, 500);
            let mut registry = AdapterRegistry::new();
            registry.register_adapter(Arc::new(UniswapV2Adapter::default()))?;
            let runtime = AmmRuntime::spawn(
                cache,
                registry,
                runtime_baseline(500),
                AmmRuntimeConfig::default(),
            )?;
            let old_adapter = runtime
                .latest_snapshot()
                .registry()
                .adapters()
                .next()
                .expect("V2 adapter generation")
                .1
                .clone();
            let pool = Address::repeat_byte(0x96);
            let assertions = Asserter::new();
            assertions.push_success(&encoded_words([
                U256::from_be_slice(Address::repeat_byte(0xa0).as_slice()),
                U256::from_be_slice(Address::repeat_byte(0xa1).as_slice()),
                U256::from(55) | (U256::from(66) << 112),
            ]));
            assertions.push_success(&v2_account_proof(pool));
            let transport = GatedMockTransport::new(assertions);
            let provider = RootProvider::<AnyNetwork>::new(RpcClient::new(transport.clone(), true));
            let worker = runtime
                .attach_cold_start_worker(provider, AmmColdStartWorkerConfig::default())
                .await?;
            let scheduled = runtime
                .queue_cold_start(
                    vec![complete_v2_registration(pool)],
                    AmmColdStartOptions::default(),
                )
                .await?;
            tokio::time::timeout(std::time::Duration::from_millis(250), async {
                while !transport
                    .requests()
                    .iter()
                    .any(|(method, _)| method == "eth_call")
                {
                    tokio::task::yield_now().await;
                }
            })
            .await?;

            runtime.remove_adapter(old_adapter.clone()).await?;
            let replacement = runtime
                .add_adapter(Arc::new(UniswapV2Adapter::default()))
                .await?;
            assert_ne!(old_adapter, replacement);
            assert!(
                runtime
                    .latest_status()
                    .active_work(scheduled[0].work())
                    .is_none()
            );

            transport.release_one();
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            assert_eq!(runtime.latest_snapshot().registry().pool_count(), 0);

            worker.shutdown();
            runtime.shutdown().await?;
            Ok(())
        })
        .await
}

#[tokio::test(flavor = "multi_thread")]
async fn deferred_slot_patch_streams_a_same_generation_state_commit() -> Result<()> {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut cache = setup_cache().await;
            align_cache(&mut cache, 500);
            let address = Address::repeat_byte(0x97);
            let target = StateSlot::new(address, U256::from(7));
            let adapter = Arc::new(TestWriteAdapter {
                protocol: "runtime-deferred",
                emitter: address,
                topic: B256::repeat_byte(0x98),
                target,
            });
            let registration =
                custom_registration("runtime-deferred", address).with_state_address(address);
            let key = registration.key.clone();
            let mut registry = AdapterRegistry::new();
            registry.register_adapter(adapter)?;
            registry.register_pool(registration)?;
            let runtime = AmmRuntime::spawn(
                cache,
                registry,
                runtime_baseline(500),
                AmmRuntimeConfig::default(),
            )?;
            let instance = runtime
                .latest_snapshot()
                .registry()
                .pool_instance(&key)
                .expect("pool generation is active")
                .clone();
            let assertions = Asserter::new();
            assertions.push_success(&encoded_words([U256::from(123)]));
            let provider = RootProvider::<AnyNetwork>::new(RpcClient::mocked(assertions));
            let worker = runtime
                .attach_cold_start_worker(
                    provider,
                    AmmColdStartWorkerConfig::default().with_queue_capacity(1),
                )
                .await?;
            let scheduled = runtime
                .queue_deferred(
                    instance.clone(),
                    vec![DeferredWork::VerifySlots(vec![(address, target.slot())])],
                )
                .await?
                .expect("non-empty deferred work is scheduled");

            let mut snapshots = runtime.subscribe_snapshots();
            tokio::time::timeout(std::time::Duration::from_millis(250), async {
                while snapshots.borrow().pool_revision(&instance)
                    == runtime.latest_snapshot().pool_revision(&instance)
                {
                    snapshots.changed().await.expect("runtime remains alive");
                    if snapshots
                        .borrow()
                        .cache()
                        .storage_value(address, target.slot())
                        == Some(U256::from(123))
                    {
                        break;
                    }
                }
            })
            .await?;
            let snapshot = runtime.latest_snapshot();
            assert_eq!(
                snapshot.cache().storage_value(address, target.slot()),
                Some(U256::from(123))
            );
            assert_eq!(snapshot.registry().pool_instance(&key), Some(&instance));
            assert!(
                runtime
                    .latest_status()
                    .active_work(scheduled.work())
                    .is_none()
            );

            worker.shutdown();
            runtime.shutdown().await?;
            Ok(())
        })
        .await
}

#[tokio::test(flavor = "multi_thread")]
async fn lazy_cold_start_publishes_then_runs_deferred_slots_at_capacity_one() -> Result<()> {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut cache = setup_cache().await;
            align_cache(&mut cache, 500);
            let mut registry = AdapterRegistry::new();
            registry.register_adapter(Arc::new(UniswapV2Adapter::default()))?;
            let runtime = AmmRuntime::spawn(
                cache,
                registry,
                runtime_baseline(500),
                AmmRuntimeConfig::default(),
            )?;
            let pool = Address::repeat_byte(0x9b);
            let token0 = Address::repeat_byte(0xc0);
            let token1 = Address::repeat_byte(0xc1);
            let reserves = U256::from(41) | (U256::from(42) << 112);
            let assertions = Asserter::new();
            assertions.push_failure_msg("force exact-hash storage fallback");
            assertions.push_success(&v2_account_proof(pool));
            assertions.push_success(&reserves);
            assertions.push_success(&encoded_words([
                U256::from_be_slice(token0.as_slice()),
                U256::from_be_slice(token1.as_slice()),
            ]));
            let transport = GatedMockTransport::new(assertions);
            transport.release_one();
            transport.release_one();
            let provider = RootProvider::<AnyNetwork>::new(RpcClient::new(transport.clone(), true));
            let worker = runtime
                .attach_cold_start_worker(
                    provider,
                    AmmColdStartWorkerConfig::default().with_queue_capacity(1),
                )
                .await?;
            let scheduled = runtime
                .queue_cold_start(
                    vec![complete_v2_registration(pool)],
                    AmmColdStartOptions::default().with_policy(ColdStartPolicy::Lazy),
                )
                .await?;

            let mut snapshots = runtime.subscribe_snapshots();
            let completed = tokio::time::timeout(std::time::Duration::from_millis(500), async {
                while snapshots
                    .borrow()
                    .cache()
                    .storage_value(pool, V2_TOKEN1_SLOT)
                    != Some(U256::from_be_slice(token1.as_slice()))
                {
                    snapshots.changed().await.expect("runtime remains alive");
                }
            })
            .await;
            assert!(completed.is_ok(), "requests: {:?}", transport.requests());
            let snapshot = runtime.latest_snapshot();
            assert_eq!(
                snapshot.registry().pool_instance(&PoolKey::UniswapV2(pool)),
                Some(scheduled[0].pool())
            );
            assert_eq!(
                snapshot.cache().storage_value(pool, V2_RESERVES_SLOT),
                Some(reserves)
            );
            assert_eq!(
                snapshot.cache().storage_value(pool, V2_TOKEN0_SLOT),
                Some(U256::from_be_slice(token0.as_slice()))
            );

            worker.shutdown();
            runtime.shutdown().await?;
            Ok(())
        })
        .await
}

#[tokio::test(flavor = "multi_thread")]
async fn mixed_deferred_refresh_chains_its_remaining_slot_patch() -> Result<()> {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut cache = setup_cache().await;
            align_cache(&mut cache, 500);
            let pool = Address::repeat_byte(0x9c);
            let token0 = Address::repeat_byte(0xc2);
            let token1 = Address::repeat_byte(0xc3);
            let mut registry = AdapterRegistry::new();
            registry.register_adapter(Arc::new(UniswapV2Adapter::default()))?;
            registry.register_pool(complete_v2_registration(pool))?;
            let runtime = AmmRuntime::spawn(
                cache,
                registry,
                runtime_baseline(500),
                AmmRuntimeConfig::default(),
            )?;
            let instance = runtime
                .latest_snapshot()
                .registry()
                .pool_instance(&PoolKey::UniswapV2(pool))
                .expect("V2 pool generation")
                .clone();
            let assertions = Asserter::new();
            assertions.push_success(&encoded_words([
                U256::from_be_slice(token0.as_slice()),
                U256::from_be_slice(token1.as_slice()),
                U256::from(51) | (U256::from(52) << 112),
            ]));
            assertions.push_success(&v2_account_proof(pool));
            let patched_token0 = U256::from(999);
            assertions.push_success(&encoded_words([patched_token0]));
            let provider = RootProvider::<AnyNetwork>::new(RpcClient::mocked(assertions));
            let worker = runtime
                .attach_cold_start_worker(
                    provider,
                    AmmColdStartWorkerConfig::default().with_queue_capacity(1),
                )
                .await?;
            runtime
                .queue_deferred(
                    instance.clone(),
                    vec![
                        DeferredWork::ColdStart {
                            pool: instance.key().clone(),
                            policy: ColdStartPolicy::Eager,
                        },
                        DeferredWork::VerifySlots(vec![(pool, V2_TOKEN0_SLOT)]),
                    ],
                )
                .await?;

            let mut snapshots = runtime.subscribe_snapshots();
            tokio::time::timeout(std::time::Duration::from_millis(500), async {
                while snapshots.borrow().pool_revision(&instance)
                    != Some(evm_amm_state::adapters::PoolStateRevision::new(2))
                {
                    snapshots.changed().await.expect("runtime remains alive");
                }
            })
            .await?;
            assert_eq!(
                runtime
                    .latest_snapshot()
                    .registry()
                    .pool_instance(instance.key()),
                Some(&instance)
            );
            assert_eq!(
                runtime
                    .latest_snapshot()
                    .cache()
                    .storage_value(pool, V2_TOKEN0_SLOT),
                Some(patched_token0)
            );

            worker.shutdown();
            runtime.shutdown().await?;
            Ok(())
        })
        .await
}

#[tokio::test(flavor = "multi_thread")]
async fn canonical_repair_is_automatically_scheduled_and_recovers_same_generation() -> Result<()> {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut cache = setup_cache().await;
            align_cache(&mut cache, 500);
            let address = Address::repeat_byte(0x99);
            let target = StateSlot::new(address, U256::from(9));
            let topic = B256::repeat_byte(0x9a);
            let adapter = Arc::new(TestRepairAdapter {
                protocol: "runtime-auto-repair",
                emitter: address,
                topic,
                target,
            });
            let registration =
                custom_registration("runtime-auto-repair", address).with_state_address(address);
            let key = registration.key.clone();
            let mut registry = AdapterRegistry::new();
            registry.register_adapter(adapter)?;
            registry.register_pool(registration)?;
            let runtime = AmmRuntime::spawn(
                cache,
                registry,
                runtime_baseline(500),
                AmmRuntimeConfig::default(),
            )?;
            let instance = runtime
                .latest_snapshot()
                .registry()
                .pool_instance(&key)
                .expect("pool generation is active")
                .clone();
            let assertions = Asserter::new();
            assertions.push_success(&encoded_words([U256::from(321)]));
            let transport = GatedMockTransport::new(assertions);
            let provider = RootProvider::<AnyNetwork>::new(RpcClient::new(transport.clone(), true));
            let worker = runtime
                .attach_cold_start_worker(
                    provider,
                    AmmColdStartWorkerConfig::default().with_queue_capacity(1),
                )
                .await?;

            let log = PrimitiveLog::new_unchecked(address, vec![topic], Bytes::new());
            let degraded = runtime
                .ingest_batch(canonical_primitive_log_batch(
                    501,
                    runtime.interest_revision(),
                    log,
                ))
                .await?;
            assert!(
                degraded
                    .pool_changes()
                    .iter()
                    .any(|change| change.pool() == &instance
                        && change.kind() == AmmPoolChangeKind::Degraded)
            );
            assert_eq!(
                runtime
                    .latest_snapshot()
                    .registry()
                    .pool(&instance)
                    .expect("degraded generation remains registered")
                    .status,
                PoolStatus::Degraded
            );
            transport.release_one();

            let mut snapshots = runtime.subscribe_snapshots();
            tokio::time::timeout(std::time::Duration::from_millis(250), async {
                while snapshots
                    .borrow()
                    .cache()
                    .storage_value(address, target.slot())
                    != Some(U256::from(321))
                {
                    snapshots.changed().await.expect("runtime remains alive");
                }
            })
            .await?;
            let snapshot = runtime.latest_snapshot();
            assert_eq!(snapshot.registry().pool_instance(&key), Some(&instance));
            assert_eq!(
                runtime.latest_status().pool_state(&instance),
                Some(evm_amm_state::adapters::PoolRuntimeState::Searchable)
            );

            worker.shutdown();
            runtime.shutdown().await?;
            Ok(())
        })
        .await
}

#[tokio::test(flavor = "multi_thread")]
async fn ready_metadata_without_its_declared_state_is_not_publishable() -> Result<()> {
    tokio::task::LocalSet::new()
        .run_until(async {
            let point = AmmStatePoint::post_block(1, 500, block_hash(500));
            let mut cache = setup_cache().await;
            align_cache(&mut cache, 500);
            let adapter = Arc::new(TestWriteAdapter {
                protocol: "runtime-unprepared",
                emitter: Address::repeat_byte(0x72),
                topic: B256::repeat_byte(0x73),
                target: StateSlot::new(Address::repeat_byte(0x71), U256::from(1)),
            });
            let mut registry = AdapterRegistry::new();
            registry.register_adapter(adapter)?;
            let runtime = AmmRuntime::spawn(
                cache,
                registry,
                runtime_baseline(500),
                AmmRuntimeConfig::default(),
            )?;

            assert!(matches!(
                runtime
                    .install_prepared_pools(
                        vec![custom_registration(
                            "runtime-unprepared",
                            Address::repeat_byte(0x71)
                        )],
                        point
                    )
                    .await,
                Err(evm_amm_state::adapters::AmmRuntimeCommandError::MissingPreparedState { .. })
            ));
            assert_eq!(
                runtime.latest_snapshot().version(),
                AmmStateVersion::initial()
            );
            runtime.shutdown().await?;
            Ok(())
        })
        .await
}

#[tokio::test(flavor = "multi_thread")]
async fn try_submit_is_immediate_and_reports_a_saturated_canonical_queue() -> Result<()> {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut cache = setup_cache().await;
            align_cache(&mut cache, 500);
            let runtime = AmmRuntime::spawn(
                cache,
                AdapterRegistry::new(),
                runtime_baseline(500),
                AmmRuntimeConfig::default()
                    .with_command_capacity(1)
                    .with_canonical_input_capacity(1)
                    .with_critical_change_capacity(1),
            )?;
            let _stalled_consumer = runtime.subscribe_changes().await?;
            runtime.ingest_batch(empty_canonical_batch(501)).await?;
            let blocked_handle = runtime.clone();
            let blocked = tokio::task::spawn_local(async move {
                blocked_handle
                    .ingest_batch(empty_canonical_batch(502))
                    .await
            });
            tokio::task::yield_now().await;

            let accepted = runtime.try_ingest_batch(empty_canonical_batch(503))?;
            assert_eq!(accepted.id().get(), 0);
            assert!(matches!(
                runtime.try_ingest_batch(empty_canonical_batch(504)),
                Err(AmmRuntimeSubmitError::Full)
            ));

            runtime.shutdown().await?;
            assert!(matches!(
                accepted.wait().await,
                Err(evm_amm_state::adapters::AmmRuntimeCommandError::Closed)
            ));
            assert!(matches!(
                blocked.await?,
                Err(evm_amm_state::adapters::AmmRuntimeCommandError::Closed)
            ));
            Ok(())
        })
        .await
}

#[tokio::test(flavor = "multi_thread")]
async fn lifecycle_try_submit_reports_control_backpressure_without_waiting() -> Result<()> {
    tokio::task::LocalSet::new()
        .run_until(async {
            let point = AmmStatePoint::post_block(1, 500, block_hash(500));
            let mut cache = setup_cache().await;
            align_cache(&mut cache, 500);
            let runtime = AmmRuntime::spawn(
                cache,
                AdapterRegistry::new(),
                runtime_baseline(500),
                AmmRuntimeConfig::default()
                    .with_command_capacity(1)
                    .with_critical_change_capacity(1),
            )?;
            let _stalled_consumer = runtime.subscribe_changes().await?;
            runtime.ingest_batch(empty_canonical_batch(501)).await?;
            let blocked_handle = runtime.clone();
            let blocked = tokio::task::spawn_local(async move {
                blocked_handle
                    .ingest_batch(empty_canonical_batch(502))
                    .await
            });
            tokio::task::yield_now().await;

            let accepted = runtime.try_install_prepared_pools(Vec::new(), point)?;
            assert_eq!(accepted.id().get(), 0);
            let missing = PoolInstanceId::new(
                PoolKey::SolidlyV2(Address::repeat_byte(0xfe)),
                PoolGeneration::new(0),
            );
            assert!(matches!(
                runtime.try_remove_pool(missing, AmmEvictionPolicy::Retain),
                Err(AmmRuntimeSubmitError::Full)
            ));

            runtime.shutdown().await?;
            assert!(matches!(
                accepted.wait().await,
                Err(evm_amm_state::adapters::AmmRuntimeCommandError::Closed)
            ));
            assert!(matches!(
                blocked.await?,
                Err(evm_amm_state::adapters::AmmRuntimeCommandError::Closed)
            ));
            Ok(())
        })
        .await
}

#[tokio::test(flavor = "multi_thread")]
async fn control_command_enqueue_p99_stays_below_the_offline_gate() -> Result<()> {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut cache = setup_cache().await;
            align_cache(&mut cache, 500);
            let runtime = AmmRuntime::spawn(
                cache,
                AdapterRegistry::new(),
                runtime_baseline(500),
                AmmRuntimeConfig::default(),
            )?;
            let point = runtime.latest_snapshot().point();
            let mut samples = Vec::with_capacity(10_000);
            for _ in 0..10_000 {
                let started = std::time::Instant::now();
                let ticket = runtime.try_install_prepared_pools(Vec::new(), point)?;
                samples.push(started.elapsed());
                ticket.wait().await?;
            }
            samples.sort_unstable();
            let p99 = samples[9_899];
            eprintln!("control command enqueue p99: {p99:?}");
            assert!(
                p99 < std::time::Duration::from_millis(1),
                "control command enqueue p99 exceeded the 1ms offline gate: {p99:?}"
            );
            runtime.shutdown().await?;
            Ok(())
        })
        .await
}

#[tokio::test(flavor = "multi_thread")]
async fn control_work_is_not_starved_by_a_saturated_canonical_queue() -> Result<()> {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut cache = setup_cache().await;
            align_cache(&mut cache, 500);
            let runtime = AmmRuntime::spawn(
                cache,
                AdapterRegistry::new(),
                runtime_baseline(500),
                AmmRuntimeConfig::default().with_canonical_input_capacity(128),
            )?;

            let mut tickets = Vec::new();
            for block in 501..=600 {
                tickets.push(runtime.try_ingest_batch(empty_canonical_batch(block))?);
            }

            let subscription = runtime.subscribe_changes().await?;
            assert!(
                subscription.snapshot().version().get() < 100,
                "bounded scheduling must service control before draining the live backlog"
            );
            drop(subscription);

            for ticket in tickets {
                ticket.wait().await?;
            }
            assert_eq!(
                runtime.latest_snapshot().version(),
                AmmStateVersion::new(100)
            );
            runtime.shutdown().await?;
            Ok(())
        })
        .await
}

#[tokio::test(flavor = "multi_thread")]
async fn canonical_work_is_not_starved_by_a_saturated_control_queue() -> Result<()> {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut cache = setup_cache().await;
            align_cache(&mut cache, 500);
            let runtime = AmmRuntime::spawn(
                cache,
                AdapterRegistry::new(),
                runtime_baseline(500),
                AmmRuntimeConfig::default().with_command_capacity(128),
            )?;
            let missing = PoolInstanceId::new(
                PoolKey::SolidlyV2(Address::repeat_byte(0xfd)),
                PoolGeneration::new(0),
            );
            let mut controls = Vec::new();
            for _ in 0..100 {
                controls.push(runtime.try_remove_pool(missing.clone(), AmmEvictionPolicy::Retain)?);
            }
            let canonical = runtime.try_ingest_batch(empty_canonical_batch(501))?;
            canonical.wait().await?;
            assert_eq!(runtime.latest_snapshot().version(), AmmStateVersion::new(1));
            for control in controls {
                assert!(matches!(
                    control.wait().await,
                    Err(evm_amm_state::adapters::AmmRuntimeCommandError::StalePoolInstance { .. })
                ));
            }
            runtime.shutdown().await?;
            Ok(())
        })
        .await
}

#[tokio::test(flavor = "multi_thread")]
async fn partial_batch_failure_publishes_nothing_and_fences_future_mutation() -> Result<()> {
    tokio::task::LocalSet::new()
        .run_until(async {
            let point = AmmStatePoint::post_block(1, 500, block_hash(500));
            let mut cache = setup_cache().await;
            align_cache(&mut cache, 500);
            let target = StateSlot::new(Address::repeat_byte(0x90), U256::from(1));
            let first_emitter = Address::repeat_byte(0x91);
            let conflict_emitter = Address::repeat_byte(0x92);
            let first_topic = B256::repeat_byte(0x93);
            let conflict_topic = B256::repeat_byte(0x94);
            let first_adapter = Arc::new(TestWriteAdapter {
                protocol: "runtime-first-write",
                emitter: first_emitter,
                topic: first_topic,
                target,
            });
            let conflict_adapter = Arc::new(TestWriteAdapter {
                protocol: "runtime-conflicting-writes",
                emitter: conflict_emitter,
                topic: conflict_topic,
                target,
            });
            let mut registry = AdapterRegistry::new();
            registry.register_adapter(first_adapter)?;
            registry.register_adapter(conflict_adapter)?;
            registry.register_pool(custom_registration(
                "runtime-first-write",
                Address::repeat_byte(1),
            ))?;
            registry.register_pool(custom_registration(
                "runtime-conflicting-writes",
                Address::repeat_byte(2),
            ))?;
            registry.register_pool(custom_registration(
                "runtime-conflicting-writes",
                Address::repeat_byte(3),
            ))?;
            let runtime = AmmRuntime::spawn(
                cache,
                registry,
                runtime_baseline(500),
                AmmRuntimeConfig::default(),
            )?;
            let mut critical = runtime.subscribe_changes().await?;

            let error = runtime
                .ingest_batch(canonical_log_batch(
                    501,
                    [
                        (first_emitter, first_topic, 0),
                        (conflict_emitter, conflict_topic, 1),
                    ],
                ))
                .await
                .expect_err("the second record writes conflicting values");
            assert!(matches!(
                error,
                evm_amm_state::adapters::AmmRuntimeCommandError::Sync(_)
            ));
            assert_eq!(
                runtime.latest_snapshot().version(),
                AmmStateVersion::initial()
            );
            assert_eq!(runtime.latest_snapshot().point(), point);
            assert_eq!(
                runtime
                    .latest_snapshot()
                    .cache()
                    .storage_value(target.address(), target.slot()),
                None,
                "the previously published immutable cache stays unchanged"
            );
            assert_eq!(
                runtime.latest_status().health(),
                AmmRuntimeHealth::Untrusted
            );
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(10), critical.next_commit())
                    .await
                    .is_err(),
                "a partially failed batch must not reach the reliable stream"
            );
            assert!(matches!(
                runtime.ingest_batch(empty_canonical_batch(502)).await,
                Err(evm_amm_state::adapters::AmmRuntimeCommandError::Untrusted)
            ));

            runtime.shutdown().await?;
            Ok(())
        })
        .await
}

#[tokio::test(flavor = "multi_thread")]
async fn degradation_is_coherent_across_change_snapshot_status_and_observers() -> Result<()> {
    tokio::task::LocalSet::new()
        .run_until(async {
            let protocol = "runtime-degradation";
            let pool_address = Address::repeat_byte(0xd1);
            let emitter = Address::repeat_byte(0xd2);
            let topic = B256::repeat_byte(0xd3);
            let adapter = Arc::new(TestFailingAdapter {
                protocol,
                emitter,
                topic,
            });
            let mut registry = AdapterRegistry::new();
            registry.register_adapter(adapter)?;
            let pool = custom_registration(protocol, pool_address);
            let pool_key = pool.key.clone();
            registry.register_pool(pool)?;
            let mut cache = setup_cache().await;
            align_cache(&mut cache, 500);
            let runtime = AmmRuntime::spawn(
                cache,
                registry,
                runtime_baseline(500),
                AmmRuntimeConfig::default(),
            )?;
            let mut observer = runtime.subscribe_events();

            let changes = runtime
                .ingest_batch(canonical_log_batch(501, [(emitter, topic, 0)]))
                .await?;
            assert!(matches!(
                changes.pool_changes(),
                [change] if change.kind() == AmmPoolChangeKind::Degraded
            ));
            assert_eq!(
                runtime
                    .latest_snapshot()
                    .registry()
                    .registry()
                    .pool(&pool_key)
                    .expect("published registration")
                    .status,
                PoolStatus::Degraded
            );
            assert_eq!(runtime.latest_status().health(), AmmRuntimeHealth::Degraded);
            assert!(matches!(
                observer.next_event().await?.kind(),
                AmmRuntimeEventKind::HealthChanged {
                    from: AmmRuntimeHealth::Healthy,
                    to: AmmRuntimeHealth::Degraded,
                }
            ));
            assert!(matches!(
                observer.next_event().await?.kind(),
                AmmRuntimeEventKind::StateCommitted { .. }
            ));
            runtime.shutdown().await?;
            Ok(())
        })
        .await
}

#[test]
fn canonical_envelope_rejects_records_from_a_different_block() {
    assert!(matches!(
        AmmCanonicalBatch::from_verified_block(
            1,
            canonical_header(501),
            7,
            raw_canonical_batch(502)
        ),
        Err(AmmCanonicalBatchError::RecordBlockMismatch { .. })
    ));
}

#[test]
fn canonical_envelope_rejects_a_header_with_an_invalid_hash() {
    let mut header = canonical_header(501);
    header.hash = B256::repeat_byte(0xff);
    assert!(matches!(
        AmmCanonicalBatch::from_verified_block(1, header, 7, raw_canonical_batch(501)),
        Err(AmmCanonicalBatchError::HeaderHashMismatch { .. })
    ));
}

#[test]
fn canonical_envelope_rejects_intrinsic_log_mismatch_and_duplicates() {
    let mut records = raw_canonical_batch(501).into_records();
    let duplicate = records[0].clone();
    records.push(duplicate);
    assert!(matches!(
        AmmCanonicalBatch::from_verified_block(
            1,
            canonical_header(501),
            7,
            ReactiveInputBatch::new(records)
        ),
        Err(AmmCanonicalBatchError::DuplicateRecord { index: 1 })
    ));

    let mut records = raw_canonical_batch(501).into_records();
    let mut conflicting_position = records[0].clone();
    let ReactiveInput::Log(log) = &mut conflicting_position.input else {
        unreachable!("fixture is a log")
    };
    log.transaction_hash = Some(B256::repeat_byte(0x99));
    records.push(conflicting_position);
    assert!(matches!(
        AmmCanonicalBatch::from_verified_block(
            1,
            canonical_header(501),
            7,
            ReactiveInputBatch::new(records)
        ),
        Err(AmmCanonicalBatchError::DuplicateRecord { index: 1 })
    ));

    let mut records = raw_canonical_batch(501).into_records();
    let ReactiveInput::Log(log) = &mut records[0].input else {
        unreachable!("fixture is a log")
    };
    log.block_hash = Some(B256::repeat_byte(0xee));
    assert!(matches!(
        AmmCanonicalBatch::from_verified_block(
            1,
            canonical_header(501),
            7,
            ReactiveInputBatch::new(records)
        ),
        Err(AmmCanonicalBatchError::MalformedRecord { index: 0, .. })
    ));
}

#[test]
fn runtime_publications_and_handle_are_send_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<evm_amm_state::adapters::AmmRuntimeHandle>();
    assert_send_sync::<evm_amm_state::adapters::AmmStateSnapshot>();
    assert_send_sync::<evm_amm_state::adapters::AmmStateCommit>();
}
