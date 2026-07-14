//! Offline routing-cost comparison for the Stage 3 indexed pool-handler model.
//!
//! The target pool is registered last. The compatibility case keeps every
//! interest under one fallback registry-wide handler; the pool-scoped case
//! gives each pool a generation-fenced handler with an exhaustive exact index.

use std::sync::Arc;

use alloy_network::Ethereum;
use alloy_primitives::{Address, B256, Bytes, Log as PrimitiveLog, keccak256};
use alloy_rpc_types_eth::Log as RpcLog;
use criterion::{BenchmarkId, Criterion, black_box, criterion_group, criterion_main};
use evm_amm_state::adapters::{
    AdapterRegistry, AmmAdapter, AmmPoolReactiveHandler, AmmReactiveHandler,
    AmmReactiveRoutingContext, CustomPoolKey, EventSource, PoolGeneration, PoolInstanceId, PoolKey,
    PoolRegistration, ProtocolId,
};
use evm_fork_cache::reactive::{ReactiveInterest, ReactiveRegistry, RegisterError};

const POOL_COUNTS: [usize; 4] = [16, 64, 320, 4096];
const PROTOCOL: &str = "stage2-routing-bench";

struct DirectAdapter;

impl AmmAdapter for DirectAdapter {
    fn protocol(&self) -> ProtocolId {
        ProtocolId::Custom(PROTOCOL)
    }
}

fn address(value: usize) -> Address {
    let mut bytes = [0_u8; 20];
    bytes[12..].copy_from_slice(&(value as u64 + 1).to_be_bytes());
    Address::from(bytes)
}

fn adapter_registry(count: usize) -> AdapterRegistry {
    let mut registry = AdapterRegistry::new();
    registry
        .register_adapter(Arc::new(DirectAdapter))
        .expect("register adapter");
    for index in 0..count {
        let address = address(index);
        registry
            .register_pool(
                PoolRegistration::new(PoolKey::Custom(CustomPoolKey::Address {
                    protocol: PROTOCOL,
                    address,
                }))
                .with_event_source(EventSource::direct(
                    address,
                    vec![keccak256("Sync(uint112,uint112)")],
                )),
            )
            .expect("unique pool");
    }
    registry
}

fn compatibility_router(
    registry: &AdapterRegistry,
) -> Result<(ReactiveRegistry<Ethereum>, Address), RegisterError> {
    let mut router = ReactiveRegistry::new();
    let handler = AmmReactiveHandler::new(registry.clone());
    let last_emitter = handler
        .interests()
        .into_iter()
        .rev()
        .find_map(|interest| match interest {
            ReactiveInterest::Logs(interest) => {
                interest.provider_filter.address.iter().next().copied()
            }
            ReactiveInterest::Blocks(_) | ReactiveInterest::PendingTransactions(_) => None,
        })
        .expect("at least one direct interest");
    router.register_handler(Arc::new(handler))?;
    Ok((router, last_emitter))
}

fn pool_router(
    registry: &AdapterRegistry,
) -> Result<ReactiveRegistry<Ethereum>, Box<dyn std::error::Error>> {
    let mut router = ReactiveRegistry::new();
    let registry = Arc::new(registry.clone());
    let routing = AmmReactiveRoutingContext::new(registry.clone());
    let mut pools: Vec<_> = registry.pools().map(|pool| pool.key.clone()).collect();
    pools.sort();
    for pool in pools {
        router.register_handler(Arc::new(AmmPoolReactiveHandler::with_routing_context(
            routing.clone(),
            PoolInstanceId::new(pool, PoolGeneration::new(0)),
        )?))?;
    }
    Ok(router)
}

fn pool_routing(c: &mut Criterion) {
    let mut compatibility = c.benchmark_group("pool_routing/compatibility_worst_case");
    for count in POOL_COUNTS {
        let registry = adapter_registry(count);
        let (router, target) = compatibility_router(&registry).expect("compatibility router");
        let log = RpcLog {
            inner: PrimitiveLog::new_unchecked(
                target,
                vec![keccak256("Sync(uint112,uint112)")],
                Bytes::new(),
            ),
            block_hash: Some(B256::repeat_byte(1)),
            block_number: Some(1),
            ..RpcLog::default()
        };
        assert_eq!(router.route_log(&log).len(), 1);
        compatibility.bench_with_input(BenchmarkId::from_parameter(count), &count, |b, _| {
            b.iter(|| black_box(router.route_log(black_box(&log))))
        });
    }
    compatibility.finish();

    let mut pool_scoped = c.benchmark_group("pool_routing/pool_scoped_worst_case");
    for count in POOL_COUNTS {
        let registry = adapter_registry(count);
        let router = pool_router(&registry).expect("pool router");
        let log = RpcLog {
            inner: PrimitiveLog::new_unchecked(
                address(count - 1),
                vec![keccak256("Sync(uint112,uint112)")],
                Bytes::new(),
            ),
            block_hash: Some(B256::repeat_byte(1)),
            block_number: Some(1),
            ..RpcLog::default()
        };
        assert_eq!(router.route_log(&log).len(), 1);
        pool_scoped.bench_with_input(BenchmarkId::from_parameter(count), &count, |b, _| {
            b.iter(|| black_box(router.route_log(black_box(&log))))
        });
    }
    pool_scoped.finish();
}

criterion_group!(benches, pool_routing);
criterion_main!(benches);
