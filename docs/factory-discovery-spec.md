# Factory discovery — design spec (0.2.0)

Status: **draft for review** · Owner: pre-0.2.0 workstream · Supersedes the
sketch in ROADMAP.md's 0.2.0 section (which covered only the event half).

## 1. Goal

Make pool discovery **declarative**. A consumer should never need to paste a
pool address for a protocol the crate supports; they say *what* they want and
get cold-start-ready registrations back:

```rust
// "Give me the pool for WETH/USDC at the 0.3% fee tier."
let pools = discovery.find(&mut cache, ProtocolId::UniswapV3,
    PoolQuery::pair(WETH, USDC).with_fee(3_000))?;

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

- **No reimplemented factory logic.** Exactly like `simulate_swap`, queries
  run the factory's *own* deployed bytecode (`getPool`, `getPair`,
  `find_pools_for_coins`) through `AdapterCache::call_raw` — the same
  `from = ZERO, commit = false` discipline as [`quote_via_call`]. We never
  re-derive CREATE2 addresses or copy factory salt math into Rust.
- **Block-consistent.** Queries through the pinned `EvmCache` answer *as of
  the pinned block*. A pool created later is invisible to the query and
  arrives via the event half instead — no torn view.
- **Crate-owned vocabulary, additive surface.** All new types are
  `#[non_exhaustive]` with constructors/builders; the one `AmmAdapter` hook is
  defaulted. Existing consumers and third-party adapters compile unchanged —
  this ships in 0.2.0 as a semver-minor addition.
- **Config over heuristics.** Factory addresses are per-chain config
  (`FactoryConfig`, mirroring `SimConfig`: canonical mainnet defaults,
  `with_*` overrides, `Option` for protocols with no canonical mainnet
  deployment). Curve variant detection becomes **provenance, not heuristic**:
  the registry/factory that answers for a pool determines its
  `CurveVariant`.

## 3. Vocabulary (new types, all `#[non_exhaustive]`)

```rust
/// What you want from a factory, protocol-agnostically.
pub struct PoolQuery {
    /// The (unordered) token pair. `PoolQuery::pair` normalizes order, so
    /// callers never care about token0/token1 sorting.
    pub tokens: (Address, Address),
    /// Which variant(s) of the pair to resolve.
    pub variant: PoolVariant,
}

impl PoolQuery {
    pub fn pair(a: Address, b: Address) -> Self;          // variant = Any
    pub fn with_fee(self, fee: u32) -> Self;              // V3 family fee tier
    pub fn with_tick_spacing(self, spacing: i32) -> Self; // Slipstream
    pub fn with_stable(self, stable: bool) -> Self;       // Solidly
}

/// Protocol-specific variant selector.
pub enum PoolVariant {
    /// Every variant the factory knows for the pair (fee tiers, tick
    /// spacings, stable+volatile, all matching Curve pools).
    Any,
    FeeTier(u32),
    TickSpacing(i32),
    Stable(bool),
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
    /// Factory call failed — boxed un-flattened cause (CacheError etc.).
    Factory(Box<dyn std::error::Error + Send + Sync + 'static>),
    /// The factory answered but the result didn't decode.
    Malformed(&'static str),
}
```

`FactoryConfig` (mirrors `SimConfig`): `v2_factory`, `v3_factory`,
`pancake_v3_factory`, `balancer_vault`, `curve_meta_registry` default to the
canonical Ethereum-mainnet deployments; `slipstream_factory` and
`solidly_factory` are `Option<Address>` (their canonical homes are Base/OP —
no honest mainnet default) plus a `solidly_abi` fork selector
(`getPool(address,address,bool)` vs legacy `getPair`) and the existing
per-fork `SolidlyStorageLayout` hook. All addresses verified by the gated RPC
tests at implementation time, not trusted from memory.

## 4. The `PoolFactory` trait and the adapter hook

```rust
/// Per-protocol factory driver: point queries + creation-event decoding.
pub trait PoolFactory: Send + Sync {
    fn protocol(&self) -> ProtocolId;

    /// Resolve existing pools for a query against the (pinned) cache.
    /// Runs the factory's own bytecode via `AdapterCache::call_raw`.
    fn find_pools(
        &self,
        cache: &mut dyn AdapterCache,
        query: &PoolQuery,
    ) -> Result<Vec<DiscoveredPool>, DiscoveryError>;

    /// Factory emitters + creation topics for the live (push) half.
    fn creation_sources(&self) -> Vec<EventSource>;

    /// Decode one factory log into a discovered pool (None = not a
    /// creation log this factory owns).
    fn decode_creation(&self, log: &Log) -> Result<Option<DiscoveredPool>, DiscoveryError>;
}
```

`AmmAdapter` gains **one defaulted method** (replacing the two-method ROADMAP
sketch — the factory object subsumes both):

```rust
fn pool_factory(&self, config: &FactoryConfig) -> Option<Box<dyn PoolFactory>> {
    None // opt-in per adapter; third-party adapters unchanged
}
```

The factory is a *separate object* rather than more adapter methods because it
carries chain config (addresses, tier lists, fork ABIs) that the chain-neutral
adapter deliberately doesn't. Third parties can also implement `PoolFactory`
directly for custom AMMs without touching their adapter.

## 5. The `PoolDiscovery` front-end

```rust
pub struct PoolDiscovery { /* HashMap<ProtocolId, Box<dyn PoolFactory>> */ }

impl PoolDiscovery {
    /// Assemble from a registry's adapters + factory config: every registered
    /// adapter that opts in contributes its factory.
    pub fn for_registry(registry: &AdapterRegistry, config: FactoryConfig) -> Self;

    /// One protocol, one query.
    pub fn find(&self, cache: &mut dyn AdapterCache, protocol: ProtocolId, query: PoolQuery)
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
let discovery = PoolDiscovery::for_registry(engine.registry(), FactoryConfig::default());

// Declare the pool; get it tracked.
for found in discovery.find(&mut cache, ProtocolId::UniswapV3,
    PoolQuery::pair(WETH, USDC).with_fee(3_000))? {
    let mut registration = found.registration;
    registry_view.cold_start(&mut registration, &mut cache, ColdStartPolicy::Eager)?;
    engine.register_pools([registration])?;
}
```

(A `discover_and_track` convenience wrapping exactly that loop is slice 5
sugar once the pieces are proven.)

## 6. Per-protocol mechanics

| Protocol | Query call(s) (via `call_raw`) | "All pools for pair" | Creation event | Metadata fill | Difficulty |
| --- | --- | --- | --- | --- | --- |
| Uniswap V2 | `getPair(a,b)` | the single pair | `PairCreated(token0,token1,pair,len)` | tokens from query/event | **low** |
| Uniswap V3 | `getPool(a,b,fee)` | probe enabled tiers {100,500,3000,10000} ∪ config extras | `PoolCreated(token0,token1,fee,tickSpacing,pool)` | fee from query; `tickSpacing` via `feeAmountTickSpacing(fee)` (one cached call) → layout derivable | **low** |
| PancakeSwap V3 | same ABI | tiers {100,500,2500,10000} | same shape | same | **low** |
| Slipstream | `getPool(a,b,tickSpacing)` | enumerate `tickSpacings()` (on-chain array — no hardcoding) | `PoolCreated(token0,token1,tickSpacing,pool)` | spacing from query/event → layout; sim `fee` via `tickSpacingToFee(spacing)` or pool `fee()` (one cached call) | **medium** |
| Solidly V2 | `getPool(a,b,stable)` (fork ABI selector; legacy `getPair`) | probe `stable ∈ {true,false}` | `PoolCreated(token0,token1,stable,pool,len)` (fork-dependent name) | tokens + `stable` from query/event; `SolidlyStorageLayout` still per-fork config — factory discovery is only as automatic as that table | **medium** |
| Curve | MetaRegistry `find_pools_for_coins(a,b)` → `address[]`; per pool `get_coins`/`get_n_coins`/`is_meta` | native — the registry returns the list | factory `*PoolDeployed` events (slice 6; see caveat) | `coins` from registry; **variant from provenance** (which base registry/factory handles the pool), killing the 0.1.0 heuristic; `is_meta == true` filtered out (metapools out of scope) | **medium** (query) / **high** (events) |
| Balancer V2 | — no on-chain pair index exists | **scan**: one-time Vault log backfill (`PoolRegistered` + `TokensRegistered`), client-side token-set filter (tokens aren't indexed) | `PoolRegistered(poolId,pool,specialization)` on the singleton Vault — cleanest push story of all | poolId from event/scan; everything else from existing cold-start discovery | **low** (events) / **medium** (scan) |

Notes:

- **Pair queries mean "token set ⊇ {a,b}"** for multi-token pools (Curve
  tricrypto, Balancer weighted): a WBTC/WETH query returns tricrypto (USDT,
  WBTC, WETH). The registration carries the full coin set.
- **Zero address = no pool** (V2/V3/Slipstream/Solidly return
  `address(0)`) — an empty result, not an error.
- **Curve creation events** are the one genuinely hard push case (4–5 factory
  generations per chain; older `PlainPoolDeployed` events don't carry the
  pool address). The *query* half sidesteps all of it via the MetaRegistry,
  which is why queries ship first and Curve creation events ship last —
  with address-less generations handled by a follow-up
  `find_pools_for_coins` probe rather than receipt archaeology.
- **Balancer's scan is the one provider-bound path** (log ranges are RPC, not
  EVM calls): a free async helper à la `v3_sync::run_full_sync` —
  `scan_balancer_pools(provider, vault, from_block, query) -> Vec<DiscoveredPool>`
  — rather than forcing `PoolFactory::find_pools` to be async for everyone.
  Its result is cached into a consumer-suppliable snapshot so repeat queries
  don't re-scan.

## 7. Execution model

- **Queries are sync and cache-mediated** (`&mut dyn AdapterCache`): they hit
  warmed factory state or lazily fetch through the pinned backend, exactly
  like quotes. Offline tests use mock factory bytecode fixtures — the same
  pattern as `adapter_swap_sim`'s mock quote contracts — so the whole query
  layer tests without RPC.
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

## 8. Admission pipeline (unchanged, now fed automatically)

```text
PoolQuery ──find_pools──▶ DiscoveredPool ──cold_start──▶ Ready registration
factory log ─decode_creation─▶      │                          │
                                    └── engine.register_pools ─┘
```

Discovery never mutates the registry or cache itself — it *produces
registrations*. Cold-start remains the single gate that warms state, verifies
slots, and sets `PoolStatus`. This keeps the new surface read-only and keeps
every existing invariant (including eviction attribution) intact.

## 9. Testing strategy

- **Offline (per factory):** mock factory bytecode fixtures answering
  `getPool`/`getPair`/`find_pools_for_coins`; golden creation-log decode
  tests from real captured logs; query normalization (unsorted token input),
  zero-address misses, `UnsupportedQuery` shapes, tier/spacing enumeration.
- **Gated RPC parity (`E2E_RPC_URL`, `#[ignore]`):** at a pinned block, assert
  the canonical answers — e.g. V3 WETH/USDC 0.3% resolves to the known
  mainnet pool, `find_all(WBTC, WETH)` includes the V2 pair + V3 tiers +
  tricrypto; Slipstream/Solidly against `E2E_BASE_RPC_URL`; Balancer scan
  over a bounded historical range. These tests are also where factory
  addresses get verified rather than trusted.
- **End-to-end example:** `examples/declarative_discovery.rs` — declare
  WETH/USDC across all protocols, discover → cold-start → register → quote,
  no addresses in user code.

## 10. Slices (each: spec'd tests first, then implementation, then review)

1. **Vocabulary + trait + front-end + Uniswap V2/V3** — types, `PoolFactory`,
   `FactoryConfig`, `PoolDiscovery`, both Uniswap factories (query + creation
   decode), offline fixtures + gated parity. *~2–3 days.*
2. **Pancake + Slipstream + Solidly** — tier/spacing/stable enumeration,
   fork-ABI selector, Base-gated parity. *~2 days.*
3. **Curve queries via MetaRegistry** — `find_pools_for_coins`, provenance →
   variant, metapool filtering. *~2 days.*
4. **Balancer** — creation events + async scan helper with snapshot cache.
   *~2 days.*
5. **Live wiring + polish** — `DiscoveryDriver` (consumer-loop helper:
   creation sources → decode → cold-start → `register_pools`),
   `discover_and_track` sugar, example, `docs/pool-discovery.md`, README,
   CHANGELOG. *~1–2 days.*
6. **Curve creation events** (hardest, lowest marginal value given slice 3) —
   per-chain factory-generation config, address-less event probe. *Explicitly
   droppable from 0.2.0 if it drags.*

## 11. Non-goals and open questions

**Non-goals (0.2.0):** off-chain indexes (subgraphs) — on-chain answers only;
metapools/lending pools (still out of adapter scope); automatic cold-start
inside discovery (admission stays explicit and consumer-paced); token-symbol
resolution ("WETH" → address is the consumer's lookup).

**Open questions for review:**
1. Should `find_all` fan out sequentially per protocol (simple, proposed) or
   take a concurrency knob? Point queries are a handful of `call_raw`s each —
   sequential seems fine at this fan-out.
2. `PoolQuery` today models pairs. Add `PoolQuery::tokens([..])` (exact-set
   match, ≥2 tokens) now or when someone needs it? Proposed: builder-ready,
   implemented later — the type is `#[non_exhaustive]`.
3. Does `DiscoveryDriver` belong in this crate or the consumer's loop until
   the interests-refresh API lands? Proposed: ship it here but document the
   rebuild cost, same stance as `register_pools`.
