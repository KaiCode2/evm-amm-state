#![cfg(feature = "live-runtime")]

use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use alloy_consensus::Header as ConsensusHeader;
use alloy_json_rpc::{RequestPacket, ResponsePacket};
use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_provider::{RootProvider, network::AnyNetwork};
use alloy_rpc_client::RpcClient;
use alloy_rpc_types_eth::{EIP1186AccountProofResponse, Header as RpcHeader};
use alloy_transport::mock::{Asserter, MockTransport};
use alloy_transport::{TransportError, TransportFut};
use anyhow::Result;
use evm_amm_state::adapters::{
    AdapterRegistry, AmmColdStartOptions, AmmColdStartWorkerConfig, AmmRuntime, AmmRuntimeBaseline,
    AmmRuntimeConfig, AmmRuntimeEventKind, AmmWorkClass, PoolKey, PoolRegistration,
    ProtocolMetadata, UniswapV2Adapter, UniswapV2Metadata, uniswap_v2_pair_runtime_code_hash,
};
use evm_fork_cache::cache::EvmCache;
use tower::Service;

#[derive(Clone)]
struct GatedMockTransport {
    inner: MockTransport,
    permits: Arc<tokio::sync::Semaphore>,
    methods: Arc<Mutex<Vec<String>>>,
}

impl GatedMockTransport {
    fn new(asserter: Asserter) -> Self {
        Self {
            inner: MockTransport::new(asserter),
            permits: Arc::new(tokio::sync::Semaphore::new(0)),
            methods: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn release(&self, permits: usize) {
        self.permits.add_permits(permits);
    }

    fn has_method(&self, expected: &str) -> bool {
        self.methods
            .lock()
            .expect("method log lock")
            .iter()
            .any(|method| method == expected)
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
        let methods: Vec<_> = request.method_names().map(str::to_owned).collect();
        let should_gate = methods
            .iter()
            .any(|method| method == "eth_call" || method == "eth_getStorageAt");
        self.methods
            .lock()
            .expect("method log lock")
            .extend(methods);
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

fn block_hash(number: u64) -> B256 {
    canonical_header(number).hash
}

fn canonical_header(number: u64) -> RpcHeader {
    RpcHeader::new(ConsensusHeader {
        parent_hash: if number == 0 {
            B256::ZERO
        } else {
            block_hash(number - 1)
        },
        number,
        timestamp: 1_700_000_000 + number,
        base_fee_per_gas: Some(100 + number),
        beneficiary: Address::repeat_byte(0xcb),
        gas_limit: 30_000_000,
        mix_hash: B256::repeat_byte(0xab),
        ..ConsensusHeader::default()
    })
}

async fn runtime_with_registry(
    registry: AdapterRegistry,
) -> Result<evm_amm_state::adapters::AmmRuntimeHandle> {
    let provider = RootProvider::<AnyNetwork>::new(RpcClient::mocked(Asserter::new()));
    let mut cache = EvmCache::new(Arc::new(provider)).await;
    cache.advance_block(&canonical_header(500))?;
    Ok(AmmRuntime::spawn(
        cache,
        registry,
        AmmRuntimeBaseline::from_verified_header(1, canonical_header(500))?,
        AmmRuntimeConfig::default(),
    )?)
}

fn v2_pool(address: Address) -> PoolRegistration {
    PoolRegistration::new(PoolKey::UniswapV2(address))
        .with_state_address(address)
        .with_metadata(ProtocolMetadata::UniswapV2(
            UniswapV2Metadata::default()
                .with_token0(Address::repeat_byte(0xa0))
                .with_token1(Address::repeat_byte(0xa1))
                .with_fee_bps(30),
        ))
}

fn encoded_v2_state(seed: u64) -> Bytes {
    let mut bytes = Vec::new();
    for word in [
        U256::from_be_slice(Address::repeat_byte(0xa0).as_slice()),
        U256::from_be_slice(Address::repeat_byte(0xa1).as_slice()),
        U256::from(seed) | (U256::from(seed + 1) << 112),
    ] {
        bytes.extend_from_slice(&word.to_be_bytes::<32>());
    }
    bytes.into()
}

fn v2_proof(address: Address) -> EIP1186AccountProofResponse {
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

async fn wait_for_method(transport: &GatedMockTransport, method: &str) -> Result<()> {
    tokio::time::timeout(Duration::from_millis(500), async {
        while !transport.has_method(method) {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn focused_work_overtakes_queued_bootstrap_work() -> Result<()> {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut registry = AdapterRegistry::new();
            registry.register_adapter(Arc::new(UniswapV2Adapter::default()))?;
            let runtime = runtime_with_registry(registry).await?;
            let first = Address::repeat_byte(0x31);
            let second = Address::repeat_byte(0x32);
            let focused = Address::repeat_byte(0x33);
            let assertions = Asserter::new();
            for (address, seed) in [(first, 10), (focused, 20), (second, 30)] {
                assertions.push_success(&encoded_v2_state(seed));
                assertions.push_success(&v2_proof(address));
            }
            let transport = GatedMockTransport::new(assertions);
            let provider = RootProvider::<AnyNetwork>::new(RpcClient::new(transport.clone(), true));
            let _worker = runtime
                .attach_cold_start_worker(
                    provider,
                    AmmColdStartWorkerConfig::default().with_max_concurrency(1),
                )
                .await?;
            let mut events = runtime.subscribe_events();
            let bootstrap = runtime
                .queue_cold_start(
                    vec![v2_pool(first), v2_pool(second)],
                    AmmColdStartOptions::default(),
                )
                .await?;
            wait_for_method(&transport, "eth_call").await?;
            let focused = runtime
                .queue_cold_start(
                    vec![v2_pool(focused)],
                    AmmColdStartOptions::default().with_class(AmmWorkClass::Focused),
                )
                .await?;
            transport.release(3);

            let mut completed = Vec::new();
            tokio::time::timeout(Duration::from_secs(1), async {
                while completed.len() < 3 {
                    let event = events.next_event().await.expect("runtime remains open");
                    if let AmmRuntimeEventKind::WorkCompleted { work } = event.kind() {
                        completed.push(work.clone());
                    }
                }
            })
            .await?;
            assert_eq!(
                completed,
                [
                    bootstrap[0].work().clone(),
                    focused[0].work().clone(),
                    bootstrap[1].work().clone(),
                ]
            );
            runtime.shutdown().await?;
            Ok(())
        })
        .await
}
