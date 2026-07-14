//! Fully offline baseline for transactional mid-lifecycle pool add/remove.
//!
//! The benchmark names/workloads are unchanged from Stage 0/2, when these
//! methods cloned the registry and rebuilt `ReactiveRuntime`. Stage 3 performs
//! generation-scoped in-place mutation, giving a direct before/after comparison.
//!
//! ```text
//! cargo bench --bench runtime_lifecycle
//! ```

use std::sync::Arc;

use alloy_primitives::Address;
use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use evm_amm_state::adapters::{
    AdapterRegistry, AmmSyncEngine, PoolKey, PoolRegistration, PoolStatus, ProtocolMetadata,
    UniswapV2Adapter, UniswapV2Metadata,
};

const EXISTING_POOL_COUNTS: [usize; 4] = [0, 16, 64, 320];

fn address(value: u64) -> Address {
    let mut bytes = [0_u8; 20];
    bytes[12..].copy_from_slice(&value.to_be_bytes());
    Address::from(bytes)
}

fn pool(index: usize) -> PoolRegistration {
    let pool = address(index as u64 + 1);
    PoolRegistration::new(PoolKey::UniswapV2(pool))
        .with_state_address(pool)
        .with_metadata(ProtocolMetadata::UniswapV2(
            UniswapV2Metadata::default()
                .with_token0(address(10_001))
                .with_token1(address(10_002))
                .with_fee_bps(30),
        ))
        .with_status(PoolStatus::Ready)
}

fn engine_with_pools(count: usize) -> AmmSyncEngine {
    let mut registry = AdapterRegistry::new();
    registry
        .register_adapter(Arc::new(UniswapV2Adapter::default()))
        .expect("register V2 adapter");
    for index in 0..count {
        registry.register_pool(pool(index)).expect("unique pool");
    }
    AmmSyncEngine::new(registry).expect("build AMM sync engine")
}

fn runtime_lifecycle(c: &mut Criterion) {
    let mut register = c.benchmark_group("runtime_lifecycle/register_one");
    register.throughput(Throughput::Elements(1));
    for existing in EXISTING_POOL_COUNTS {
        register.bench_with_input(
            BenchmarkId::from_parameter(existing),
            &existing,
            |b, &count| {
                b.iter_batched(
                    || (engine_with_pools(count), pool(count + 10_000)),
                    |(mut engine, registration)| {
                        engine
                            .register_pools([registration])
                            .expect("register one pool");
                        engine
                    },
                    BatchSize::SmallInput,
                );
            },
        );
    }
    register.finish();

    let mut unregister = c.benchmark_group("runtime_lifecycle/unregister_one");
    unregister.throughput(Throughput::Elements(1));
    for existing in EXISTING_POOL_COUNTS.into_iter().filter(|count| *count > 0) {
        unregister.bench_with_input(
            BenchmarkId::from_parameter(existing),
            &existing,
            |b, &count| {
                b.iter_batched(
                    || (engine_with_pools(count), pool(count - 1).key),
                    |(mut engine, key)| {
                        let removed = engine
                            .unregister_pools(std::slice::from_ref(&key))
                            .expect("unregister one pool");
                        assert_eq!(removed.len(), 1);
                        (engine, removed)
                    },
                    BatchSize::SmallInput,
                );
            },
        );
    }
    unregister.finish();
}

criterion_group!(benches, runtime_lifecycle);
criterion_main!(benches);
