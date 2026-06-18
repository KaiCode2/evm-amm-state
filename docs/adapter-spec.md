# AMM Adapter Spec

Status: draft for Phase A0 review.

This document specifies the first implementation target for the adapter layer in
`evm-amm-state`.

Code snippets are target API sketches. They should guide implementation, but
module paths, derives, and object-safety details can be adjusted during the
implementation phase.

The guiding separation is:

- `evm-fork-cache` owns generic EVM state, cache lifecycle, execution,
  snapshots, storage/account updates, freshness, and log ingestion mechanics.
- `amm-math` owns pure Rust AMM simulation math.
- `evm-amm-state` owns protocol semantics: how AMM contracts are identified,
  initialized, subscribed to, updated from events, repaired after missed data,
  and optionally converted into pure Rust simulation models.

The adapter layer must build without `amm-math`.

## Core Decision

Adapters are instructional protocol objects. They know AMM layouts, events, and
repair rules. They do not own RPC transport, fork lifecycle, EVM execution, or
cache persistence.

For cache mutations, adapters should emit `evm_fork_cache::StateUpdate` values.
`evm-fork-cache` already has the generic writer vocabulary:

- `StateUpdate::Slot`
- `StateUpdate::SlotMasked`
- `StateUpdate::SlotDelta`
- `StateUpdate::BalanceDelta`
- `StateUpdate::Account`
- `StateUpdate::AccountUpsert`
- `StateUpdate::Purge`

So this crate should not introduce a competing generic `StateMutationPlan`.
When this document says "cache update", it means a protocol-derived
`Vec<StateUpdate>` plus any adapter-level repair policy.

For cache reads, this crate does need protocol-owned cold-start and repair
instructions. Reads are protocol-specific: Uniswap V2 wants reserves and
immutable token metadata, Uniswap V3 wants slot0, liquidity, tick layout, tick
snapshots, and optionally tick words, Balancer wants vault/pool metadata. These
read recipes live in `evm-amm-state` and execute through a narrow cache facade.

### Why Not `StateReadPlan` / `StateMutationPlan`?

The earlier "read plan / mutation plan" language was too generic.

The finalized cache crate already provides the mutation plan: `StateUpdate`.
Re-wrapping it here would create two vocabularies for the same thing and make
custom adapters harder to write.

Reads are different. A cold-start or repair read is not just "read slot X":
it often has protocol branching, metadata hydration, validation, and follow-up
repair. For example, V3 reads fresh `slot0` and liquidity, compares them to a
tick snapshot, then chooses snapshot reuse, targeted repair, incremental repair,
or full resync. That belongs in this crate as adapter logic, not as a generic
cache primitive.

So the split should be:

- event/cold-start mutations: emit `Vec<StateUpdate>`
- cold-start and repair reads: adapter recipes executed through `AdapterCache`
- batching/concurrency/retries/persistence: owned by `evm-fork-cache`

The result still matches the "adapters give orders to the cache" model: the
orders are generic cache operations plus `StateUpdate`s, while the logic deciding
which orders to issue lives in the adapter.

## Adapter Setup

Adapter setup has two distinct layers:

1. Protocol adapters

   A protocol adapter is stateless or mostly-static code for one protocol family.
   Examples: `UniswapV2Adapter`, `UniswapV3Adapter`, `BalancerV2Adapter`.

   It knows:

   - protocol id
   - event ABI and topics
   - storage layout
   - cold-start recipe
   - event-to-cache update logic
   - repair policy
   - optional classification rules

2. Tracked pools

   A tracked pool is a per-pool registration owned by this crate. It is not an
   `EvmCache` concept.

   It records:

   - strongly typed pool key
   - protocol id
   - state addresses the cache may need to mutate, purge, or verify
   - event sources, including emitter address, topics, and routing rule
   - optional vault/factory addresses
   - token addresses and immutable protocol metadata once known
   - protocol storage layout
   - cold-start/sync status
   - optional protocol snapshot metadata, such as V3 tick snapshots

Recommendation: adapter registrations and protocol metadata live in an
`evm-amm-state` sidecar store, not inside `EvmCache`.

`EvmCache` should persist generic EVM state only. This keeps
`evm-fork-cache` protocol-free after its `protocols` feature is removed. A user
can reconstruct the adapter registry from explicit config, discovery output, or
classification against cached bytecode/storage. Once reconstructed, adapters can
hydrate missing protocol metadata from cache or chain reads.

## Public Concepts

The initial adapter module should introduce these protocol-neutral concepts.

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ProtocolId {
    UniswapV2,
    UniswapV3,
    PancakeV3,
    Slipstream,
    SolidlyV2,
    BalancerV2,
    BalancerV3,
    Curve,
    Erc4626,
    UniswapV4,
    Custom(&'static str),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum PoolKey {
    UniswapV2(Address),
    UniswapV3(Address),
    PancakeV3(Address),
    Slipstream(Address),
    SolidlyV2(Address),
    BalancerV2(B256),
    BalancerV3(Address),
    Curve(Address),
    Erc4626(Address),
    UniswapV4(B256),
    Custom(CustomPoolKey),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum CustomPoolKey {
    Address {
        protocol: &'static str,
        address: Address,
    },
    Bytes32 {
        protocol: &'static str,
        id: B256,
    },
    Composite {
        protocol: &'static str,
        address: Address,
        id: B256,
    },
}

#[derive(Clone, Debug)]
pub struct EventSource {
    pub emitter: Address,
    pub topics: Vec<B256>,
    pub route: EventRoute,
}

#[derive(Clone, Debug)]
pub enum EventRoute {
    /// The log belongs to this pool when `log.address()` equals the emitter.
    Direct,
    /// The log carries an indexed address identifying the pool.
    IndexedAddress { topic_index: usize },
    /// The log carries an indexed bytes32 key identifying the pool.
    IndexedBytes32 { topic_index: usize },
    /// Routing cannot be expressed generically; delegate to the adapter.
    AdapterDefined,
}

pub struct PoolRegistration {
    pub key: PoolKey,
    pub state_addresses: Vec<Address>,
    pub event_sources: Vec<EventSource>,
    pub metadata: ProtocolMetadata,
    pub status: PoolStatus,
}
```

`PoolKey` should be strongly typed because protocols do not identify pools the
same way. Uniswap V2/V3-style pools are address-keyed. Balancer V2 and Uniswap
V4 are bytes32-keyed. Encoding this distinction in the type prevents accidental
address assumptions in routing, cache repair, and simulation construction.

`state_addresses` are addresses whose EVM storage may be mutated, verified, or
purged for this pool. For most pools this is one pool contract. For vault-style
protocols it may include a vault, pool contract, or manager.

`event_sources` describe where relevant logs come from and how to route them.
For normal pools this is usually one direct source: pool address plus pool event
topics. For vault/manager protocols, the emitter may be shared by many pools and
the route extracts a `B256` or address from an indexed topic.

Examples:

- Uniswap V2: `PoolKey::UniswapV2(pair)`, one direct event source at `pair`.
- Uniswap V3: `PoolKey::UniswapV3(pool)`, one direct event source at `pool`.
- Balancer V2: `PoolKey::BalancerV2(pool_id)`, vault event source routed by
  indexed `poolId`; optionally a pool-contract source for protocol config
  events if the adapter tracks them.
- Uniswap V4: `PoolKey::UniswapV4(pool_id)`, PoolManager event source routed by
  indexed pool id.
- ERC4626: vault event source for `Deposit`/`Withdraw`; optionally asset/share
  token sources if an adapter elects to track token-transfer-derived state.

Factory events should be represented separately as discovery sources. They are
not per-pool event sources because they create new registrations rather than
updating an existing pool's state.

`ProtocolMetadata` should be an enum owned by this crate:

```rust
pub enum ProtocolMetadata {
    Unknown,
    UniswapV2(UniswapV2Metadata),
    UniswapV3(V3Metadata),
    BalancerV2(BalancerV2Metadata),
    // ...
    Custom(Arc<dyn Any + Send + Sync>),
}
```

The concrete metadata structs replace the AMM-specific metadata that currently
lives in `evm-fork-cache`, for example V2 metadata, V3 metadata, storage layout,
and V3 tick snapshot metadata.

## Cache Facade

Adapters should not depend on `EvmCache` internals. They need a small facade
that can be implemented for `EvmCache` and mocked in tests.

```rust
pub trait AdapterCache {
    fn cached_storage(&self, address: Address, slot: U256) -> Option<U256>;

    fn apply_updates(&mut self, updates: &[StateUpdate]) -> StateDiff;

    fn verify_slots(&mut self, slots: &[(Address, U256)]) -> anyhow::Result<Vec<SlotChange>>;

    fn purge_storage(&mut self, address: Address) -> StateDiff;

    fn purge_slots(&mut self, address: Address, slots: &[U256]) -> StateDiff;

    fn read_storage_slot(&mut self, address: Address, slot: U256) -> anyhow::Result<U256>;

    fn call_raw(
        &mut self,
        from: Address,
        to: Address,
        calldata: Bytes,
        commit: bool,
    ) -> anyhow::Result<ExecutionResult>;
}
```

The actual trait can be refined during implementation. The important boundary is
that adapters ask for generic cache actions and emit generic cache mutations.
They do not reach into backend maps, overlay maps, or persistence internals.

For async account prewarming, either add an async facade method:

```rust
async fn ensure_account(&mut self, address: Address) -> anyhow::Result<()>;
```

or keep account prewarming in the orchestration layer so the core adapter trait
stays synchronous. The current `EvmCache::ensure_account` is async, while most
event and cold repair operations are synchronous.

## Adapter Trait

The initial trait should model both setup-time and hot-path behavior.

```rust
pub trait AmmAdapter: Send + Sync {
    fn protocol(&self) -> ProtocolId;

    fn event_sources(&self, pool: &PoolRegistration) -> Vec<EventSource>;

    fn route_log(&self, log: &Log, registry: &AdapterRegistry) -> Option<PoolKey>;

    fn cold_start(
        &self,
        pool: &mut PoolRegistration,
        cache: &mut dyn AdapterCache,
        policy: ColdStartPolicy,
    ) -> anyhow::Result<ColdStartOutcome>;

    fn decode_event(
        &self,
        pool: &PoolRegistration,
        log: &Log,
        view: &dyn evm_fork_cache::events::StateView,
    ) -> AdapterEventResult;

    fn after_apply(
        &self,
        pool: &PoolRegistration,
        event: &AdapterEvent,
        diff: &StateDiff,
    ) -> RepairAction {
        RepairAction::None
    }
}
```

Notes:

- `route_log` exists because `log.address()` is not always the pool address.
  Balancer vault events and Uniswap V4 manager events need protocol-aware
  routing.
- `decode_event` should perform no RPC and should not mutate the cache. It
  decodes the event and returns desired `StateUpdate`s and semantic metadata.
- `after_apply` lets the adapter inspect skipped masked/delta updates from
  `StateDiff`. This is where cold slots become repair work instead of silent
  drift.
- For compatibility with `evm-fork-cache`, adapters may also expose an
  `EventDecoder` implementation, but the primary AMM event driver should keep
  adapter-level semantic results and repair hooks.

## Adapter Registry

`AdapterRegistry` owns tracked pools and protocol adapters.

Responsibilities:

- register protocol adapters
- register tracked pools
- route logs to `PoolKey`
- build subscription filters
- expose pool metadata to search/simulation layers
- persist and restore tracked pool metadata through an `evm-amm-state` sidecar
  format

```rust
pub struct AdapterRegistry {
    adapters: HashMap<ProtocolId, Arc<dyn AmmAdapter>>,
    pools: HashMap<PoolKey, PoolRegistration>,
    by_emitter: HashMap<Address, Vec<(PoolKey, EventSource)>>,
}
```

This registry should be constructible from:

- explicit user registrations
- TOML/config entries
- factory discovery results
- bytecode/storage classification
- a sidecar registry file owned by this crate

It should not be reconstructed from `EvmCache` alone. The cache may hold enough
raw state to classify some contracts, but the set of tracked pools is a user
choice and belongs outside the cache.

## Cold Start

Cold start means bringing one tracked pool into a state where:

- relevant cache slots are current enough for EVM simulation
- immutable metadata needed by the adapter is known
- protocol snapshot metadata is either valid, repaired, or marked deferred
- event processing can safely begin

Cold start should be explicit and inspectable.

```rust
pub enum ColdStartPolicy {
    Strict,
    Eager,
    Lazy,
    HotSlotsOnly,
}

pub enum ColdStartOutcome {
    Ready(ColdStartReport),
    ReadyWithDeferred(ColdStartReport, Vec<DeferredWork>),
    NeedsRepair(ColdStartReport, RepairAction),
    Unsupported(UnsupportedReason),
}
```

Policy meanings:

- `Strict`: fail if all state needed for exact updates cannot be validated.
- `Eager`: perform expensive sync immediately, such as V3 bitmap/tick scans.
- `Lazy`: validate hot state now and defer expensive state until needed.
- `HotSlotsOnly`: only initialize slots required for event-driven hot updates.
  This is useful for adapter-only users that do not need local AMM simulation.

Cold start should use the cache's generic operations:

- account prewarm
- cached storage inspection
- slot purge
- slot read
- view call
- `verify_slots`
- `apply_updates`

It should not use protocol-specific methods on `EvmCache`.

### Uniswap V2 Cold Start

Inputs:

- pool address
- fee, if not inferable
- optional cached metadata: token0, token1, last reserve timestamp

Steps:

1. Ensure or lazily load account.
2. Hydrate immutable metadata:
   - if sidecar metadata has token0/token1, use it
   - otherwise call `token0()` and `token1()` through the cache
3. Force fresh reserves:
   - purge the reserves slot, or
   - call `getReserves()` and compile the result into a reserve slot write
4. Apply:
   - `StateUpdate::Slot { address: pool, slot: V2_RESERVES_SLOT, value: packed }`
5. Update sidecar metadata:
   - tokens
   - last block timestamp from `getReserves()`, if fetched

Event handling:

- `Sync(uint112,uint112)` is exact.
- It compiles directly to one absolute reserve slot update.
- No repair is needed unless the slot write fails at the cache layer.

### Uniswap V3 Cold Start

Inputs:

- pool address
- V3 flavor/layout
- optional metadata: token0, token1, fee, tick spacing
- optional tick snapshot from this crate's sidecar store

Steps:

1. Ensure or lazily load account.
2. Hydrate immutable metadata:
   - token0
   - token1
   - fee
   - tick spacing
3. Read fresh hot state:
   - purge/read or verify `slot0`
   - purge/read or verify global liquidity
4. Compare fresh hot state against stored V3 snapshot metadata:
   - if no snapshot exists, choose full tick sync or deferred tick sync
   - if liquidity differs, missed Mint/Burn is assumed
   - if current tick drift exceeds policy threshold, snapshot is stale
   - if liquidity matches and tick drift is acceptable, snapshot can be reused
5. Repair or defer tick state:
   - eager policy: prefetch bitmap/tick slots and inject/refresh snapshot
   - lazy policy: mark deferred full/incremental tick repair
   - hot-slots policy: only keep slot0/liquidity current
6. Apply fresh hot slots with `StateUpdate`.

Event handling:

- `Swap` is exact from event data:
  - masked `slot0` update for sqrt price and tick
  - absolute liquidity slot update
- `Mint`/`Burn` is exact only if needed tick/global-liquidity pre-state is hot:
  - recompute tick liquidityGross/liquidityNet
  - update initialized flag
  - update tick bitmap
  - update global liquidity if the current tick is inside the affected range
- If needed pre-state is cold, do not invent zero values. Emit repair:
  - targeted tick-range resync when tick range is known
  - incremental resync when only liquidity mismatch is known
  - full resync when no useful snapshot exists

### Balancer Cold Start

Inputs:

- pool address
- vault address
- pool id
- optional token list, weights, fees

Steps:

1. Hydrate pool/vault metadata:
   - `getPoolTokens(poolId)`
   - pool weights/fee where applicable
2. Identify cache addresses affected by events:
   - event emitter is the vault
   - state may live under vault storage, not the pool address
3. Initialize balances and metadata.
4. If exact vault storage slot mapping is known, write/verify targeted slots.
5. Otherwise mark conservative invalidation policy for vault storage.

Event handling:

- `Swap(poolId, tokenIn, tokenOut, amountIn, amountOut)` routes by `poolId`.
- If vault balance slots are known and hot, compile to balance deltas or absolute
  slot writes.
- If exact storage mapping is unknown, use conservative purge/repair of the vault
  storage affected by the pool.

## Event Result Model

`decode_event` returns a structured event result instead of just raw updates.

```rust
pub struct AdapterEventResult {
    pub event: Option<AdapterEvent>,
    pub error: Option<AdapterEventError>,
}

pub struct AdapterEvent {
    pub pool: PoolKey,
    pub emitter: Address,
    pub topic0: B256,
    pub kind: AdapterEventKind,
    pub updates: Vec<StateUpdate>,
    pub quality: UpdateQuality,
    pub repair: RepairAction,
}

pub enum AdapterEventKind {
    Swap,
    LiquidityAdded,
    LiquidityRemoved,
    Sync,
    Deposit,
    Withdraw,
    Unknown,
}

pub enum UpdateQuality {
    Exact,
    ExactIfApplied,
    RequiresRepair,
    ConservativeInvalidation,
    Ignored,
}
```

`ExactIfApplied` is important for cold-aware updates. A V3 Mint may produce
correct `StateUpdate`s, but if `StateDiff` reports skipped masks because a tick
word was cold, the update was not actually applied. The adapter driver's
post-apply step must inspect the diff and schedule repair.

## Repair Model

Repairs are adapter-level follow-up work. They can be produced during cold start,
event decode, or post-apply diff inspection.

```rust
pub enum RepairAction {
    None,
    VerifySlots(Vec<(Address, U256)>),
    PurgeStorage(Address),
    PurgeSlots { address: Address, slots: Vec<U256> },
    ColdStart { pool: PoolKey, policy: ColdStartPolicy },
    V3TickRange {
        pool: PoolKey,
        tick_lower: i32,
        tick_upper: i32,
    },
    V3Incremental {
        pool: PoolKey,
    },
    V3Full {
        pool: PoolKey,
    },
    Custom(Box<dyn Any + Send + Sync>),
}
```

The initial implementation can support only:

- `None`
- `VerifySlots`
- `PurgeStorage`
- `PurgeSlots`
- V3 targeted/incremental/full repairs

Repairs should return a `RepairReport` describing:

- slots verified
- slots changed
- slots purged
- whether the pool is exact, degraded, or disabled
- any deferred work

## Event Driver

This crate should provide an AMM-specific event driver instead of using
`evm-fork-cache::EventPipeline` directly as the main API.

The cache pipeline is generic and useful, but it only knows `EventDecoder ->
StateUpdate -> StateDiff`. The AMM layer also needs:

- pool routing where `log.address()` is not the pool
- semantic update kind
- pool-level repair decisions
- pool status changes
- optional notification to search/simulation layers

Recommended flow:

```text
Log
  -> AdapterRegistry route
  -> AmmAdapter::decode_event(log, StateView)
  -> EvmCache::apply_updates(event.updates)
  -> AmmAdapter::after_apply(event, StateDiff)
  -> optional repair execution
  -> AdapterEventReport
```

The driver must process logs in canonical order within a block. Later logs in
the same block must decode against the cache state produced by earlier logs.

Reorg handling should purge or repair all pools touched after the reorg point,
then let the caller replay canonical logs. The default reorg purge can be
conservative:

- pool-emitted protocols: purge pool storage
- vault-emitted protocols: purge vault storage or known affected vault slots

## Subscription Model

Adapters define event sources; the registry builds filters.

For simple protocols:

```text
emitter = pool address
topic0 = protocol topics
route = Direct
```

For vault/manager protocols:

```text
emitter = vault or manager
topic0 = protocol topics
route = IndexedBytes32 or IndexedAddress
```

The public API should expose:

```rust
pub struct SubscriptionSpec {
    pub sources: Vec<EventSource>,
}
```

and optionally a compact grouped filter representation for provider APIs.

This is intentionally more exact than a raw `Vec<Address>`:

- a direct pool source subscribes to only that pool's topics
- a Balancer vault source subscribes to vault topics and routes by indexed pool id
- a Uniswap V4 source subscribes to PoolManager topics and routes by indexed pool
  id
- a future custom source can fall back to `AdapterDefined` routing without
  weakening the common cases

## Sidecar Store

This crate should own a sidecar store for adapter metadata. Initial persistence
can be JSON, TOML, or bincode; the format can be stabilized later.

It should store:

- tracked pool keys
- protocol id
- event sources
- vault/factory addresses
- immutable metadata
- storage layout
- V3 tick snapshots or references to snapshot data
- last successful cold-start status

It should not store:

- generic account state
- generic storage state
- block pinning state
- EVM snapshots

Those remain in `evm-fork-cache`.

## Classification

The adapter layer should expose optional helpers to create a `PoolRegistration`
from an address.

Classification inputs:

- explicit protocol hint from caller
- bytecode hash or bytecode matcher
- factory metadata
- storage shape checks
- successful protocol view calls

Classification output:

```rust
pub enum Classification {
    Known(PoolRegistration),
    Unknown(Address),
    Ambiguous(Vec<ProtocolId>),
    Unsupported(UnsupportedReason),
}
```

Classification must be optional. Many users will register pools explicitly and
skip bytecode probing.

## Feature Flags

Initial adapter features:

- `adapters`: core traits, registry, event driver, cold-start/repair model
- `uniswap-v2`: Uniswap V2 adapter
- `uniswap-v3`: Uniswap V3 adapter
- `pancake-v3`: Pancake V3 layout flavor
- `balancer-v2`: Balancer V2 adapter
- `discovery`: factory discovery
- `simulation`: construction of `amm-math`/`amms` simulation models
- `search`: route/search logic over simulation snapshots

The `adapters` feature must not require `amm-math` or `amms`.

## Initial Implementation Slice

The first implementation should be deliberately narrow:

1. Add core adapter types:
   - `ProtocolId`
   - `PoolKey`
   - `EventSource`
   - `EventRoute`
   - `PoolRegistration`
   - `AdapterRegistry`
   - `AmmAdapter`
   - `AdapterCache`
   - `AdapterEvent`
   - `RepairAction`
   - `ColdStartPolicy`

2. Move protocol-neutral V2/V3 storage constants and helpers into this crate.

3. Implement `AdapterCache` for `evm_fork_cache::cache::EvmCache`.

4. Implement Uniswap V2 adapter:
   - cold start metadata/reserves
   - `Sync` event -> reserve slot update
   - no `amm-math` dependency

5. Implement Uniswap V3 adapter:
   - metadata/layout hydration
   - slot0/liquidity cold start
   - V3 snapshot validation decision
   - `Swap` -> masked slot0 + liquidity
   - `Mint`/`Burn` -> exact tick updates when hot, repair when cold

6. Implement a minimal AMM event driver:
   - route
   - decode
   - apply
   - post-apply repair decision
   - per-block report

7. Add Balancer V2 routing spec/tests before full implementation:
   - vault emitter
   - pool id route
   - conservative repair when exact storage mapping is not available

## Acceptance Criteria

Phase A0 is accepted when:

- The adapter/cache boundary is clear enough to remove the `protocols` feature
  from `evm-fork-cache`.
- No AMM protocol metadata type is required in `evm-fork-cache`.
- Adapter-only usage does not depend on `amm-math`.
- A custom downstream adapter can:
  - register pools
  - subscribe to events
  - decode logs into cache updates
  - request repair when updates cannot be computed
- Uniswap V2 and Uniswap V3 can be expressed with the same trait shape.
- Balancer-style vault routing can be represented without special casing in the
  event driver.

## Open Questions

1. Should the cold-start facade be async, or should account prewarming and
   provider-backed calls stay in an orchestration layer?
2. Should the sidecar metadata store be part of the first implementation, or
   should initial registrations be in-memory only?
3. Should V3 tick snapshots be persisted in this crate directly, or should this
   crate define snapshot data while callers decide persistence?
4. Should `evm-fork-cache::EventPipeline` gain extension hooks, or should
   `evm-amm-state` own a separate AMM event driver from the start?
5. What is the default V3 cold-start policy for adapter-only users:
   `HotSlotsOnly`, `Lazy`, or `Eager`?
