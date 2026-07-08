# evm-amm-state Roadmap

This document is a draft implementation roadmap for shaping `evm-amm-state`
into the protocol-aware AMM layer between:

- `evm-fork-cache`: EVM state, fork/cache lifecycle, chain synchronization,
  batched reads/writes, and EVM simulation execution.
- `amm-math`: pure Rust AMM math and deterministic pool interaction
  simulation.
- `evm-amm-state`: protocol semantics, cache orchestration, AMM adapters,
  discovery, and optional search-oriented simulation models.

The roadmap is split into two deliverables:

1. **Adapters and cache orchestration**
2. **Simulation and search logic**

No phase below should be treated as final API design. Each phase has a review
checkpoint before implementation.

## Release Plan

### 0.1.0 (current — release candidate)

The adapters-and-cache-orchestration deliverable, feature-complete for five
protocol families (Uniswap V2, the concentrated-liquidity family — Uniswap V3 /
PancakeSwap V3 / Slipstream — Balancer V2, Solidly V2, and Curve) plus the
offline `simulate_swap` surface — **including declarative factory discovery**,
which was originally slated for 0.2.0 and shipped early (design in
[docs/factory-discovery-spec.md](docs/factory-discovery-spec.md)):
`PoolDiscovery::{find, find_many}` over a fluent `PoolQuery` resolves pools
derive-first in one batched read across Uniswap V2 / the CL family / Solidly,
with optional CREATE2 cross-checks, creation-event decoding
(`DiscoverySource::CreationEvent`), a defaulted per-adapter
`AmmAdapter::pool_factories(&FactoryConfig)` hook, and first-class escape
hatches (`register_adapter`, `PoolDiscovery::with_factory`). Reactive sync
event-sources V2/Solidly `Sync`, V3 `Mint`/`Burn` onto warm ticks, and
Balancer vault `Swap`s onto probed cash fields; Balancer/Curve cold-starts
take a verify-only fast path once their read-set is known, and
`cold_start_many` bundles every one-shot-eligible pool into one hydration
call.

### 0.1.x / 0.2.0 candidates (post-release)

- **Access-list two-shot cold-start warming** — `eth_createAccessList` fast
  first boot + `cold_start_primed` (implemented on PR #32; deliberately held
  out of 0.1.0 to keep the release frozen; first candidate to land after).
- **Balancer V2 discovery** via an async Vault `PoolRegistered` log-scan
  helper (Balancer has no on-chain token→pool index).
- **Curve discovery** via an in-EVM MetaRegistry `find_pools_for_coins`
  view call (dropped from 0.1.0: the live registry call reverted under the
  pinned-cache harness; needs its own transport shape).
- **Balancer `PoolBalanceManaged`**: subscribe + resync the asset-manager
  cash↔managed rebalance event. Today the cash probe refuses managed fields
  (a managed pool resyncs instead of event-sourcing), so exposure is
  negligible — subscribing closes it fully.
- **Algebra-style CL forks** (Camelot / QuickSwap): a different pool engine
  (dynamic fees, `globalState` packing, `tickTable`) — a new adapter, not a
  discovery config.
- **Discovery-integrated `Bootstrapper`**: pair bulk discovery with
  `cold_start_many` behind strategy knobs
  (docs/high-performance-bootstrap-defaults.md).
- **Incremental interests refresh in `AmmSyncEngine`** — the upstream API
  already shipped in `evm-fork-cache` 0.2 (`ReactiveRuntime::unregister_handler`
  plus per-owner `add_interest_owner(_with_backfill)` / `remove_interest_owner`
  / `sync_handler_interests` on the subscription engine); what remains is
  adoption **here**: `register_pools` / `replace_registry` still construct a
  fresh `ReactiveRuntime` per registry change, discarding accumulated reorg
  tracking between batches. Refresh the `AmmReactiveHandler` registration on
  the live runtime instead, and thread backfill-aware owner updates through
  for subscriber-driven callers.
- **Output-side liquidity / capacity accessor for coarse route pruning** —
  expose, per pool, a *sound upper bound* on how much of a given `token` the
  pool can pay out, so a downstream graph search can drop AMMs that blatantly
  lack the depth for a candidate route before spending a quote on them. The
  bound is the pool's **balance of the output token** (constant-product /
  weighted / stable curves all approach `reserve/balance_out` as input → ∞; V3
  is bounded by the pool's `balanceOf`), which is a pure **state read, not AMM
  math**. Deliberately a capacity *bound*, not an exact max-output: prune when
  `bound < needed_out` and no viable pool is ever wrongly dropped (a loose bound
  only under-prunes). Composes with `simulate_swap` as a two-tier filter (cheap
  capacity prune → exact quote on the survivors), keeping slippage math out of
  the crate. Held out of 0.1.0 — the search crate owns this heuristic for now;
  upstreaming it here would centralize the per-protocol reserve decode (DRY)
  once the shape settles.
  - *Shape*: an adapter method over warmed state (sibling to `simulate_swap`),
    e.g. `reserves(pool, &dyn StateView) -> Option<Vec<(Address, U256)>>` or
    `output_capacity(pool, token, &dyn StateView) -> Option<U256>`; `None` when
    the needed state is not warm.
  - *Per-protocol effort*: V2 / Solidly / Balancer are easy — reserves / the
    112-bit cash field are already in the warmed quote read-set and the decode
    already exists. Curve needs its balances labeled (the `get_dy`
    `discovered_slots` set is unlabeled) or a `balances(i)` /
    `coin.balanceOf(pool)` read. V3 has **no** native reserve slot (it tracks
    virtual `liquidity`), so a sound bound needs `token.balanceOf(pool)` — which
    lives in the token contract, *outside* the quote read-set (an extra slot to
    warm, or a prune-time call); in-range `liquidity` is only a *lower* bound
    and is unsafe to prune on.
  - *Design fork to settle first*: (a) native reserves where already hot — free
    for V2/Solidly/Balancer, but partial (V3 `None`, Curve needs labeling) — vs
    (b) a uniform `token.balanceOf(pool)` bound (uniform + sound, but must warm
    token-balance slots, and Balancer special-cases to its tracked cash since
    the Vault commingles all pools' tokens). Choice hinges on whether the search
    crate's warm set already includes pool token balances.
  - *Explicit non-goals*: exact max output (= the AMM math the crate refuses,
    especially V3's tick-by-tick `L·Δ√P` integral) and "max **input**"
    (ill-defined — input is unbounded with diminishing output, so the cap is a
    slippage tolerance the search crate owns).
- **`StateView`-aware quote-target warming** — 0.1.0's eager `cold_start_many`
  pre-warms the *metadata-derivable* quote entrypoints (Uniswap V3 `QuoterV2`,
  V2 `Router02`, Balancer vault) via `ProtocolMetadata::quote_code_targets`.
  Extend it to the two quote-path dependencies it can't name from metadata
  alone: Solidly V2's `PoolFactory` (the `getFee()` STATICCALL target, held in
  *pool storage*) and a Curve NG pool's `DELEGATECALL`ed math/views
  implementation. Both need a read of already-warmed state to resolve the
  address, so the clean shape is a `StateView`-aware resolver (sibling to
  `quote_code_targets`) run after hydration. Pairs with a defaulted
  `AmmAdapter::quote_code_targets` override so custom adapters can pre-warm
  their own quote entrypoints (alongside the deferred `AmmAdapter::tokens`
  override). Until then these stay a one-time lazy first-quote fetch — correct,
  just not pre-warmed.

## Scope

### This crate should own

- Protocol-specific AMM adapters.
- Event topic registration and log decoding.
- Event-to-cache update logic.
- Cold start sync planning and stale-cache validation.
- Known-pool classification from address, bytecode, storage shape, or factory
  metadata.
- Factory discovery for new pools.
- Optional construction of pure Rust AMM simulation models.
- Optional route/search utilities over immutable AMM snapshots.

### This crate should not own

- RPC transport policy, retry policy, or provider failover beyond expressing
  what data is needed.
- Fork lifecycle, block pinning, cache persistence, or EVM execution.
- Transaction signing, calldata routing execution, bundle submission, or relay
  integration.
- Strategy scheduling, alerting, portfolio management, or bot orchestration.
- Raw AMM math that belongs in `amm-math`.
- General chain indexing.

## Feature Model

The final feature model should allow users to opt into only the layers they
need.

Proposed feature groups:

- `adapters`: protocol adapters, event decoding, sync planning, and cache
  update orchestration. This should not require `amm-math`.
- `simulation`: pure Rust AMM model construction from cache-backed protocol
  state. This enables `amm-math` and any required simulation model crates.
- `search`: route construction, sizing, arbitrage search, and parallel
  evaluation. This depends on `simulation`.
- `discovery`: factory-based pool discovery.
- `toml`: optional config file parsing.

Proposed protocol features:

- `uniswap-v2`
- `uniswap-v3`
- `pancake-v3`
- `slipstream`
- `solidly-v2`
- `balancer-v2`
- `balancer-v3`
- `curve`
- `erc4626`
- `uniswap-v4`

Review decision:

- Whether `default` should be minimal (`adapters`) or ergonomic
  (`adapters`, common protocols, `toml`).
- Whether protocol features should gate both adapters and simulation support, or
  whether each protocol needs separate `*-adapter` and `*-simulation` features.

## Deliverable 1: Adapters And Cache Orchestration

Goal: make this crate the protocol semantics layer for AMM-aware cache
synchronization, without requiring `amm-math`.

### Architecture Update (post-A1)

Two decisions made during A0/A1 implementation supersede parts of the phase
text below:

- **No bespoke plan vocabulary.** Adapters emit `evm_fork_cache::StateUpdate`
  values directly for cache mutations and read through a narrow cache facade
  (`AdapterCache: StateView`). The earlier `CacheReadPlan` / `CacheMutationPlan`
  language is dropped — see `docs/adapter-spec.md` ("Why Not StateReadPlan /
  StateMutationPlan?").
- **Reactive runtime as the execution surface.** `evm-fork-cache` now exposes a
  reactive runtime (`ReactiveHandler` / `ReactiveEffect` / resync + reorg
  recovery). This crate bridges adapters into it via `AmmReactiveHandler`
  instead of building a standalone executor and event router. This reshapes
  A2 and A5 (see those phases).

Status: A0 and A1 are implemented (PR #2). A2 is the current target.

### Phase A0: Boundary And API Contract

Define the precise interface between `evm-amm-state` and `evm-fork-cache`.

Deliverables:

- Document which low-level capabilities `evm-fork-cache` exposes:
  account reads, storage reads, storage writes, slot patching, batch reads,
  cache invalidation, observations, immutable metadata, and EVM calls.
- Document which protocol-specific concepts move out of `evm-fork-cache`:
  AMM slot constants, V2/V3 metadata structs, tick snapshot structs, protocol
  storage-key helpers, and AMM-specific injection helpers.
- Decide whether adapters directly mutate `EvmCache` or produce cache mutation
  plans that `evm-fork-cache` executes.

Resolved direction:

- Adapters emit `evm_fork_cache::StateUpdate` for mutations and an
  `AdapterEvent` + `RepairAction` for semantics; they do not define a second
  mutation-plan type.
- `evm-fork-cache` (via its reactive runtime) executes the resulting effects
  and owns concurrency, batching, retries, resync, and persistence.

Acceptance criteria:

- A short design note defines the boundary clearly enough that both crates can
  evolve independently.
- No protocol-specific AMM state type is required in `evm-fork-cache`.

Review checkpoint:

- Approve the adapter-to-cache interface before moving code.

### Phase A1: Core Adapter Abstractions

Introduce protocol adapter traits and common data types.

Deliverables:

- `ProtocolId` enum.
- `PoolKey` / `CustomPoolKey` model, incl. bytes32 keys for vault-emitted events.
- `AmmAdapter` trait, covering:
  - protocol identity
  - event topics
  - log target routing
  - event decoding
  - cold-start sync planning
  - stale-cache validation
  - cache mutation via emitted `StateUpdate`s
- `AdapterCache: StateView` facade for account/storage reads and view calls.
- `AdapterEvent` + `UpdateQuality` + `RepairAction` as the normalized event
  result, independent of simulation models.
- `AmmReactiveHandler` bridging the registry into the reactive runtime, plus a
  synchronous `AdapterDriver` for caller-ordered application.
- Error types that distinguish decode errors, unsupported protocol state,
  stale cache, and missing cache data.

Acceptance criteria:

- A Uniswap V2 adapter can be expressed without depending on `amm-math` or
  `amms`.
- The abstraction can represent Balancer vault-emitted events where
  `log.address()` is not the pool address.

Review checkpoint:

- Confirm trait shape with examples for V2, V3, and Balancer before broad
  migration.

### Phase A2: Cache Plan Executor Integration

Connect adapter reports and repair actions to a cache execution policy on top
of the reactive runtime. The `AmmReactiveHandler` bridge already emits
`StateUpdate`s, hash-pinned resyncs, invalidations, and repair hooks; A2 makes
the repair side concrete and complete.

Deliverables:

- Implement an executor or thin bridge that applies `CacheMutationPlan` to
  `EvmCache`.
- Preserve the current hot-state behavior:
  - V2 reserve slot injection.
  - V3 slot0 patching while preserving observation fields.
  - V3 liquidity slot injection.
  - V3 tick bitmap/tick info injection after liquidity events.
  - Fallback purges for protocols where exact slot injection is not available.
- Preserve or replace `SlotObservationTracker` integration.

Acceptance criteria:

- Event-driven V2 and V3 updates can update cache state without RPC.
- Protocol adapters do not reach into `evm-fork-cache` internals beyond the
  approved public API.

Review checkpoint:

- Decide whether observation tracking belongs entirely in `evm-fork-cache` or
  whether this crate supplies protocol hints.

### Phase A3: Protocol Migration From Current Code

Move current protocol-specific sync/event logic into adapter modules.

Suggested module shape:

```text
src/adapters/
  mod.rs
  traits.rs
  plans.rs
  uniswap_v2.rs
  uniswap_v3.rs
  pancake_v3.rs
  slipstream.rs
  solidly_v2.rs
  balancer_v2.rs
  balancer_v3.rs
  curve.rs
  erc4626.rs
  uniswap_v4.rs
```

Migration order:

1. Uniswap V2: smallest complete adapter.
2. Uniswap V3: cold-start, tick snapshot, event sync, and cache injection.
3. Pancake V3 and Slipstream: reuse V3 adapter with storage-layout flavors.
4. Solidly V2: V2-like reserves plus stable/volatile metadata.
5. Balancer V2/V3: vault event routing and balance updates.
6. Curve: exchange events plus documented drift/reconciliation behavior.
7. ERC4626: deposit/withdraw event semantics.
8. Uniswap V4: metadata-only adapter until simulation/cold sync semantics are
   implemented.

Acceptance criteria:

- Existing behavior is preserved under protocol feature flags.
- Protocol slot constants and protocol storage-key helpers live in this crate.
- `evm-fork-cache` no longer has a `protocols` feature.

Review checkpoint:

- After V2/V3 migration, validate the module shape before migrating remaining
  protocols.

### Phase A4: Cold Start Sync Planning

Replace ad hoc initialization paths with explicit cold-start sync plans.

Deliverables:

- Per-protocol cold-start plans that declare:
  - required account reads
  - required storage reads
  - required view calls, if storage-only initialization is not practical
  - immutable metadata reads
  - freshness validators
  - mutation plans for cache repair or injection
- V3-specific cold-start planner:
  - read fresh slot0 and liquidity
  - compare against tick snapshot/cache metadata
  - decide complete, inject snapshot, incremental resync, or full resync
  - create batchable bitmap/tick-info read plans
- Support archive-node failure classification for historical storage misses.

Acceptance criteria:

- Cold start can be described before execution, making it batchable and
  inspectable.
- V3 cold start preserves the current snapshot validation and adaptive scan
  behavior.
- The caller can choose eager or lazy completion of expensive sync work.

Review checkpoint:

- Approve `ColdStartPlan` and V3 resync state machine before implementation.

### Phase A5: Event Subscription And Routing API

Make event subscriptions adapter-driven.

Deliverables:

- `EventRegistry` that aggregates topics by enabled adapters.
- `EventRouter` that maps logs to target pools through adapter routing rules.
- Normalized event outputs:
  - swap
  - liquidity change
  - balance/reserve sync
  - cache-only invalidation
  - unsupported/no-op
- Batch application API for block-level log processing.
- Optional cache mirroring after event application.

Acceptance criteria:

- Users can subscribe to all relevant topics for a tracked AMM set.
- Users can apply events to cache without constructing simulation models.
- Balancer-style vault routing is first-class.

Review checkpoint:

- Decide whether `EventRouter` owns pool state, cache state, both, or neither.

### Phase A6: Pool Classification And Instantiation From Address

Implement utilities to identify and load known AMMs from an address in the EVM
cache.

Deliverables:

- `PoolClassifier` API.
- Classification strategies:
  - exact bytecode hash where stable
  - bytecode signature/probe where hashes vary
  - factory metadata where available
  - storage/layout probes as fallback
- `PoolDescriptor` that captures protocol, address, tokens, fees, pool id,
  vault, tick spacing, hooks, and other non-simulation metadata.
- `load_descriptor_from_cache(address)` using enabled adapters.
- Clear handling for ambiguous or unsupported contracts.

Acceptance criteria:

- A known V2, V3, Balancer, Curve, Solidly, or Slipstream pool can be
  identified from cache-accessible data.
- Classification does not require `amm-math`.
- Ambiguous matches are explicit, not silently guessed.

Review checkpoint:

- Decide how aggressive classification should be. Conservative false negatives
  are probably better than false positives.

### Phase A7: Factory Discovery

Refactor factory discovery around adapter-owned factory definitions.

Deliverables:

- `FactoryAdapter` or factory support on `ProtocolAdapter`.
- Pair query planning for V2, V3-style, Slipstream, and future factories.
- Configurable fee tiers/tick spacings.
- Discovery results as `PoolDescriptor`s, not immediately simulation models.
- Optional TOML formatting as a separate convenience layer.

Acceptance criteria:

- Discovery can run with only adapter features.
- Existing factory discovery behavior is preserved or intentionally narrowed.
- TOML output uses canonical `AmmType` names.

Review checkpoint:

- Decide whether Balancer/Curve registry discovery belongs in this crate now or
  later.

### Phase A8: Adapter Test Matrix And Docs

Harden the adapter layer before building on it.

Deliverables:

- Unit tests for event decoding and cache mutation plans.
- Mock-cache tests for cold-start planning.
- Golden tests for V2/V3 slot encoding and decoding.
- Integration tests against mocked `EvmCache` APIs.
- Protocol docs covering assumptions and known approximations.

Acceptance criteria:

- Adapter tests do not require live RPC.
- Protocol-specific caveats are documented in module docs.
- Feature combinations compile independently.

Review checkpoint:

- Adapter deliverable is complete when users can track AMM cache state without
  enabling simulation/search features.

## Deliverable 2: Simulation And Search Logic

Goal: provide optional, high-performance AMM simulation and route search on top
of adapter-produced state.

### Phase S0: Optionalize Simulation Dependencies

Make `amm-math`, `amms`, and `rayon` optional where possible.

Deliverables:

- Move current `LocalAMM` and simulation structs behind `simulation`.
- Move routing/search behind `search`.
- Keep adapter-only APIs compiling without `amm-math`.
- Audit public exports so adapter users do not see simulation-only types unless
  enabled.

Acceptance criteria:

- `cargo check --no-default-features --features adapters` does not compile
  `amm-math` or search dependencies.
- Existing tests pass with full features.

Review checkpoint:

- Confirm feature names and default feature set before changing `Cargo.toml`.

### Phase S1: Normalized Protocol State

Introduce state structs that bridge adapters and simulation models.

Deliverables:

- `PoolDescriptor`: metadata required to identify and route a pool.
- `PoolState`: mutable protocol state needed for cache sync and simulation.
- `SimulationState`: normalized state guaranteed sufficient to instantiate a
  pure Rust simulation model.
- Conversion APIs:
  - cache reads -> `PoolState`
  - event application -> updated `PoolState`
  - `PoolState` -> simulation model

Acceptance criteria:

- Search code no longer needs to know about cache internals.
- Cache adapters do not need to depend on `amm-math` models.

Review checkpoint:

- Decide whether `PoolState` is one enum or protocol-specific structs behind a
  trait.

### Phase S2: Simulation Model Construction

Rebuild `LocalAMM` as an optional simulation-facing abstraction.

Deliverables:

- `LocalAMM` or renamed `SimulatedAMM` under `simulation`.
- Protocol-specific constructors from `PoolState`.
- Explicit unsupported simulation cases:
  - Uniswap V4 until hook-aware simulation is implemented.
  - Curve tricrypto until math exists in `amm-math`.
  - Any protocol state missing required fields.
- Preserve `AutomatedMarketMaker` compatibility where useful.

Acceptance criteria:

- A loaded descriptor/state can instantiate a simulation model when the protocol
  supports it.
- Unsupported cases fail explicitly.
- Current examples can be ported to the new API.

Review checkpoint:

- Decide whether to keep compatibility with the upstream `amms`
  `AutomatedMarketMaker` trait or define this crate's own narrower trait.

### Phase S3: Snapshot And Indexing Layer

Make snapshots first-class for parallel search.

Deliverables:

- Immutable `AmmSnapshot` containing simulation models.
- Token-to-pool index.
- Optional protocol/token filters.
- Incremental snapshot refresh from adapter events.
- Copy-on-write or Arc-based storage if cloning becomes costly.

Acceptance criteria:

- Event-driven users can cheaply produce a search snapshot.
- Snapshot reads are `Send + Sync` and do not hold event-router locks.

Review checkpoint:

- Choose snapshot ownership model based on benchmark data.

### Phase S4: General Route Simulation

Expand route primitives beyond the current triangular-only path.

Deliverables:

- Directed leg and route types.
- Route simulation API over immutable snapshots.
- Configurable route constraints:
  - max hops
  - allowed tokens
  - blocked pools
  - protocol filters
  - repeated-token and repeated-pool policy
- Deterministic error reporting for failed hops.

Acceptance criteria:

- Current triangular route simulation is expressible as a specialization.
- Multi-hop route simulation is deterministic and offline.

Review checkpoint:

- Decide whether route enumeration belongs in this crate or searchers should
  provide candidate routes.

### Phase S5: Search Algorithms

Build search primitives for common AMM opportunities.

Deliverables:

- Preserve and harden triangular arbitrage search.
- Add candidate route enumeration APIs.
- Add sizing strategies:
  - fixed input sweep
  - ternary search for unimodal routes
  - hybrid search around V3 tick boundaries
  - caller-supplied candidate sizes
- Return structured results with route, input, output, profit, and failure
  context.
- Parallel evaluation using `rayon` behind `search`.

Acceptance criteria:

- Existing triangular benchmark remains available.
- Search result ordering is deterministic.
- Sizing assumptions are documented per algorithm.

Review checkpoint:

- Decide how much search sophistication belongs here versus in downstream bots.

### Phase S6: Validation Against EVM Simulation

Provide tools to compare pure Rust simulation against EVM-backed execution.

Deliverables:

- Optional quote verification helpers using `evm-fork-cache`.
- Differential tests for supported protocols.
- Drift reports comparing:
  - adapter event state
  - cache-backed state
  - pure Rust simulation output
  - EVM quote output, where available

Acceptance criteria:

- Users can validate a candidate route before execution.
- False confidence is avoided: unsupported verification paths are explicit.

Review checkpoint:

- Decide whether verification helpers are part of `search` or a separate
  `verification` feature.

### Phase S7: Performance Hardening

Optimize only after the new boundaries are stable.

Deliverables:

- Benchmarks for:
  - event decode/apply
  - cache mutation planning
  - cold-start planning
  - V3 snapshot injection
  - route simulation
  - route search
- Allocation profiling for snapshots and route enumeration.
- Parallelism tuning for search workloads.
- Regression thresholds for hot paths.

Acceptance criteria:

- Current benchmark coverage is preserved and expanded.
- Performance-sensitive APIs are measured before stabilization.

Review checkpoint:

- Decide which benchmarks are release blockers.

### Phase S8: Public API Stabilization

Prepare the crate for open source release.

Deliverables:

- Public module layout cleanup.
- Crate-level docs explaining the adapter/search split.
- Feature flag documentation.
- Examples:
  - adapter-only event-to-cache sync
  - cold start from configured addresses
  - address classification
  - factory discovery
  - simulation snapshot
  - triangular arbitrage search
- Compatibility policy for protocol adapters.

Acceptance criteria:

- A new user can understand which feature set to enable.
- Adapter-only and full-search users both have clear quickstarts.
- Public APIs expose stable concepts rather than current internal migration
  details.

Review checkpoint:

- Final API review before open source release.

## Cross-Cutting Work

### Protocol Correctness

- Keep protocol behavior isolated by adapter.
- Document approximations explicitly, especially Curve event drift and
  V3 tick-scan assumptions.
- Prefer conservative invalidation over silent stale state.
- Add golden tests for storage layouts.

### Error Handling

- Distinguish:
  - unsupported protocol
  - unknown protocol
  - missing cache data
  - stale cache data
  - decode failure
  - execution/read failure
  - simulation unsupported
- Avoid collapsing everything into generic `anyhow` at public boundaries.

### Observability

- Preserve tracing on cold start, event application, cache mutation, and search.
- Expose structured summaries for:
  - slots read
  - slots injected
  - purges
  - resync reason
  - pools skipped
  - search candidates evaluated

### Compatibility

- Keep current examples working during migration where possible.
- Provide deprecation shims if public names need to change.
- Maintain tests for all enabled feature combinations that are expected to
  compile.

## Suggested Milestones

1. **Boundary approved**
   - Adapter/cache interface agreed with `evm-fork-cache`.

2. **Minimal adapter path**
   - Uniswap V2 event -> cache mutation works without `amm-math`.

3. **V3 adapter path**
   - V3 cold start, event sync, tick snapshot injection, and incremental resync
     work through the new abstractions.

4. **Protocol migration complete**
   - Existing protocols moved behind protocol feature flags.

5. **Adapter-only crate works**
   - Users can track AMM cache state without simulation features.

6. **Simulation optionalized**
   - `amm-math` is only required for simulation/search features.

7. **Search rebuilt on snapshots**
   - Current triangular arbitrage flow works on the new simulation snapshot
     layer.

8. **Open source readiness**
   - Docs, examples, feature matrix, tests, and benchmarks are ready.

## Initial Open Questions

- ~~Should adapters mutate `EvmCache` directly, or only emit plans?~~ Resolved:
  adapters emit `StateUpdate`/`ReactiveEffect`s consumed by the reactive
  runtime; they never mutate `EvmCache` directly.
- Should `SlotObservationTracker` remain in `evm-fork-cache`, move here, or
  become a generic cache service with protocol hints from this crate? (Still
  open: the reactive runtime now owns resync/reorg, but the legacy
  `cache_sync` path still uses `SlotObservationTracker` directly.)
- Should `LocalAMM` remain named as-is, or should simulation-facing state be
  renamed to avoid confusing it with adapter-only protocol state?
- Should protocol feature flags gate both adapter and simulation support, or
  should those be separate axes?
- How conservative should bytecode/storage classification be?
- Which discovery sources beyond factories belong in the first open source
  release?
- Should EVM quote verification live in `search`, `simulation`, or a separate
  feature?
