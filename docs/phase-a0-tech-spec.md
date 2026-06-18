# Phase A0 Tech Spec: Base Adapter Contract

Status: manager draft for implementation.

## Spec

Implement the base adapter contract described in `docs/adapter-spec.md` without
migrating protocol behavior yet.

The target is a new `adapters` module that defines the crate-owned boundary
between AMM protocol semantics and `evm-fork-cache`:

- protocol-neutral pool identity and registration types
- exact event source/routing metadata
- subscription topic aggregation for tracked pools
- storage-layout constants/helpers currently imported from `evm-fork-cache`
- adapter event result, update quality, repair, and cold-start vocabulary
- an `AdapterCache` facade that speaks `evm_fork_cache::StateUpdate` and can be
  implemented for `evm_fork_cache::cache::EvmCache`

This phase should not migrate the old `src/events` simulation router or the
existing `cache_sync` initialization logic. Those remain compatibility paths
until later phases.

### Proposed Module Shape

```text
src/adapters/
  mod.rs
  cache.rs
  registry.rs
  storage.rs
  traits.rs
  types.rs
```

`src/lib.rs` should expose `pub mod adapters;`.

### Public Types

The module should export at least:

- `ProtocolId`
- `PoolKey`
- `CustomPoolKey`
- `EventSource`
- `EventRoute`
- `PoolRegistration`
- `ProtocolMetadata`
- `PoolStatus`
- `AdapterRegistry`
- `RegistryError`
- `AmmAdapter`
- `AdapterCache`
- `AdapterEvent`
- `AdapterEventKind`
- `AdapterEventResult`
- `UpdateQuality`
- `RepairAction`
- `ColdStartPolicy`
- `ColdStartOutcome`
- `ColdStartReport`
- `DeferredWork`
- `UnsupportedReason`

The adapter APIs should use `alloy_primitives::Log` because
`evm-fork-cache::events::EventDecoder` uses that log type. Existing
`alloy_rpc_types_eth::Log` code in `src/events` does not need to be migrated in
this phase.

### Identity

`PoolKey` must be strongly typed:

- address-keyed protocols use address variants, e.g. `PoolKey::UniswapV3(Address)`
- bytes32-keyed protocols use bytes32 variants, e.g. `PoolKey::BalancerV2(B256)`
- custom protocols use `CustomPoolKey`

Provide convenience methods:

- `PoolKey::protocol(&self) -> ProtocolId`
- `PoolKey::address(&self) -> Option<Address>`
- `PoolKey::bytes32(&self) -> Option<B256>`

### Event Sources And Routing

`EventSource` should bind emitter, topic0 filters, and a routing rule:

- `EventRoute::Direct`
- `EventRoute::IndexedAddress { topic_index: usize }`
- `EventRoute::IndexedBytes32 { topic_index: usize }`
- `EventRoute::AdapterDefined`

Topic indexes are indexes into `log.topics()`, so Balancer V2 pool ids use
`topic_index: 1` for `Swap(bytes32 indexed poolId, ...)`.

Provide constructors:

- `EventSource::direct(emitter, topics)`
- `EventSource::indexed_address(emitter, topics, topic_index)`
- `EventSource::indexed_bytes32(emitter, topics, topic_index)`
- `EventSource::adapter_defined(emitter, topics)`

Routing should only match a source when:

- `log.address` equals `source.emitter`
- the log has a `topic0`
- `source.topics` is empty or contains `topic0`
- the route can identify the registered pool key

`AdapterDefined` should be ignored by generic registry routing and left for
`AmmAdapter::route_log`.

### Registry

`AdapterRegistry` should own tracked pools. It should not infer tracked pools
from `EvmCache`.

Required behavior:

- `AdapterRegistry::new()`
- `register_pool(PoolRegistration) -> Result<(), RegistryError>`
- duplicate pool keys are rejected
- `pool(&PoolKey) -> Option<&PoolRegistration>`
- `pools() -> impl Iterator<Item = &PoolRegistration>`
- `route_log(&Log) -> Option<&PoolRegistration>`
- `subscription_topics() -> Vec<B256>` returns sorted, deduped topic0s

The registry should route direct and indexed-address/bytes32 event sources
without needing a protocol adapter. Adapter-defined routes can be added once
real protocol adapters exist.

### Storage Helpers

Move protocol storage constants/helpers into this crate now. The base slice
should include:

- `V2_RESERVES_SLOT`
- `V3_SLOT0_SLOT`
- `V3_LIQUIDITY_SLOT`
- `V3_TICKS_BASE_SLOT`
- `V3_TICK_BITMAP_BASE_SLOT`
- `PANCAKE_V3_LIQUIDITY_SLOT`
- `PANCAKE_V3_TICKS_BASE_SLOT`
- `PANCAKE_V3_TICK_BITMAP_BASE_SLOT`
- `SLIPSTREAM_SLOT0_SLOT`
- `SLIPSTREAM_LIQUIDITY_SLOT`
- `SLIPSTREAM_TICKS_BASE_SLOT`
- `SLIPSTREAM_TICK_BITMAP_BASE_SLOT`
- `V3StorageLayout`
- `v3_tick_bitmap_storage_key`
- `v3_tick_bitmap_storage_key_with_base`
- `v3_tick_info_storage_keys`
- `v3_tick_info_storage_keys_with_base`

Do not remove the old imports from `cache_sync` yet. Later phases will migrate
users from `evm_fork_cache::cache::*` to `crate::adapters::storage::*`.

### Cache Facade

Define `AdapterCache` over generic cache operations:

- `cached_storage`
- `apply_updates`
- `verify_slots`
- `purge_storage`
- `purge_slots`
- `read_storage_slot`
- `call_raw`

Implement it for `evm_fork_cache::cache::EvmCache` by delegating to the public
methods already exposed by `evm-fork-cache`.

The facade must use `StateUpdate` and `StateDiff` directly rather than creating
a new mutation-plan type.

## Acceptance Criteria

- The crate exposes `evm_amm_state::adapters`.
- Core adapter types compile without importing `amm_math`, `amms`, or
  simulation model types.
- `PoolKey` strongly distinguishes address-keyed pools from bytes32-keyed pools.
- `PoolRegistration` stores `state_addresses` and exact `event_sources`.
- `AdapterRegistry` routes direct pool events and Balancer-style
  bytes32-indexed vault events.
- `subscription_topics()` returns sorted, deduped topic0 signatures.
- Storage constants and V3 mapping-key helpers are available from this crate.
- `AdapterCache` is implemented for `EvmCache` using `StateUpdate`/`StateDiff`.
- Existing compatibility modules are not broadly rewritten.

## Test Plan

Manager-authored tests live in `tests/adapter_core.rs`:

- `pool_key_preserves_protocol_specific_identity`
- `registry_routes_direct_pool_events_by_emitter_and_topic`
- `registry_routes_bytes32_indexed_vault_events`
- `storage_layout_helpers_are_available_from_this_crate`

Expected red behavior before implementation: the test target fails to compile
because `evm_amm_state::adapters` does not exist.

Expected green behavior after implementation: the adapter core tests pass.

Broader verification:

```sh
cargo test --test adapter_core
cargo test --lib
cargo check
```

## Implementation Handoff

The implementation agent should add the new adapter core module and make the
manager-owned tests pass. It should not migrate existing protocol sync/event
logic yet and should not change the existing `cache_sync` or simulation paths
unless needed to expose the new module.

## Risks And Assumptions

- The registry uses `alloy_primitives::Log`, while existing simulation event
  code uses `alloy_rpc_types_eth::Log`. This is intentional for the new
  adapter/cache boundary.
- `ProtocolId::Custom(&'static str)` and `CustomPoolKey` are acceptable for this
  base phase. If runtime-owned custom protocol ids are needed, that can be
  revisited before public release.
- `ProtocolMetadata::Custom` can use `Arc<dyn Any + Send + Sync>` and therefore
  should not derive `Serialize` in the base phase.
- Cold-start and repair types are vocabulary only in A0. Real V2/V3 cold-start
  implementations are later phases.
