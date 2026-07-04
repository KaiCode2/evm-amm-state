# Factory discovery — design spec (0.2.0)

Status: **draft for review, rev 2** · Owner: pre-0.2.0 workstream · Supersedes
the sketch in ROADMAP.md's 0.2.0 section (which covered only the event half).
Rev 2 replaces bytecode-execution queries with **derive-first resolution**
(exact storage slots / CREATE2) for the hot protocols, per review feedback;
view calls remain the fallback tier.

## 1. Goal

Make pool discovery **declarative**. A consumer should never need to paste a
pool address for a protocol the crate supports; they say *what* they want and
get cold-start-ready registrations back:

```rust
// "Give me the pool for WETH/USDC at the 0.3% fee tier."
let pools = discovery.find(&mut cache, ProtocolId::UniswapV3,
    PoolQuery::pair(WETH, USDC))?;

// "Give me the WETH/USDC Uniswap V3 pool at the 0.3% fee tier."
let pools = discovery.find_uniswap_v3(&mut cache,
    UniswapV3PoolQuery::pair(WETH, USDC).with_fee_tier(3_000))?;

// "Give me all Uniswap V3 pools between WBTC/WETH."
let pools = discovery.find(&mut cache, ProtocolId::UniswapV3,
    PoolQuery::pair(WBTC, WETH))?;

// "Give me every pool between WBTC/WETH on every protocol I track."
let pools = discovery.find_all(&mut cache, PoolQuery::pair(WBTC, WETH))?;
```

Two complementary halves, one vocabulary:

1. **Pull (query)** — ask the factory/registry for pools that *already exist*.
   This is the primary UX and is **fully unblocked today**: point queries are
   pull-based and need nothing from the upstream interests-refresh work.
2. **Push (creation events)** — subscribe to factory `PoolCreated`/
   `PairCreated`/`PoolRegistered` logs so pools created *after* startup can be
   admitted live.

Both produce the same output type, feed the same admission pipeline
(cold-start → `register_pools`), and stitch together without a gap when the
query runs at the pinned block and the event stream starts there.

## 2. Design principles

- **Derive first, execute as fallback.** Pool resolution is a hot path, so
  the well-known factories are resolved *without running any bytecode*:
  either a **derived storage-slot read** of the factory's own
  `getPool`/`getPair` mapping, or a pure **CREATE2 address computation**
  (§4). Executing the factory's view function in revm is the fallback tier
  for protocols whose resolution is genuinely hard to recreate (Curve's
  Vyper registry indirection) or too fork-variable to trust from config.
- **This does not weaken the no-reimplemented-math rule.** What we refuse to
  reimplement is *behavior that can differ per pool or evolve per fork*
  (swap math, fee logic). A deployed factory's CREATE2 salt scheme, init-code
  hash, and storage layout are **immutable by construction** — frozen at
  deployment, consensus-critical, and independently cross-checkable. The
  drift risk the original rule guards against does not exist here; wrong
  *configuration* does, and §4's guardrails handle that.
- **Block-consistent.** All reads (derived slots included) go through the
  pinned `EvmCache`, so queries answer *as of the pinned block*. A pool
  created later is invisible to the query and arrives via the event half
  instead — no torn view.
- **Crate-owned vocabulary, additive surface.** All new types are
  `#[non_exhaustive]` with constructors/builders; the one `AmmAdapter` hook is
  defaulted. Existing consumers and third-party adapters compile unchanged —
  this ships in 0.2.0 as a semver-minor addition.
- **Config over heuristics.** Factory addresses, init-code hashes, and
  mapping base slots are per-chain config (`FactoryConfig`, mirroring
  `SimConfig` but with empty defaults). Callers opt into explicit factories
  or named presets; no protocol assumes a chain-global factory address. Curve
  variant detection becomes **provenance, not heuristic**: the registry/factory
  that answers for a pool determines its `CurveVariant`.

## 3. Vocabulary (new types, all `#[non_exhaustive]`)

```rust
/// What you want from a factory, protocol-agnostically.
pub struct PoolQuery {
    /// The (unordered) token pair. `PoolQuery::pair` normalizes order, so
    /// callers never care about token0/token1 sorting.
    pub tokens: (Address, Address),
}

impl PoolQuery {
    pub fn pair(a: Address, b: Address) -> Self;
}

/// Uniswap V3-specific query. Future protocol selectors should follow this
/// pattern instead of adding protocol-specific knobs to `PoolQuery`.
pub struct UniswapV3PoolQuery {
    pub tokens: (Address, Address),
    pub variant: UniswapV3PoolVariant,
}

pub enum UniswapV3PoolVariant {
    /// Every configured fee tier for the pair.
    Any,
    FeeTier(u32),
}

/// A pool located by query or creation event, ready for admission.
pub struct DiscoveredPool {
    pub key: PoolKey,
    /// Cold-start-ready registration: metadata pre-filled from the factory
    /// answer (tokens, fee/tick-spacing → derivable layout, vault + poolId,
    /// Curve coins + variant) and adapter event sources attached.
    pub registration: PoolRegistration,
    pub source: DiscoverySource, // Query { block } | CreationEvent { block, log_index }
}

pub enum DiscoveryError {
    /// No factory configured for this protocol (FactoryConfig hole).
    MissingFactory(ProtocolId),
    /// The query shape doesn't fit the protocol (e.g. Stable(_) on V3).
    UnsupportedQuery(&'static str),
    /// Factory read/call failed — boxed un-flattened cause (CacheError etc.).
    Factory(Box<dyn std::error::Error + Send + Sync + 'static>),
    /// The factory answered but the result didn't decode.
    Malformed(&'static str),
    /// Cross-check failure: derived-slot answer and CREATE2 derivation
    /// disagree — almost always a wrong init-code-hash / base-slot config.
    DerivationMismatch { mapping: Address, derived: Address },
}
```

## 4. Resolution strategies: derive first, execute as fallback

Every factory driver resolves a `(pair, variant)` through one of three
strategies, selectable per driver (canonical default per protocol, consumer
override for exotic forks):

```rust
pub enum Resolution {
    /// Compute the factory's `getPool`/`getPair` mapping slot in Rust and
    /// point-read it. One 32-byte storage read; address-or-zero answers
    /// existence and identity in a single shot. DEFAULT for V2/V3 family.
    DerivedSlot,
    /// Compute the pool address with CREATE2 (salt + init-code hash). Zero
    /// I/O — but CREATE2 cannot confirm the pool exists (the address is
    /// deterministic whether or not it was ever deployed), so this is for
    /// callers that already know existence, plus the cross-check guardrail.
    Create2,
    /// Execute the factory's own view function in revm via
    /// `AdapterCache::call_raw`. Fallback tier: Curve registry queries,
    /// unknown forks, and any driver whose direct config is absent.
    ViewCall,
}
```

**Why `DerivedSlot` (not `Create2`) is the hot-path default:** the mapping
read costs one batchable point read and is *existence-aware* — `address(0)`
means "no pool", with no ambiguity. A CREATE2 answer still needs an existence
probe to be useful for discovery, and a zero storage read at a derived pool
address cannot distinguish "not deployed" from "deployed, slot empty" (a V3
pool that is created but not yet initialized has a zero `slot0`). CREATE2's
jobs are (a) zero-I/O derivation when existence is already known, and (b) the
mismatch guardrail below.

**The pure derivation module** (`adapters::factory::derive`, public — the
same keccak mapping machinery `storage.rs` already uses for tick keys):

```rust
// Mapping-slot derivation (Solidity mapping key rules):
pub fn v2_get_pair_slot(base_slot: U256, token0: Address, token1: Address) -> U256;
pub fn v3_get_pool_slot(base_slot: U256, token0: Address, token1: Address, fee: u32) -> U256;

// CREATE2 derivation:
pub fn v2_pair_address(factory: Address, init_code_hash: B256,
                       token0: Address, token1: Address) -> Address;
pub fn v3_pool_address(deployer: Address, init_code_hash: B256,
                       token0: Address, token1: Address, fee: u32) -> Address;
```

Canonical constants do not install factory addresses through `Default`. They
are **verified, not trusted** when a caller opts into a known factory config or
named preset: the gated parity tests assert, at a pinned block, that
`DerivedSlot`, `Create2`, and `ViewCall` all agree with each other and with
known pool addresses for each shipped preset. Getting a fork's config wrong is
the real risk of derive-first, so:

- `FactoryConfig::verify_derivations(bool)` (default **on**): when a driver
  has both a mapping answer and a CREATE2 config, the first successful
  resolution per factory cross-checks them and returns
  `DiscoveryError::DerivationMismatch` on disagreement — wrong init-code
  hashes fail loudly on first use, then the check gets out of the hot path.
- Drivers with incomplete direct config (no base slot / no init hash for a
  fork) degrade to `ViewCall` automatically rather than guessing.

**Performance shape.** `ViewCall` pays a one-time factory *code* fetch
(~20 KB+ for V3) plus a revm execution per query. `DerivedSlot` pays one
32-byte point read — and because slot addresses are computed locally, they
**batch**: "all fee tiers for a pair" is one batched fetch instead of four
sequential executions, and a bulk watchlist ("resolve these 500 pairs across
4 tiers") compiles into a single `storage_sync` calldata-loader program — one
`eth_call` round trip for the entire resolution, reusing the loader shipped
in 0.1.0. `Create2` is two keccaks, no I/O at all.

## 5. The `PoolFactory` trait and the adapter hook

```rust
/// Per-protocol factory driver: point queries + creation-event decoding.
pub trait PoolFactory: Send + Sync {
    fn protocol(&self) -> ProtocolId;

    /// Resolve existing pools for a query against the (pinned) cache, using
    /// the driver's configured `Resolution` strategy.
    fn find_pools(
        &self,
        cache: &mut dyn AdapterCache,
        query: &FactoryQuery,
    ) -> Result<Vec<DiscoveredPool>, DiscoveryError>;

    /// Factory emitters + creation topics for the live (push) half.
    fn creation_sources(&self) -> Vec<EventSource>;

    /// Decode one factory log into a discovered pool (None = not a
    /// creation log this factory owns).
    fn decode_creation(&self, log: &Log) -> Result<Option<DiscoveredPool>, DiscoveryError>;
}
```

The resolution strategy is a *driver* concern — `find_pools`'s signature is
strategy-agnostic, so swapping `DerivedSlot` for `ViewCall` on an exotic fork
changes configuration, not call sites.

`AmmAdapter` gains **one defaulted method** (replacing the two-method ROADMAP
sketch — the factory object subsumes both):

```rust
fn pool_factory(&self, config: &FactoryConfig) -> Option<Box<dyn PoolFactory>> {
    None // opt-in per adapter; third-party adapters unchanged
}
```

The factory is a *separate object* rather than more adapter methods because it
carries chain config (addresses, init-code hashes, base slots, tier lists,
fork ABIs) that the chain-neutral adapter deliberately doesn't. Third parties
can also implement `PoolFactory` directly for custom AMMs without touching
their adapter.

`FactoryConfig` (mirrors `SimConfig` shape, but with no assumed addresses):
`v2_factory`, `v3_factory`, `pancake_v3_factory`, `balancer_vault`,
`curve_meta_registry`, and fork-specific factories are all opt-in. Convenience
constructors can install canonical mapping slots and tier lists once the caller
has supplied the factory address; named presets may be added for known chains,
but `Default` remains empty. Derivation constants such as
`v2_get_pair_base_slot`, `v3_get_pool_base_slot`, `pancake_v3_deployer` + hash
+ slot are verified by gated tests rather than trusted. `slipstream_factory`
and `solidly_factory` are `Option<Address>` plus a `solidly_abi` fork selector
(`getPool(address,address,bool)` vs legacy `getPair`), optional Slipstream
implementation address + clone init hash for its CREATE2 path, and the existing
per-fork `SolidlyStorageLayout` hook.

## 6. The `PoolDiscovery` front-end

```rust
pub struct PoolDiscovery { /* HashMap<ProtocolId, Box<dyn PoolFactory>> */ }

impl PoolDiscovery {
    /// Assemble from a registry's adapters + factory config: every registered
    /// adapter that opts in contributes its factory.
    pub fn for_registry(registry: &AdapterRegistry, config: FactoryConfig) -> Self;

    /// One protocol, one query.
    pub fn find(&self, cache: &mut dyn AdapterCache, protocol: ProtocolId, query: PoolQuery)
        -> Result<Vec<DiscoveredPool>, DiscoveryError>;

    /// Uniswap V3-specific query with a typed fee-tier selector.
    pub fn find_uniswap_v3(&self, cache: &mut dyn AdapterCache, query: UniswapV3PoolQuery)
        -> Result<Vec<DiscoveredPool>, DiscoveryError>;

    /// Same query fanned across every configured protocol.
    pub fn find_all(&self, cache: &mut dyn AdapterCache, query: PoolQuery)
        -> Result<Vec<DiscoveredPool>, DiscoveryError>;

    /// Union of factory creation sources (feed to the subscription).
    pub fn creation_sources(&self) -> Vec<EventSource>;

    /// Route + decode a creation log to whichever factory owns it.
    pub fn decode_creation(&self, log: &Log) -> Result<Option<DiscoveredPool>, DiscoveryError>;
}
```

End-to-end, the declarative flow the feature exists for:

```rust
let config = FactoryConfig::default().with_uniswap_v3_factory(my_v3_factory);
let discovery = PoolDiscovery::for_registry(engine.registry(), config);

// Declare the pool; get it tracked.
for found in discovery.find_uniswap_v3(&mut cache,
    UniswapV3PoolQuery::pair(WETH, USDC).with_fee_tier(3_000))? {
    let mut registration = found.registration;
    registry_view.cold_start(&mut registration, &mut cache, ColdStartPolicy::Eager)?;
    engine.register_pools([registration])?;
}
```

(A `discover_and_track` convenience wrapping exactly that loop is slice 5
sugar once the pieces are proven.)

## 7. Per-protocol mechanics

| Protocol | Resolution (default) | Query mechanics | Creation event | Metadata fill | Difficulty |
| --- | --- | --- | --- | --- | --- |
| Uniswap V2 | **DerivedSlot** (`getPair` mapping) + Create2 cross-check | 1 point read → pair-or-zero | `PairCreated(token0,token1,pair,len)` | tokens from query/event | **low** |
| Uniswap V3 | **DerivedSlot** (`getPool` mapping) + Create2 cross-check (canonical `PoolAddress` derivation) | 1 read per tier; "all tiers" = one **batched** read of {100,500,3000,10000} ∪ config extras | `PoolCreated(token0,token1,fee,tickSpacing,pool)` | fee from query; `tickSpacing` via the `feeAmountTickSpacing` mapping — also a derived slot read | **low** |
| PancakeSwap V3 | **DerivedSlot**; Create2 goes through the separate `PancakeV3PoolDeployer` (own address + init hash in config) | tiers {100,500,2500,10000} batched | same shape | same | **low** |
| Slipstream | **DerivedSlot** when base slot configured; Create2 available via clone init hash + `poolImplementation`; else **ViewCall** | spacings from `tickSpacings()` (one view call, cacheable) then batched slot reads | `PoolCreated(token0,token1,tickSpacing,pool)` | spacing from query/event → layout; sim `fee` via `tickSpacingToFee` | **medium** |
| Solidly V2 | **ViewCall** (fork variance) with DerivedSlot opt-in per fork config | `getPool/getPair(a,b,stable)` × {true,false} | `PoolCreated(token0,token1,stable,pool,len)` (fork-dependent name) | tokens + `stable` from query/event; `SolidlyStorageLayout` still per-fork config | **medium** |
| Curve | **ViewCall** (MetaRegistry — Vyper registry indirection is the textbook fallback case) | `find_pools_for_coins(a,b)` → `address[]`; per pool `get_coins`/`get_n_coins`/`is_meta` | factory `*PoolDeployed` events (slice 6; see caveat) | `coins` from registry; **variant from provenance**; `is_meta` filtered (metapools out of scope) | **medium** (query) / **high** (events) |
| Balancer V2 | **Scan** (no on-chain pair index exists) | one-time Vault log backfill (`PoolRegistered` + `TokensRegistered`), client-side token-set filter | `PoolRegistered(poolId,pool,specialization)` on the singleton Vault — cleanest push story of all | poolId from event/scan; everything else from existing cold-start discovery | **low** (events) / **medium** (scan) |

Notes:

- **Pair queries mean "token set ⊇ {a,b}"** for multi-token pools (Curve
  tricrypto, Balancer weighted): a WBTC/WETH query returns tricrypto (USDT,
  WBTC, WETH). The registration carries the full coin set.
- **Zero mapping answer = no pool** — an empty result, not an error.
- **Curve creation events** are the one genuinely hard push case (4–5 factory
  generations per chain; older `PlainPoolDeployed` events don't carry the
  pool address). The *query* half sidesteps all of it via the MetaRegistry,
  which is why queries ship first and Curve creation events ship last —
  with address-less generations handled by a follow-up
  `find_pools_for_coins` probe rather than receipt archaeology.
- **Balancer's scan is the one provider-bound path** (log ranges are RPC, not
  EVM reads): a free async helper à la `v3_sync::run_full_sync` —
  `scan_balancer_pools(provider, vault, from_block, query) -> Vec<DiscoveredPool>`
  — rather than forcing `PoolFactory::find_pools` to be async for everyone.
  Its result is cached into a consumer-suppliable snapshot so repeat queries
  don't re-scan.

## 8. Execution model

- **Queries are sync and cache-mediated** (`&mut dyn AdapterCache`):
  `DerivedSlot` resolves through `read_storage_slot`/batched verifies against
  the pinned backend; `ViewCall` through `call_raw`. Offline tests need no
  factory bytecode at all for the derived paths — fixtures are just seeded
  mapping slots — and use mock factory bytecode only for the `ViewCall` tier
  (the `adapter_swap_sim` pattern).
- **Bulk resolution rides `storage_sync`:** a watchlist of derived mapping
  slots compiles into the existing calldata slot-loader program — hundreds of
  pair×tier resolutions in a single `eth_call`.
- **Backfill + live stitching:** run queries at the pinned block, start the
  creation-event subscription from that block. `DiscoverySource` carries the
  block so consumers can dedupe the overlap (`PoolDiscovery::decode_creation`
  returning an already-registered key is a no-op admission).
- **Creation logs do not flow through `AmmReactiveHandler`.** They are not
  pool-state events; routing them through the state handler would tangle
  lifecycle with state sync. The consumer loop (or the slice-5
  `DiscoveryDriver` helper) matches `creation_sources()` topics, decodes, and
  admits **between batches** — which is exactly the boundary
  `register_pools` already requires. When the `evm-fork-cache`
  interests-refresh API lands, only this admission step changes (no rebuild);
  the vocabulary and factory drivers are untouched.

## 9. Admission pipeline (unchanged, now fed automatically)

```text
PoolQuery ──find_pools──▶ DiscoveredPool ──cold_start──▶ Ready registration
factory log ─decode_creation─▶      │                          │
                                    └── engine.register_pools ─┘
```

Discovery never mutates the registry or cache itself — it *produces
registrations*. Cold-start remains the single gate that warms state, verifies
slots, and sets `PoolStatus`. This keeps the new surface read-only and keeps
every existing invariant (including eviction attribution) intact.

## 10. Testing strategy

- **Offline (per factory):** seeded-mapping-slot fixtures for `DerivedSlot`
  (no bytecode needed); pure-function goldens for the CREATE2 and slot
  derivations against known mainnet pools (WETH/USDC 0.3% et al. — addresses
  and hashes pinned as constants in the tests); mock factory bytecode for the
  `ViewCall` tier; golden creation-log decode tests from real captured logs;
  query normalization (unsorted token input), zero-address misses,
  `UnsupportedQuery` shapes, `DerivationMismatch` on a deliberately wrong
  init hash.
- **Gated RPC parity (`E2E_RPC_URL`, `#[ignore]`):** at a pinned block,
  assert **all three strategies agree** — `DerivedSlot` == `Create2` ==
  `ViewCall` == the known canonical pools — for every shipped default
  (Slipstream/Solidly against `E2E_BASE_RPC_URL`); Balancer scan over a
  bounded historical range. This is where the shipped constants (base slots,
  init-code hashes, factory addresses) are verified rather than trusted.
- **End-to-end example:** `examples/declarative_discovery.rs` — declare
  WETH/USDC across all protocols, discover → cold-start → register → quote,
  no addresses in user code.

## 11. Slices (each: spec'd tests first, then implementation, then review)

1. **Vocabulary + derivation module + trait + front-end + Uniswap V2/V3** —
   types, `Resolution`, `factory::derive` (pure slot + CREATE2 fns with
   goldens), `PoolFactory`, `FactoryConfig` (incl. derivation constants),
   `PoolDiscovery`, both Uniswap drivers on `DerivedSlot` with cross-check +
   `ViewCall` fallback, creation decode, offline fixtures + gated
   three-way parity. *~3 days.*
2. **Pancake + Slipstream + Solidly** — deployer-based Create2 config,
   spacing enumeration, fork-ABI selector + per-fork resolution opt-in,
   Base-gated parity. *~2 days.*
3. **Curve queries via MetaRegistry** — `find_pools_for_coins`, provenance →
   variant, metapool filtering (ViewCall tier by design). *~2 days.*
4. **Balancer** — creation events + async scan helper with snapshot cache.
   *~2 days.*
5. **Live wiring + polish** — `DiscoveryDriver` (consumer-loop helper:
   creation sources → decode → cold-start → `register_pools`),
   `discover_and_track` sugar, bulk watchlist resolution via `storage_sync`,
   example, `docs/pool-discovery.md`, README, CHANGELOG. *~1–2 days.*
6. **Curve creation events** (hardest, lowest marginal value given slice 3) —
   per-chain factory-generation config, address-less event probe. *Explicitly
   droppable from 0.2.0 if it drags.*

## 12. Non-goals and open questions

**Non-goals (0.2.0):** off-chain indexes (subgraphs) — on-chain answers only;
metapools/lending pools (still out of adapter scope); automatic cold-start
inside discovery (admission stays explicit and consumer-paced); token-symbol
resolution ("WETH" → address is the consumer's lookup).

**Open questions for review:**
1. `verify_derivations` default: proposed **on** (first-use cross-check per
   factory, then out of the hot path). Alternative: off by default, on in
   debug builds only.
2. Should `find_all` fan out sequentially per protocol (simple, proposed) or
   take a concurrency knob? Derived-slot batching already collapses the
   dominant cost; sequential seems fine at this fan-out.
3. `PoolQuery` today models pairs only. Add `PoolQuery::tokens([..])` (exact-set
   match, ≥2 tokens) now or when someone needs it? Proposed: builder-ready,
   implemented later — the type is `#[non_exhaustive]`. Protocol-specific
   selectors should remain on protocol-specific query types.
4. Does `DiscoveryDriver` belong in this crate or the consumer's loop until
   the interests-refresh API lands? Proposed: ship it here but document the
   rebuild cost, same stance as `register_pools`.
