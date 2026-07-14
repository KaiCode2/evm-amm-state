//! Offline Stage 4 cache-actor creation baseline after cache/provider supply.
//!
//! ```text
//! cargo bench --features live-runtime --bench live_runtime_actor
//! ```

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use alloy_consensus::Header as ConsensusHeader;
use alloy_json_rpc::{RequestPacket, ResponsePacket};
use alloy_primitives::{Address, B256, U256};
use alloy_provider::{RootProvider, network::AnyNetwork};
use alloy_rpc_client::RpcClient;
use alloy_rpc_types_eth::{EIP1186AccountProofResponse, Header as RpcHeader};
use alloy_transport::mock::{Asserter, MockTransport};
use alloy_transport::{TransportError, TransportFut};
use criterion::{Criterion, criterion_group, criterion_main};
use evm_amm_state::adapters::{
    AdapterRegistry, AmmCanonicalBatch, AmmColdStartOptions, AmmColdStartWorkerConfig, AmmRuntime,
    AmmRuntimeBaseline, AmmRuntimeConfig, PoolKey, PoolRegistration, ProtocolMetadata,
    UniswapV2Adapter, UniswapV2Metadata, uniswap_v2_pair_runtime_code_hash,
};
use evm_fork_cache::cache::EvmCache;
use evm_fork_cache::reactive::ReactiveInputBatch;
use tower::Service;

#[derive(Clone)]
struct GatedTransport {
    inner: MockTransport,
    permits: Arc<tokio::sync::Semaphore>,
    provider_blocked: Arc<AtomicBool>,
}

impl GatedTransport {
    fn new(asserter: Asserter) -> Self {
        Self {
            inner: MockTransport::new(asserter),
            permits: Arc::new(tokio::sync::Semaphore::new(0)),
            provider_blocked: Arc::new(AtomicBool::new(false)),
        }
    }

    fn release_one(&self) {
        self.permits.add_permits(1);
    }

    fn provider_blocked(&self) -> bool {
        self.provider_blocked.load(Ordering::Acquire)
    }
}

impl Service<RequestPacket> for GatedTransport {
    type Response = ResponsePacket;
    type Error = TransportError;
    type Future = TransportFut<'static>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: RequestPacket) -> Self::Future {
        let should_gate = request.method_names().any(|method| method == "eth_call");
        if should_gate {
            self.provider_blocked.store(true, Ordering::Release);
        }
        let permits = Arc::clone(&self.permits);
        let mut inner = self.inner.clone();
        Box::pin(async move {
            if should_gate {
                permits
                    .acquire_owned()
                    .await
                    .expect("benchmark provider gate remains open")
                    .forget();
            }
            inner.call(request).await
        })
    }
}

fn header() -> RpcHeader {
    RpcHeader::new(ConsensusHeader {
        number: 500,
        timestamp: 1_700_000_500,
        base_fee_per_gas: Some(100),
        beneficiary: Address::repeat_byte(0xcb),
        gas_limit: 30_000_000,
        mix_hash: B256::repeat_byte(0xab),
        ..ConsensusHeader::default()
    })
}

fn child_header(parent: &RpcHeader) -> RpcHeader {
    let number = parent.inner.number.checked_add(1).expect("benchmark range");
    RpcHeader::new(ConsensusHeader {
        parent_hash: parent.hash,
        number,
        timestamp: 1_700_000_000 + number,
        base_fee_per_gas: Some(100 + number),
        beneficiary: Address::repeat_byte(0xcb),
        gas_limit: 30_000_000,
        mix_hash: B256::repeat_byte(0xab),
        ..ConsensusHeader::default()
    })
}

fn empty_canonical_batch(header: RpcHeader) -> AmmCanonicalBatch {
    AmmCanonicalBatch::from_verified_block(1, header, 0, ReactiveInputBatch::new(Vec::new()))
        .expect("benchmark batch is coherent")
}

fn encoded_words(words: impl IntoIterator<Item = U256>) -> alloy_primitives::Bytes {
    let mut encoded = Vec::new();
    for word in words {
        encoded.extend_from_slice(&word.to_be_bytes::<32>());
    }
    encoded.into()
}

fn account_proof(address: Address) -> EIP1186AccountProofResponse {
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

fn v2_registration(address: Address) -> PoolRegistration {
    PoolRegistration::new(PoolKey::UniswapV2(address))
        .with_state_address(address)
        .with_metadata(ProtocolMetadata::UniswapV2(
            UniswapV2Metadata::default()
                .with_token0(Address::repeat_byte(0xa0))
                .with_token1(Address::repeat_byte(0xa1))
                .with_fee_bps(30),
        ))
}

async fn prepared_cache(header: &RpcHeader) -> EvmCache {
    let provider = RootProvider::<AnyNetwork>::new(RpcClient::mocked(Asserter::new()));
    let mut cache = EvmCache::new(Arc::new(provider)).await;
    cache
        .advance_block(header)
        .expect("benchmark header has complete context");
    cache
}

fn live_runtime_actor(c: &mut Criterion) {
    let tokio = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_time()
        .build()
        .expect("build benchmark runtime");
    c.bench_function("live_runtime/handle_creation_after_cache_supply", |b| {
        b.iter_custom(|iterations| {
            let mut measured = Duration::ZERO;
            for _ in 0..iterations {
                let header = header();
                let cache = tokio.block_on(prepared_cache(&header));
                let baseline = AmmRuntimeBaseline::from_verified_header(1, header)
                    .expect("sealed benchmark baseline");
                let local = tokio::task::LocalSet::new();
                measured += local.block_on(&tokio, async {
                    let started = Instant::now();
                    let runtime = AmmRuntime::spawn(
                        cache,
                        AdapterRegistry::new(),
                        baseline,
                        AmmRuntimeConfig::default(),
                    )
                    .expect("spawn cache actor");
                    let elapsed = started.elapsed();
                    runtime.shutdown().await.expect("shutdown cache actor");
                    elapsed
                });
            }
            measured
        });
    });
    c.bench_function("live_runtime/control_command_enqueue", |b| {
        b.iter_custom(|iterations| {
            let header = header();
            let cache = tokio.block_on(prepared_cache(&header));
            let baseline = AmmRuntimeBaseline::from_verified_header(1, header)
                .expect("sealed benchmark baseline");
            let local = tokio::task::LocalSet::new();
            local.block_on(&tokio, async {
                let runtime = AmmRuntime::spawn(
                    cache,
                    AdapterRegistry::new(),
                    baseline,
                    AmmRuntimeConfig::default(),
                )
                .expect("spawn cache actor");
                let point = runtime.latest_snapshot().point();
                let mut measured = Duration::ZERO;
                for _ in 0..iterations {
                    let started = Instant::now();
                    let ticket = runtime
                        .try_install_prepared_pools(Vec::new(), point)
                        .expect("benchmark command queue has capacity");
                    measured += started.elapsed();
                    ticket.wait().await.expect("empty install commits");
                }
                runtime.shutdown().await.expect("shutdown cache actor");
                measured
            })
        });
    });
    c.bench_function(
        "live_runtime/live_batch_round_trip_during_blocked_bootstrap",
        |b| {
            b.iter_custom(|iterations| {
                let baseline_header = header();
                let cache = tokio.block_on(prepared_cache(&baseline_header));
                let baseline = AmmRuntimeBaseline::from_verified_header(1, baseline_header.clone())
                    .expect("sealed benchmark baseline");
                let local = tokio::task::LocalSet::new();
                local.block_on(&tokio, async {
                    let mut registry = AdapterRegistry::new();
                    registry
                        .register_adapter(Arc::new(UniswapV2Adapter::default()))
                        .expect("register benchmark adapter");
                    let runtime =
                        AmmRuntime::spawn(cache, registry, baseline, AmmRuntimeConfig::default())
                            .expect("spawn cache actor");
                    let address = Address::repeat_byte(0x92);
                    let assertions = Asserter::new();
                    assertions.push_success(&encoded_words([
                        U256::from_be_slice(Address::repeat_byte(0xa0).as_slice()),
                        U256::from_be_slice(Address::repeat_byte(0xa1).as_slice()),
                        U256::from(33) | (U256::from(44) << 112),
                    ]));
                    assertions.push_success(&account_proof(address));
                    let transport = GatedTransport::new(assertions);
                    let provider =
                        RootProvider::<AnyNetwork>::new(RpcClient::new(transport.clone(), true));
                    let worker = runtime
                        .attach_cold_start_worker(
                            provider,
                            AmmColdStartWorkerConfig::default().with_max_concurrency(1),
                        )
                        .await
                        .expect("attach benchmark worker");
                    let queued = runtime
                        .queue_cold_start(
                            vec![v2_registration(address)],
                            AmmColdStartOptions::default(),
                        )
                        .await
                        .expect("queue blocked benchmark cold start");
                    tokio::time::timeout(Duration::from_secs(1), async {
                        while !transport.provider_blocked() {
                            tokio::task::yield_now().await;
                        }
                    })
                    .await
                    .expect("cold-start provider call reaches the benchmark gate");

                    let mut current_header = baseline_header;
                    let mut measured = Duration::ZERO;
                    for _ in 0..iterations {
                        current_header = child_header(&current_header);
                        let batch = empty_canonical_batch(current_header.clone());
                        let started = Instant::now();
                        runtime
                            .ingest_batch(batch)
                            .await
                            .expect("benchmark canonical batch commits");
                        measured += started.elapsed();
                    }

                    transport.release_one();
                    let mut status = runtime.subscribe_status();
                    tokio::time::timeout(Duration::from_secs(1), async {
                        while status.borrow().active_work(queued[0].work()).is_some() {
                            status
                                .changed()
                                .await
                                .expect("benchmark runtime remains alive");
                        }
                    })
                    .await
                    .expect("stale blocked work resolves after the measurement");
                    worker.shutdown();
                    runtime.shutdown().await.expect("shutdown cache actor");
                    measured
                })
            });
        },
    );
    c.bench_function("live_runtime/cold_start_queue_return", |b| {
        b.iter_custom(|iterations| {
            let mut measured = Duration::ZERO;
            for _ in 0..iterations {
                let header = header();
                let cache = tokio.block_on(prepared_cache(&header));
                let baseline = AmmRuntimeBaseline::from_verified_header(1, header)
                    .expect("sealed benchmark baseline");
                let local = tokio::task::LocalSet::new();
                measured += local.block_on(&tokio, async {
                    let mut registry = AdapterRegistry::new();
                    registry
                        .register_adapter(Arc::new(UniswapV2Adapter::default()))
                        .expect("register benchmark adapter");
                    let runtime =
                        AmmRuntime::spawn(cache, registry, baseline, AmmRuntimeConfig::default())
                            .expect("spawn cache actor");
                    let provider =
                        RootProvider::<AnyNetwork>::new(RpcClient::mocked(Asserter::new()));
                    let worker = runtime
                        .attach_cold_start_worker(
                            provider,
                            AmmColdStartWorkerConfig::default().with_max_concurrency(1),
                        )
                        .await
                        .expect("attach benchmark worker");
                    let address = Address::repeat_byte(0x91);
                    let registration = PoolRegistration::new(PoolKey::UniswapV2(address))
                        .with_metadata(ProtocolMetadata::UniswapV2(
                            UniswapV2Metadata::default()
                                .with_token0(Address::repeat_byte(0xa0))
                                .with_token1(Address::repeat_byte(0xa1))
                                .with_fee_bps(30),
                        ));
                    let started = Instant::now();
                    runtime
                        .queue_cold_start(vec![registration], AmmColdStartOptions::default())
                        .await
                        .expect("queue benchmark cold start");
                    let elapsed = started.elapsed();
                    runtime.shutdown().await.expect("shutdown cache actor");
                    assert!(matches!(
                        worker.latest_state(),
                        evm_amm_state::adapters::AmmColdStartWorkerState::Running
                            | evm_amm_state::adapters::AmmColdStartWorkerState::Stopped
                    ));
                    elapsed
                });
            }
            measured
        });
    });
}

criterion_group!(benches, live_runtime_actor);
criterion_main!(benches);
