# Changelog

All notable changes to `evm-amm-state` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.0] - 2026-07-14

### Added

- Added the progressive live-runtime domain model: generation-scoped pool,
  adapter, discovery-owner, and work identities; post-block state points;
  checked lifecycle/version/revision/event sequences; registration provenance;
  deterministic committed AMM change sets; and recoverable typed runtime
  status/observer events.
- Added deterministic `AmmSyncBatchReport::{affected_pools, pool_changes,
  incidents, requires_full_refresh}` attribution after direct effects and
  authoritative resync completion, including shared-state ownership,
  degradation/recovery, coverage gaps, missed ranges, and reorgs.
- Added typed `AmmReactiveSignal` hook payloads for decoded AMM events and
  per-pool decode failures, avoiding debug-label parsing in integrations.
- Added generation-scoped `AmmPoolReactiveHandler`s with exact direct,
  shared-emitter, and adapter-defined local routing; pool-scoped hook payloads
  (including actionable repairs) and repair IDs retain the exact
  `PoolInstanceId` so stale generations cannot alias replacements. The public
  compatibility handler retains its previous resync-ID format.
- Added `AmmOwnershipIndex` and adapter-declared `PoolStateDependencies` for
  pool/handler/adapter, associated-address, whole-account, exact-slot, emitter,
  runtime-job, and resync ownership. Sync attribution, degradation/recovery,
  purge handling, and explicit removal eviction now use these indexes instead
  of registry-wide protocol-specific scans; resync ownership is registered from
  real handler reports, consulted before shared-target fallback, and removed on
  synchronous completion.
- Added offline `pool_routing` coverage alongside the lifecycle benchmark and
  documented the transactional lifecycle baselines.
- Added transactional `AmmSyncEngine::{add_pools, remove_pools}` lifecycle APIs
  with retained pool/adapter generations, checked successor allocation,
  lifecycle tombstones, atomic batch rejection/rollback, typed
  `AmmLifecycleReport`s, and incremental compatibility wrappers that preserve
  runtime journals and pending work.
- Added adapter-family add/remove and explicit cascade lifecycle. Default
  removal rejects in-use adapters; cascade uses prevalidated no-scan detach
  primitives and applies exclusive cache eviction only after the complete
  adapter commit.
- Added explicit `Retain`/`Exclusive` eviction reporting and exact
  generation-owned resync cancellation. Shared-address pending requests and
  cache slots belonging to remaining pools are preserved. Multi-pool teardown
  batches every exact ID into one pending-queue pass, and explicit eviction is
  fenced until the removed handler generation leaves the reorg journal so a
  later rollback cannot rematerialize purged state.
- Upgraded the lifecycle path to `evm-fork-cache 0.3.0`: direct/indexed pool
  handlers declare exhaustive route indexes, indexed hit/miss dispatch skips
  unrelated handlers, and ordered owner sets make handler churn logarithmic
  without tombstone growth. Adapter-defined third-party routes retain the
  compatible fallback path.
- Added the opt-in `live-runtime` cache actor and Alloy subscriber driver:
  sealed full-header baselines, immutable version/interest-revision snapshots,
  complete zero-log block commits, typed reorg/degradation incidents,
  transactional generation-owned interest add/remove, bounded reliable and
  observer channels, recoverable snapshot/status watches, explicit
  backpressure, bidirectional queue fairness, and prompt coordinated shutdown.
- Added checked prepared-pool publication, non-mutating exact subscription
  previews, hash-pinned/chunked complete-block log reconciliation, and the
  `live_runtime_actor` creation benchmark.
- Added progressive background cold-start: atomic exact-generation
  reservations, bounded job admission and provider concurrency, priority/fair
  resumable planner quanta, recoverable round progress and queue depths,
  independent per-pool publication, cancellation-safe tombstones, and stale
  work/baseline rejection. Worker artifacts now pair exact-hash storage with
  root-only account/code proofs and commit verified runtime code without an
  intermediate unverified cache state.
- Added Stage 6 live orchestration: bounded connector and factory-event
  discovery, background canonical repair and lazy deferred warming, exact
  adapter/factory watcher generations, dynamic add/remove and cascade teardown,
  capacity-one successor handoffs, stale-point retry for runtime-owned work,
  and prompt cancellation that prevents old adapter work from escaping into a
  replacement generation. Registration evidence is merged across independent
  discovery owners, query revalidation is durable and owner-keyed, and
  subscriber acknowledgement now fences interest-changing topology mutation.
- Added incremental immutable AMM commits for search consumers, including exact
  pool generations/revisions, state quality, complete state points, reliable
  change subscriptions, and snapshot-safe worker overlays.
- Added registration warm resume through the deterministic, bounded,
  chain-scoped `AmmRegistrationArchive`. Built-in metadata and reusable read-set
  hints restore as pending work and must pass current-baseline verification.
- Added attached-subscriber reorg hardening: subscriber lifecycle controls are
  serviced while canonical ingest is in flight, preventing factory-evidence
  removal from deadlocking the same driver that delivered the reorg. Overtaking
  multi-block replacements now walk exact parent hashes to the retained common
  ancestor and publish every replacement block oldest-first. Dropped creation
  hashes also prune pending and in-flight factory evidence, cancelling
  factory-only hydration while retaining independently supported work.
- Added indexed factory-watcher dispatch by emitter and creation topic, with
  atomic add/remove/cascade/rollback maintenance and deterministic wildcard
  handling. Unrelated logs no longer invoke unrelated factory decoders.
- Added production cache-owner, lifecycle, routing, progressive cold-start, and
  release-gate benchmark coverage with reproducible offline/live methodology.
- Added configurable point-read batching and bulk storage strategy propagation
  across cold start, discovery, repair, and deferred worker fetch paths.
- Added fail-closed mainnet and Base live release runners covering swap parity,
  discovery, liquidity event sourcing, WebSocket health, progressive first
  routes, and chain-specific adapter gates. Curve activity can be made strict
  with `E2E_REQUIRE_CURVE_ACTIVITY=1`; the default check reports inactive pools
  and continues to deterministic post-soak quote parity.

### Changed

- Bumped the release candidate to `0.2.0` and the companion dependency to
  `evm-fork-cache 0.3.0`; publish the companion minor release before packaging this
  crate.
- Canonical state quality now uses an incrementally maintained degraded-pool
  count, and zero-change canonical commits reuse the immutable pool-revision
  index instead of scanning or cloning it.
- `AmmRuntimeHandle::shutdown` now resolves only after actor-owned resources,
  including the persistent `EvmCache`, have been dropped and their final flush
  has completed.
- Prepared cold-start artifacts can now carry exact-block proof-verified runtime
  code for quote dependencies whose bytecode has no embedded template. The V3
  family uses this for PancakeSwap and Slipstream pool runtimes.
- Pancake V3 full sync now hydrates its verified shifted storage layout: the
  second `slot0` word containing `feeProtocol`/`unlocked`, fee-growth and
  protocol-fee slots, and the observation ring.
- Connector discovery accepts an optional deterministic per-request candidate
  limit, allowing consumers to enforce a global startup pool budget.

### Fixed

- Immutable-snapshot V3 quotes no longer require arbitrary ERC-20 balance state:
  a call-scoped transfer-success runtime neutralizes only the output transfer
  before QuoterV2's intentional revert and is restored immediately afterward.
- Background prepared-state validation now accepts self-hash-consistent,
  exact-proof code targets declared by an adapter while continuing to require
  predeclared hashes for embedded code seeds.

## [0.1.0] - 2026-07-07

First public release.

### Added

**One-shot V3 full-pool sync (`adapters::v3_sync`)** — generated EVM storage
programs (a ~360-byte assembler lives in the module) injected over a pool's
code via `evm-fork-cache`'s `eth_call` state-override transport:

- **Full sync**: zero calldata; walks the entire tick bitmap *in-EVM* (word
  range and tick spacing baked into the bytecode per `V3StorageLayout`, so
  Uniswap/Pancake/Slipstream slot bases all work) and returns statics, every
  initialized tick's four `Tick.Info` words, and the whole observation ring
  in one call. `V3PoolSnapshot` materializes back into `(slot, value)` pairs
  — including explicit zeros for empty bitmap words — for
  `EvmCache::inject_storage_batch`.
- **Partial sync**: calldata-driven word list for bounded windows (the
  cold-start planner's rounds 2+3 collapsed into one call) and for chunking
  dense spacing-1 pools.
- Live-verified on USDC/WETH 0.05%: 1,563 ticks + 723 observations → 7,674
  slots in ~140 ms / 26 CU (vs ~130k CU as per-slot point reads); tick-crossing quote
  parity against both a classically cold-started cache and the provider's
  QuoterV2 `eth_call` (`tests/v3_full_sync_rpc.rs`); offline revm execution
  suite for the generated bytecode (`tests/v3_sync.rs`); runnable demo
  (`examples/v3_full_sync.rs`).

**One-shot flat-slot sync (`adapters::storage_sync`)** — the complementary
loaders for pools whose hot state is already a known slot list: baked-slot
bytecode for small fixed layouts (Uniswap V2, Solidly V2) and a reusable
calldata-driven loader for discovered read-sets (Balancer V2 vault balances,
Curve `get_dy` read-sets), all over the same `eth_call` code-override
transport. `storage_sync_spec_for_pool` derives the spec from a registration.
Warms the cache only — pool status/metadata still come from cold-start.

**Eager quote-target code warming** — an eager
`AdapterRegistry::cold_start_many` now pre-warms the canonical quote
entrypoints' bytecode (Uniswap V3 `QuoterV2`, Uniswap V2 `Router02`, Balancer
vault) once for the whole batch, so the first `simulate_swap` of each family
runs offline instead of paying a lazy `eth_getCode` on the hot path. Targets
are resolved by the new `ProtocolMetadata::quote_code_targets` /
`PoolRegistration::quote_code_targets` (shared with `simulate_swap` via
`V3Metadata::quote_target`, so the warmed address is exactly the one quoted
against) and configured through the new `AdapterRegistry::with_sim_config`.
Solidly V2 and Curve self-quote against the pool itself, so they need no
separate quote target; their `PoolFactory` / Curve-NG math-impl dependencies
remain a one-time lazy first-quote fetch.

**Trace-backed live sync driver (`adapters::sync_manager::AmmSyncEngine`)** —
the intended live path. Wraps the upstream runtime's
`ingest_batch_with_resync` so the resync repairs emitted by Balancer, Curve,
and V3 liquidity events are **executed** (block-trace first, storage-fetch
fallback), not just reported. Tracks pool lifecycle from resync outcomes both
ways: pools whose targets fail are marked `Degraded`; degraded pools whose
covered slots are authoritatively refreshed (with no failures that batch)
recover to `Ready` (`AmmSyncBatchReport::{degraded_pools, recovered_pools}`).
`replace_registry` rebuilds the handler registration when pools or read-set
metadata change. Boundary and fallback policy are documented in
[`docs/trace-backed-sync.md`](docs/trace-backed-sync.md).

**Upgraded to `evm-fork-cache` 0.2.1** — batch storage fetches (cold-start
verify/probe, repair, prefetch) ride its bulk `eth_call` extraction by default
(thousands of slots per 26-CU call with automatic point-read fallback); the
crate-owned error mirrors map its typed errors (`CacheError`,
`StorageFetchError`) instead of `anyhow`; and 0.2.1's trace-based storage-slot
discovery was used to verify the factory-discovery preset slots against chain.

**Adapters pipeline** — a single [`AmmAdapter`] trait with five stages:
register → cold-start → subscribe → react → simulate.

- `AdapterRegistry` dispatches by `PoolKey`; `AmmReactiveHandler` bridges the
  adapters into the `evm_fork_cache` `ReactiveRuntime`; `AdapterDriver` applies
  decoded logs to an `AdapterCache` in caller order.
- `run_deferred` driver executes the `DeferredWork` produced by `Lazy`
  cold-start (warming the deferred read-set on demand).

**Protocols** (each behind its own feature flag, all on by `default`):

- **Uniswap V2** — `getAmountsOut` quotes; named-slot cold-start; `Sync` →
  exact masked reserve write (event-sourced, no refetch).
- **Uniswap V3 family** (V3, PancakeSwap V3, Slipstream) — `QuoterV2`
  quotes; slot0 + liquidity + a bounded, fixed-radius **multi-word tick-window
  warm-up** at cold-start; `Swap` → slot0/liquidity. `Mint`/`Burn` are
  **event-sourced**: the exact `liquidityGross`/`liquidityNet` (packed word 0),
  `tickBitmap` bit, and in-range global `liquidity` are written directly from the
  event for warm (in-window) ticks with no RPC, and only genuinely-cold ticks
  fall back to a targeted resync.
- **Balancer V2** — `Vault.queryBatchSwap` quotes; discover→verify cold-start
  (`getPoolTokens` read-set), with a **verify-only fast path** when the
  read-set is already known; `Swap` → **event-sourced** exact 112-bit
  `cash`-field writes where the probed cash locations are warm (TWO_TOKEN and
  GENERAL specializations), balance-slot resync as fallback;
  `PoolBalanceChanged` → balance-slot resync.
- **Solidly V2** (Aerodrome / Velodrome) — pool `getAmountOut` quotes;
  config-supplied storage layout; `Sync` → two exact slot writes.
- **Curve** — StableSwap, StableSwap-NG, CryptoSwap v2, and Tricrypto-NG
  dialects through one adapter; pool `get_dy` quotes; discover→verify
  cold-start, with a **verify-only fast path** when `discovered_slots` is
  pre-populated and an optional caller-supplied bytecode seed
  (`CurveMetadata::with_code_seed`); `TokenExchange` + liquidity events →
  discovered-slot resync. Event signatures and `get_dy` ABIs verified on-chain
  per dialect.

**Simulation** — `simulate_swap` runs each pool's **own** canonical on-chain
quote entrypoint inside a local revm against the warmed cache, then decodes the
result. There is **no reimplemented AMM math**.

**Reactive synchronization** — fully offline (no RPC in the hot path) for the
common case. Pools whose events carry absolute state are event-sourced with exact
writes (Uniswap V2 / Solidly `Sync`); Uniswap V3 `Mint`/`Burn` are event-sourced
too, applying the exact liquidity delta to the warmed tick/bitmap/liquidity slots
and resyncing only ticks outside the warmed window; Balancer V2 `Swap`s
event-source the vault's packed 112-bit `cash` fields directly when the probed
cash locations are warm, falling back to a slot resync on gaps; Curve events
(and Balancer joins/exits) carry deltas over a non-predictable layout and
re-verify the discovered slots (`VerifySlots`).

**Testing & CI**

- Unit, offline reactive, cold-start adoption, and full register→cold-start→
  react→simulate pipeline tests.
- Env-gated, `#[ignore]`d network tests: RPC parity (fork at a pinned block,
  cold-start a real pool, assert `simulate_swap` == on-chain `eth_call` quote —
  mainnet pools plus a Base pool for Solidly), a live WebSocket soak that
  keeps state in sync from events only, and per-transaction **write parity**
  for the event-sourced paths: for real add/remove-liquidity and vault-swap
  transactions, the adapter's writes are asserted equal to the on-chain
  `trace_replayTransaction` storage diff (`tests/v3_liquidity_rpc.rs`,
  `tests/balancer_liquidity_rpc.rs`).
- CI runs fmt, clippy (all-features + a **per-protocol isolation matrix** +
  no-default-features), tests (all-features / default / no-default), doc
  (`-D warnings`), and a heavy-dependency leak guard.

**Mid-lifecycle pool management** — the tracked working set is no longer fixed
at startup: `AdapterRegistry::{unregister_pool, unregister_adapter}` (the
latter refuses while any registered pool still dispatches to the adapter), and
on the live engine `AmmSyncEngine::register_pools` /
`unregister_pools` / `unregister_pools_evicting` apply batched registry
changes with one runtime rebuild per call. The evicting variant purges cache
state owned exclusively by the removed pools — a shared address (e.g. the
Balancer vault) only loses read-set slots no remaining pool covers.

**Verified pool bytecode seeding (`adapters::bytecode`)** — adapters seed a
pool's canonical runtime bytecode into `EvmCache` at cold-start (via
`evm-fork-cache` 0.2.0's `seed_account_code`), so it is verified once against
the on-chain `EXTCODEHASH` instead of paying an `eth_getCode`. Uniswap V2 shares
one embedded pair runtime across every pair; Uniswap V3 patches the pool's
Solidity immutables (factory, token0/1, fee, tickSpacing, maxLiquidityPerTick,
and the `NoDelegateCall` self-address) into an embedded template. Curve has no
shared template, but a caller that already knows a pool's Vyper runtime can
attach it with `CurveMetadata::with_code_seed` — verified once against on-chain
code under the same purge-on-mismatch contract. Seeding is a
pure optimization: a hash mismatch, an unverifiable seed, a warm-cache code
conflict, or a template render error all degrade to lazily fetching the real
code — never a fatal error or a permanently `Degraded` pool — and every seeded
address ends `Verified` or unmarked, never left `Pending`. Verification results
surface on `ColdStartReport.code_seeds`; opt out with
`AdapterRegistry::with_code_seeding(false)`. Embedded artifacts and the V3 patch
offsets are pinned to chain-truth code hashes across tickSpacings 1/10/60
(`tests/bytecode_golden.rs`) and live-proven in
`examples/verified_bytecode_seed.rs`.

**Factory-backed pool discovery (`adapters::factory`)** — build
cold-start-ready `PoolRegistration`s from configured factories instead of pasted
addresses, across every protocol whose pools resolve through the pinned cache
via a **DerivedSlot** read — a Rust-computed factory storage slot, resolved in
the batched read. Coverage:

- **Concentrated liquidity** — one generalized `ClFactorySpec` drives the whole
  UniV3-mechanics family through a single `ConcentratedLiquidityFactory`: fee-keyed
  forks (Uniswap V3, SushiSwap V3, PancakeSwap V3 — `getPool[t0][t1][fee]` with
  tick spacing from `feeAmountTickSpacing[fee]`) and tickSpacing-keyed forks
  (Slipstream / Aerodrome CL — `getPool[t0][t1][tickSpacing]`, no fee mapping).
  `ClKeying` selects the shape, `FeeSource` where the fee comes from, and a
  per-pool `quoter` flows into metadata; fork presets (`uniswap_v3`, `sushi_v3`,
  `pancake_v3`, `slipstream`) are pre-filled specs. An optional CREATE2
  cross-check hard-fails `DerivationMismatch` on a wrong init-code hash / base
  slot.
- **Uniswap V2** — `getPair[t0][t1]`.
- **Solidly V2** (Aerodrome / Velodrome) — `SolidlyFactory` reads
  `getPool[t0][t1][bool stable]`, yielding a pair's stable **and** volatile pools
  and carrying the fork's storage layout for cold-start.
The CL and Solidly preset storage constants (Pancake `getPool` slot 2 +
`feeAmountTickSpacing` slot 1, Slipstream `getPool` slot 6, Aerodrome `getPool`
slot 5 + the pool reserve/token layout) are **verified on-chain** by the gated
`discovery_cl_rpc` / `discovery_solidly_rpc` parity tests — discovered with a
trace-based storage-slot probe rather than guessed.

Creation logs decode too (`PairCreated`, the Uniswap/Pancake and Slipstream
`PoolCreated`, the Solidly `PoolCreated`). A few AMM shapes are deliberately out
of scope for 0.1.0 and documented as integrator-supplied: **Algebra**-style CL
forks (a different pool engine, added with `register_adapter` + `with_factory`),
**Curve** (needs the Vyper MetaRegistry view call, not wired this release — its
adapter still simulates explicitly-registered pools), and **Balancer V2**
discovery (no on-chain token→pool index; an async log scan, planned later) — see
`docs/pool-discovery.md`. `FactoryConfig` is
empty by default (callers opt into explicit factory addresses). Multiple
factories of one protocol coexist — keyed by `(protocol, factory_address)`, so
Uniswap and a Sushi-style fork both resolve — and an external `PoolFactory` can
be added through the `with_factory` open channel. The whole surface is one
method, `PoolDiscovery::find(cache, query)`,
over one fluent `PoolQuery`: `PoolQuery::pair(a, b)` (a single pair),
`PoolQuery::basket([..])` (every `C(n, 2)` pair of a token basket),
`PoolQuery::pairs([..])` (an explicit pair set), each optionally scoped with
`.on(protocol)`. Any query resolves in a **single batched read** (one bulk
`eth_call` via `AdapterCache::read_storage_slots`) across all matching factories
— all pairs, all factories, all V3 fee tiers at once — with a per-pair
`find_pools` fallback for external factories that only implement `find_pools`
(no `candidate_reads`). Request count scales with factory count, not pairs, fee
tiers, or basket size (a 5-token mainnet basket: 49 pools in 1 round-trip vs 20,
~20× faster; `examples/token_basket_bench.rs`). An unscoped query spans every
matching factory; `.on(p)` filters and errors `MissingFactory(p)` if `p` has no
factory. `PoolDiscovery::find_many(cache, [query, ..])` resolves several queries
together — different protocols for different pairs — in that same single batched
read, returning the de-duplicated union. Discovery is read-only: it produces
registrations, and callers still decide when to cold-start them.

**Bundled multi-pool bootstrap (`AdapterRegistry::cold_start_many`)** — the fast
default for warming many pools at once: it seeds + verifies every
one-shot-eligible pool's code in one account-fields call, hydrates them all
through a single bundled `run_storage_programs` `eth_call` (V3 full-sync / V2
flat-slot / Balancer and Curve discovered read-sets — a discover→verify pool
qualifies once its `discovered_slots` are known), and finalizes `Ready` — falling back per pool to the normal
`cold_start` for anything without a one-shot program or whose hydration fails.
`supports_one_shot_hydration` reports eligibility. `examples/factory_discovery_live.rs`
uses the `find(PoolQuery) → cold_start_many → register` path.

### Changed

- The one-shot sync and sync-engine types (`V3SyncSpec`, `V3ObservationsSpec`,
  `V3TickSnapshot`, `V3PoolSnapshot`, `StorageSyncSpec`, `StorageSyncSnapshot`,
  `StorageSyncEncoding`, `AmmSyncError`, `AmmSyncBatchReport`) are
  `#[non_exhaustive]`, matching the rest of the public surface; construct via
  the new `V3SyncSpec::new`/`with_word_range`, `V3ObservationsSpec::new`,
  `V3TickSnapshot::new`, `V3PoolSnapshot::new`, and `StorageSyncSnapshot::new`
  (alongside the existing `uniswap`/`core`/`StorageSyncSpec::new`).
  `UpdateQuality`, `ColdStartPolicy`, and `EventRoute` stay deliberately
  exhaustive (closed control vocabularies) — now documented as such.
- Slimmed `[dependencies]` to what `src/` actually uses: dropped the unused
  `alloy-contract`, `alloy-transport-balancer`, `foundry-fork-db`, `serde`,
  and `tracing`; `futures`, `tokio`, and `anyhow` are dev-only (consumers no
  longer pull a tokio runtime, and `anyhow` is absent from the crate's entire
  normal dependency graph).
- Removed `impl From<anyhow::Error> for CacheError` — dead since the
  `evm-fork-cache` 0.2.0 upgrade moved every consumed API to typed errors;
  the crate's error surface is fully typed (`CacheError`, `DriverError`,
  `SimError`, `RegistryError`, `AdapterEventError`, `ColdStartError`,
  `StorageSyncError`, `V3SyncError`, `AmmSyncError`).
- docs.rs builds with `all-features` so the `uniswap-v3`-gated `v3_sync`
  module and `experimental-protocols` identities render.
- Error facades no longer flatten their causes: `CacheError::Backend`,
  `ColdStartError::Fetch`, `V3SyncError::Program`, and
  `StorageSyncError::Program` carry the boxed source error instead of a
  string — walk [`source`](https://doc.rust-lang.org/std/error/trait.Error.html#method.source)
  or downcast (e.g. to `evm_fork_cache::{CacheError, StorageFetchError}`) for
  typed handling. `ColdStartError` gained the `NoAccountProofFetcher` variant
  it previously folded into a stringly `Fetch`; `AdapterEventError` now
  implements `Display`/`Error` and is chained as `DriverError::Decode`'s
  source; `AmmSyncError` exposes its upstream cause through `source()`.
  `StorageSyncError` dropped its `Clone`/`PartialEq`/`Eq` derives (boxed
  payloads are not comparable).

### Fixed

- A `V3Metadata` with `tick_spacing: Some(0)` (or a zero/negative-spacing
  explicit layout) is now rejected as an unresolvable layout — cold-start
  reports `Unsupported` and the reactive repair path takes its conservative
  fallback — instead of panicking with a division by zero in the bitmap-word
  math. `v3_word_position` documents its positive-spacing precondition and
  asserts it with a clear message.

### Removed

- The declared-only `tuning` module (`SyncSpeedMode`, `ProtocolAddresses`) —
  nothing consumed it and its documented features don't exist yet; removing it
  pre-release avoids freezing dead API.

### Design notes

- **No reimplemented AMM math.** Every quote is the pool's own on-chain
  entrypoint executed in revm, so quotes cannot drift from the live contracts.
- **Adapters-only crate.** The legacy `LocalAMM` / `amm-math` /
  `configured_amms` / arbitrage-routing simulation+search path was removed
  during development (it was never part of a release). A routing/arbitrage layer
  is rebuildable on top of `simulate_swap`.

[`evm-fork-cache`]: https://github.com/KaiCode2/evm-fork-cache
[`AmmAdapter`]: src/adapters/traits.rs
[Unreleased]: https://github.com/KaiCode2/evm-amm-state/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/KaiCode2/evm-amm-state/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/KaiCode2/evm-amm-state/releases/tag/v0.1.0
