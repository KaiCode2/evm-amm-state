#![cfg(feature = "live-runtime")]

use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use alloy_consensus::Header as ConsensusHeader;
use alloy_json_rpc::{RequestPacket, ResponsePacket};
use alloy_network::AnyNetwork;
use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_provider::RootProvider;
use alloy_rpc_client::RpcClient;
use alloy_rpc_types_eth::{EIP1186AccountProofResponse, Header as RpcHeader};
use alloy_transport::mock::{Asserter, MockTransport};
use alloy_transport::{TransportError, TransportFut};
use anyhow::Result;
use evm_amm_state::adapters::{
    AdapterRegistry, AmmColdStartOptions, AmmColdStartWorkerConfig, AmmColdStartWorkerState,
    AmmRuntime, AmmRuntimeBaseline, AmmRuntimeCommandError, AmmRuntimeConfig, PoolKey,
    PoolRegistration, ProtocolMetadata, UniswapV2Adapter, UniswapV2Metadata,
    uniswap_v2_pair_runtime_code_hash,
};
use evm_fork_cache::cache::EvmCache;
use tower::Service;

#[derive(Clone)]
struct GatedMockTransport {
    inner: MockTransport,
    permits: Arc<tokio::sync::Semaphore>,
    requests: Arc<Mutex<Vec<String>>>,
}

impl GatedMockTransport {
    fn new(asserter: Asserter) -> Self {
        Self {
            inner: MockTransport::new(asserter),
            permits: Arc::new(tokio::sync::Semaphore::new(0)),
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn requested(&self, method: &str) -> bool {
        self.requests
            .lock()
            .expect("request log lock")
            .iter()
            .any(|requested| requested == method)
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
        let should_gate = request.method_names().any(|method| method == "eth_call");
        self.requests
            .lock()
            .expect("request log lock")
            .extend(request.method_names().map(std::borrow::ToOwned::to_owned));
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

fn canonical_header(block_number: u64) -> RpcHeader {
    RpcHeader::new(ConsensusHeader {
        parent_hash: B256::repeat_byte(0x11),
        number: block_number,
        timestamp: 1_700_000_000 + block_number,
        base_fee_per_gas: Some(100 + block_number),
        beneficiary: Address::repeat_byte(0xcb),
        gas_limit: 30_000_000,
        mix_hash: B256::repeat_byte(0xab),
        ..ConsensusHeader::default()
    })
}

fn v2_registration(address: Address) -> PoolRegistration {
    PoolRegistration::new(PoolKey::UniswapV2(address))
}

fn complete_v2_registration(address: Address) -> PoolRegistration {
    v2_registration(address)
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

#[tokio::test(flavor = "multi_thread")]
async fn runtime_shutdown_wakes_idle_worker_and_reports_stopped() -> Result<()> {
    tokio::task::LocalSet::new()
        .run_until(async {
            let assertions = Asserter::new();
            let provider = RootProvider::<AnyNetwork>::new(RpcClient::mocked(assertions.clone()));
            let mut cache = EvmCache::new(Arc::new(provider.clone())).await;
            let header = canonical_header(500);
            cache.advance_block(&header)?;
            let mut registry = AdapterRegistry::new();
            registry.register_adapter(Arc::new(UniswapV2Adapter::default()))?;
            let runtime = AmmRuntime::spawn(
                cache,
                registry,
                AmmRuntimeBaseline::from_verified_header(1, header)?,
                AmmRuntimeConfig::default(),
            )?;
            let worker = runtime
                .attach_cold_start_worker(provider, AmmColdStartWorkerConfig::default())
                .await?;
            let mut state = worker.subscribe_state();

            runtime.shutdown().await?;
            tokio::time::timeout(Duration::from_millis(250), async {
                while *state.borrow() != AmmColdStartWorkerState::Stopped {
                    state
                        .changed()
                        .await
                        .expect("worker state sender remains alive");
                }
            })
            .await?;

            Ok(())
        })
        .await
}

#[tokio::test(flavor = "multi_thread")]
async fn queue_capacity_bounds_individual_pool_jobs_atomically() -> Result<()> {
    tokio::task::LocalSet::new()
        .run_until(async {
            let assertions = Asserter::new();
            let provider = RootProvider::<AnyNetwork>::new(RpcClient::mocked(assertions));
            let mut cache = EvmCache::new(Arc::new(provider.clone())).await;
            let header = canonical_header(500);
            cache.advance_block(&header)?;
            let mut registry = AdapterRegistry::new();
            registry.register_adapter(Arc::new(UniswapV2Adapter::default()))?;
            let runtime = AmmRuntime::spawn(
                cache,
                registry,
                AmmRuntimeBaseline::from_verified_header(1, header)?,
                AmmRuntimeConfig::default(),
            )?;
            let worker = runtime
                .attach_cold_start_worker(
                    provider,
                    AmmColdStartWorkerConfig::default()
                        .with_queue_capacity(1)
                        .with_max_concurrency(1),
                )
                .await?;

            let result = runtime
                .queue_cold_start(
                    vec![
                        v2_registration(Address::repeat_byte(0xa1)),
                        v2_registration(Address::repeat_byte(0xb2)),
                    ],
                    AmmColdStartOptions::default(),
                )
                .await;
            let result_debug = format!("{result:?}");
            assert!(
                matches!(
                    result,
                    Err(AmmRuntimeCommandError::ColdStartWorker(message))
                        if message.contains("queue is full")
                ),
                "unexpected queue result: {result_debug}"
            );
            assert_eq!(runtime.latest_snapshot().registry().pool_count(), 0);
            let retry = runtime
                .queue_cold_start(
                    vec![v2_registration(Address::repeat_byte(0xa1))],
                    AmmColdStartOptions::default(),
                )
                .await?;
            assert_eq!(retry.len(), 1, "failed batch released every reservation");

            worker.shutdown();
            runtime.shutdown().await?;
            Ok(())
        })
        .await
}

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_rejects_new_pool_jobs_without_reserving_them() -> Result<()> {
    tokio::task::LocalSet::new()
        .run_until(async {
            let assertions = Asserter::new();
            let provider = RootProvider::<AnyNetwork>::new(RpcClient::mocked(assertions));
            let mut cache = EvmCache::new(Arc::new(provider.clone())).await;
            let header = canonical_header(500);
            cache.advance_block(&header)?;
            let mut registry = AdapterRegistry::new();
            registry.register_adapter(Arc::new(UniswapV2Adapter::default()))?;
            let runtime = AmmRuntime::spawn(
                cache,
                registry,
                AmmRuntimeBaseline::from_verified_header(1, header)?,
                AmmRuntimeConfig::default(),
            )?;
            let worker = runtime
                .attach_cold_start_worker(provider, AmmColdStartWorkerConfig::default())
                .await?;

            worker.shutdown();
            let result = runtime
                .queue_cold_start(
                    vec![v2_registration(Address::repeat_byte(0xc3))],
                    AmmColdStartOptions::default(),
                )
                .await;
            let result_debug = format!("{result:?}");
            assert!(
                matches!(
                    result,
                    Err(AmmRuntimeCommandError::ColdStartWorker(message))
                        if message.contains("worker is closed")
                ),
                "unexpected queue result: {result_debug}"
            );
            assert_eq!(runtime.latest_snapshot().registry().pool_count(), 0);

            runtime.shutdown().await?;
            Ok(())
        })
        .await
}

#[tokio::test(flavor = "multi_thread")]
async fn runtime_shutdown_drains_in_flight_fetches_before_worker_stops() -> Result<()> {
    tokio::task::LocalSet::new()
        .run_until(async {
            let assertions = Asserter::new();
            assertions.push_success(&encoded_words([
                U256::from_be_slice(Address::repeat_byte(0xa0).as_slice()),
                U256::from_be_slice(Address::repeat_byte(0xa1).as_slice()),
                U256::from(11) | (U256::from(22) << 112),
            ]));
            assertions.push_success(&v2_account_proof(Address::repeat_byte(0xd4)));
            let transport = GatedMockTransport::new(assertions);
            let provider = RootProvider::<AnyNetwork>::new(RpcClient::new(transport.clone(), true));
            let cache_provider =
                RootProvider::<AnyNetwork>::new(RpcClient::mocked(Asserter::new()));
            let mut cache = EvmCache::new(Arc::new(cache_provider)).await;
            let header = canonical_header(500);
            cache.advance_block(&header)?;
            let mut registry = AdapterRegistry::new();
            registry.register_adapter(Arc::new(UniswapV2Adapter::default()))?;
            let runtime = AmmRuntime::spawn(
                cache,
                registry,
                AmmRuntimeBaseline::from_verified_header(1, header)?,
                AmmRuntimeConfig::default(),
            )?;
            let worker = runtime
                .attach_cold_start_worker(
                    provider,
                    AmmColdStartWorkerConfig::default().with_max_concurrency(1),
                )
                .await?;
            let _queued = runtime
                .queue_cold_start(
                    vec![complete_v2_registration(Address::repeat_byte(0xd4))],
                    AmmColdStartOptions::default(),
                )
                .await?;

            tokio::time::timeout(Duration::from_millis(250), async {
                while !transport.requested("eth_call") {
                    tokio::task::yield_now().await;
                }
            })
            .await?;
            let mut state = worker.subscribe_state();
            runtime.shutdown().await?;
            tokio::time::timeout(Duration::from_millis(250), async {
                while *state.borrow() != AmmColdStartWorkerState::Stopped {
                    state
                        .changed()
                        .await
                        .expect("worker state sender remains alive");
                }
            })
            .await?;

            assert_eq!(runtime.latest_snapshot().registry().pool_count(), 0);
            Ok(())
        })
        .await
}
