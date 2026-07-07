use std::any::Any;
use std::fmt;
use std::sync::Arc;

use alloy_primitives::{Address, B256, Bytes, U256};

use super::cache::{SlotChange, StateDiff, StateUpdate};
use super::storage::{SolidlyStorageLayout, V3StorageLayout};

/// Protocol family identifier for adapter registrations.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ProtocolId {
    /// Uniswap V2 constant-product pairs.
    UniswapV2,
    /// Uniswap V3 concentrated-liquidity pools.
    UniswapV3,
    /// PancakeSwap V3 (Uniswap V3-family with its own fee tiers / slot layout).
    PancakeV3,
    /// Slipstream / Aerodrome concentrated-liquidity (tickSpacing-keyed).
    Slipstream,
    /// Solidly V2 (Aerodrome / Velodrome) reserves pools.
    SolidlyV2,
    /// Balancer V2 (shared-vault) pools.
    BalancerV2,
    /// Balancer V3 — reserved identity, no adapter yet.
    #[cfg(feature = "experimental-protocols")]
    BalancerV3,
    /// Curve StableSwap / CryptoSwap family pools.
    Curve,
    /// ERC-4626 tokenized vaults — reserved identity, no adapter yet.
    #[cfg(feature = "experimental-protocols")]
    Erc4626,
    /// Uniswap V4 — reserved identity, no adapter yet.
    #[cfg(feature = "experimental-protocols")]
    UniswapV4,
    /// A third-party protocol, identified by a `'static` name.
    Custom(&'static str),
}

/// Protocol-specific pool identity.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum PoolKey {
    /// Uniswap V2 pair, keyed by pool address.
    UniswapV2(Address),
    /// Uniswap V3 pool, keyed by pool address.
    UniswapV3(Address),
    /// PancakeSwap V3 pool, keyed by pool address.
    PancakeV3(Address),
    /// Slipstream / Aerodrome CL pool, keyed by pool address.
    Slipstream(Address),
    /// Solidly V2 pool, keyed by pool address.
    SolidlyV2(Address),
    /// Balancer V2 pool, keyed by its 32-byte `poolId`.
    BalancerV2(B256),
    /// Balancer V3 pool, keyed by pool address (reserved; no adapter yet).
    #[cfg(feature = "experimental-protocols")]
    BalancerV3(Address),
    /// Curve pool, keyed by pool address.
    Curve(Address),
    /// ERC-4626 vault, keyed by address (reserved; no adapter yet).
    #[cfg(feature = "experimental-protocols")]
    Erc4626(Address),
    /// Uniswap V4 pool, keyed by its 32-byte pool id (reserved; no adapter yet).
    #[cfg(feature = "experimental-protocols")]
    UniswapV4(B256),
    /// A third-party pool identity (see [`CustomPoolKey`]).
    Custom(CustomPoolKey),
}

impl PoolKey {
    /// Return the protocol family for this pool key.
    pub fn protocol(&self) -> ProtocolId {
        match self {
            Self::UniswapV2(_) => ProtocolId::UniswapV2,
            Self::UniswapV3(_) => ProtocolId::UniswapV3,
            Self::PancakeV3(_) => ProtocolId::PancakeV3,
            Self::Slipstream(_) => ProtocolId::Slipstream,
            Self::SolidlyV2(_) => ProtocolId::SolidlyV2,
            Self::BalancerV2(_) => ProtocolId::BalancerV2,
            #[cfg(feature = "experimental-protocols")]
            Self::BalancerV3(_) => ProtocolId::BalancerV3,
            Self::Curve(_) => ProtocolId::Curve,
            #[cfg(feature = "experimental-protocols")]
            Self::Erc4626(_) => ProtocolId::Erc4626,
            #[cfg(feature = "experimental-protocols")]
            Self::UniswapV4(_) => ProtocolId::UniswapV4,
            Self::Custom(key) => key.protocol(),
        }
    }

    /// Return the address identity for address-keyed pools.
    pub fn address(&self) -> Option<Address> {
        match self {
            Self::UniswapV2(address)
            | Self::UniswapV3(address)
            | Self::PancakeV3(address)
            | Self::Slipstream(address)
            | Self::SolidlyV2(address)
            | Self::Curve(address) => Some(*address),
            #[cfg(feature = "experimental-protocols")]
            Self::BalancerV3(address) | Self::Erc4626(address) => Some(*address),
            Self::Custom(key) => key.address(),
            Self::BalancerV2(_) => None,
            #[cfg(feature = "experimental-protocols")]
            Self::UniswapV4(_) => None,
        }
    }

    /// Return the bytes32 identity for bytes32-keyed pools.
    pub fn bytes32(&self) -> Option<B256> {
        match self {
            Self::BalancerV2(id) => Some(*id),
            #[cfg(feature = "experimental-protocols")]
            Self::UniswapV4(id) => Some(*id),
            Self::Custom(key) => key.bytes32(),
            Self::UniswapV2(_)
            | Self::UniswapV3(_)
            | Self::PancakeV3(_)
            | Self::Slipstream(_)
            | Self::SolidlyV2(_)
            | Self::Curve(_) => None,
            #[cfg(feature = "experimental-protocols")]
            Self::BalancerV3(_) | Self::Erc4626(_) => None,
        }
    }
}

/// Extension point for protocol-specific pool key shapes.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum CustomPoolKey {
    /// An address-keyed custom pool.
    Address {
        /// The custom protocol's `'static` name.
        protocol: &'static str,
        /// The pool's contract address.
        address: Address,
    },
    /// A bytes32-keyed custom pool (e.g. a vault-style pool id).
    Bytes32 {
        /// The custom protocol's `'static` name.
        protocol: &'static str,
        /// The pool's 32-byte identifier.
        id: B256,
    },
    /// A custom pool identified by both an address and a bytes32 id.
    Composite {
        /// The custom protocol's `'static` name.
        protocol: &'static str,
        /// The pool's contract address.
        address: Address,
        /// The pool's 32-byte identifier.
        id: B256,
    },
}

impl CustomPoolKey {
    /// The [`ProtocolId::Custom`] this key belongs to.
    pub fn protocol(&self) -> ProtocolId {
        match self {
            Self::Address { protocol, .. }
            | Self::Bytes32 { protocol, .. }
            | Self::Composite { protocol, .. } => ProtocolId::Custom(protocol),
        }
    }

    /// The pool's contract address, for address- or composite-keyed variants.
    pub fn address(&self) -> Option<Address> {
        match self {
            Self::Address { address, .. } | Self::Composite { address, .. } => Some(*address),
            Self::Bytes32 { .. } => None,
        }
    }

    /// The pool's 32-byte id, for bytes32- or composite-keyed variants.
    pub fn bytes32(&self) -> Option<B256> {
        match self {
            Self::Bytes32 { id, .. } | Self::Composite { id, .. } => Some(*id),
            Self::Address { .. } => None,
        }
    }
}

/// One log emitter and routing rule for a tracked pool.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EventSource {
    /// The contract address that emits the log.
    pub emitter: Address,
    /// The `topic0` signature hashes this source matches (empty = any topic).
    pub topics: Vec<B256>,
    /// How a matched log is routed to a pool key.
    pub route: EventRoute,
}

impl EventSource {
    /// A source whose logs route directly by emitter address.
    pub fn direct(emitter: Address, topics: Vec<B256>) -> Self {
        Self {
            emitter,
            topics,
            route: EventRoute::Direct,
        }
    }

    /// A source whose logs route by an indexed **address** topic at `topic_index`.
    pub fn indexed_address(emitter: Address, topics: Vec<B256>, topic_index: usize) -> Self {
        Self {
            emitter,
            topics,
            route: EventRoute::IndexedAddress { topic_index },
        }
    }

    /// A source whose logs route by an indexed **bytes32** topic at `topic_index`.
    pub fn indexed_bytes32(emitter: Address, topics: Vec<B256>, topic_index: usize) -> Self {
        Self {
            emitter,
            topics,
            route: EventRoute::IndexedBytes32 { topic_index },
        }
    }

    /// A source whose routing is decided by the adapter's own `route_log`.
    pub fn adapter_defined(emitter: Address, topics: Vec<B256>) -> Self {
        Self {
            emitter,
            topics,
            route: EventRoute::AdapterDefined,
        }
    }
}

/// Generic routing rule for a log emitted by an [`EventSource`].
///
/// Deliberately exhaustive (unlike most enums in this crate): this is a closed
/// routing vocabulary the engine matches on — a new route kind changes
/// dispatch semantics and warrants a breaking release.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EventRoute {
    /// The log belongs to the pool whose key address is the emitter.
    Direct,
    /// Route by an indexed address topic at `topic_index` (the low 20 bytes).
    IndexedAddress {
        /// Index of the topic carrying the pool address.
        topic_index: usize,
    },
    /// Route by an indexed bytes32 topic at `topic_index` (e.g. a Balancer poolId).
    IndexedBytes32 {
        /// Index of the topic carrying the pool's bytes32 id.
        topic_index: usize,
    },
    /// Routing is delegated to the adapter's own `route_log`.
    AdapterDefined,
}

/// Per-pool sidecar registration owned by `evm-amm-state`.
#[non_exhaustive]
#[derive(Clone, Debug)]
pub struct PoolRegistration {
    /// The pool's protocol-specific identity.
    pub key: PoolKey,
    /// Contract addresses whose storage backs this pool (pool and/or vault).
    pub state_addresses: Vec<Address>,
    /// Log sources to subscribe and route for this pool.
    pub event_sources: Vec<EventSource>,
    /// Protocol metadata (tokens, fee, layout, discovered slots, …).
    pub metadata: ProtocolMetadata,
    /// Lifecycle status of the registration.
    pub status: PoolStatus,
}

impl PoolRegistration {
    /// A new registration for `key` with empty sources/metadata and
    /// [`PoolStatus::Pending`].
    pub fn new(key: PoolKey) -> Self {
        Self {
            key,
            state_addresses: Vec::new(),
            event_sources: Vec::new(),
            metadata: ProtocolMetadata::Unknown,
            status: PoolStatus::Pending,
        }
    }

    /// The pool's protocol family (from its [`key`](Self::key)).
    pub fn protocol(&self) -> ProtocolId {
        self.key.protocol()
    }

    /// Add one backing state address.
    pub fn with_state_address(mut self, address: Address) -> Self {
        self.state_addresses.push(address);
        self
    }

    /// Add several backing state addresses.
    pub fn with_state_addresses(mut self, addresses: impl IntoIterator<Item = Address>) -> Self {
        self.state_addresses.extend(addresses);
        self
    }

    /// Add one event source.
    pub fn with_event_source(mut self, source: EventSource) -> Self {
        self.event_sources.push(source);
        self
    }

    /// Add several event sources.
    pub fn with_event_sources(mut self, sources: impl IntoIterator<Item = EventSource>) -> Self {
        self.event_sources.extend(sources);
        self
    }

    /// Set the protocol metadata.
    pub fn with_metadata(mut self, metadata: ProtocolMetadata) -> Self {
        self.metadata = metadata;
        self
    }

    /// Set the lifecycle status.
    pub fn with_status(mut self, status: PoolStatus) -> Self {
        self.status = status;
        self
    }
}

/// Protocol metadata known for a tracked pool.
#[non_exhaustive]
#[derive(Clone, Default)]
pub enum ProtocolMetadata {
    /// No metadata known yet (the default before cold-start/registration fills it).
    #[default]
    Unknown,
    /// Uniswap V2 pair metadata.
    UniswapV2(UniswapV2Metadata),
    /// Uniswap V3 pool metadata.
    UniswapV3(V3Metadata),
    /// PancakeSwap V3 pool metadata (shares [`V3Metadata`]).
    PancakeV3(V3Metadata),
    /// Slipstream / Aerodrome CL pool metadata (shares [`V3Metadata`]).
    Slipstream(V3Metadata),
    /// Balancer V2 pool metadata.
    BalancerV2(BalancerV2Metadata),
    /// Solidly V2 pool metadata.
    SolidlyV2(SolidlyV2Metadata),
    /// Curve pool metadata.
    Curve(CurveMetadata),
    /// Opaque third-party metadata, downcast by the custom adapter.
    Custom(Arc<dyn Any + Send + Sync>),
}

impl fmt::Debug for ProtocolMetadata {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unknown => f.write_str("Unknown"),
            Self::UniswapV2(metadata) => f.debug_tuple("UniswapV2").field(metadata).finish(),
            Self::UniswapV3(metadata) => f.debug_tuple("UniswapV3").field(metadata).finish(),
            Self::PancakeV3(metadata) => f.debug_tuple("PancakeV3").field(metadata).finish(),
            Self::Slipstream(metadata) => f.debug_tuple("Slipstream").field(metadata).finish(),
            Self::BalancerV2(metadata) => f.debug_tuple("BalancerV2").field(metadata).finish(),
            Self::SolidlyV2(metadata) => f.debug_tuple("SolidlyV2").field(metadata).finish(),
            Self::Curve(metadata) => f.debug_tuple("Curve").field(metadata).finish(),
            Self::Custom(_) => f.write_str("Custom(..)"),
        }
    }
}

/// Metadata for a Uniswap V2 pair.
#[non_exhaustive]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UniswapV2Metadata {
    /// The pair's `token0` (decoded from storage at cold-start when unset).
    pub token0: Option<Address>,
    /// The pair's `token1` (decoded from storage at cold-start when unset).
    pub token1: Option<Address>,
    /// Config-supplied swap fee in basis points (V2 has no on-chain fee slot).
    pub fee_bps: Option<u32>,
}

impl UniswapV2Metadata {
    /// Set the pool's `token0` address.
    pub fn with_token0(mut self, token0: Address) -> Self {
        self.token0 = Some(token0);
        self
    }

    /// Set the pool's `token1` address.
    pub fn with_token1(mut self, token1: Address) -> Self {
        self.token1 = Some(token1);
        self
    }

    /// Set the swap fee in basis points (e.g. `30` = 0.30%).
    pub fn with_fee_bps(mut self, fee_bps: u32) -> Self {
        self.fee_bps = Some(fee_bps);
        self
    }
}

/// Metadata for a Uniswap V3-family pool (Uniswap V3 / PancakeSwap V3 / Slipstream).
#[non_exhaustive]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct V3Metadata {
    /// The pool's `token0`.
    pub token0: Option<Address>,
    /// The pool's `token1`.
    pub token1: Option<Address>,
    /// The pool fee in hundredths of a bip (e.g. `500` = 0.05%). Required for
    /// `simulate_swap` (the QuoterV2 fee argument).
    pub fee: Option<u32>,
    /// The pool's tick spacing (drives the derived storage layout when no
    /// explicit `storage_layout` is set).
    pub tick_spacing: Option<i32>,
    /// Factory/deployer address embedded as an immutable in canonical Uniswap V3
    /// pool bytecode. Factory discovery fills this; manual registrations can set
    /// it explicitly when they want bytecode seeding.
    pub factory: Option<Address>,
    /// Per-pool swap-quote target (a fork's own QuoterV2). When set, swap
    /// simulation quotes against this address instead of the caller's
    /// [`SimConfig::v3_quoter`](super::SimConfig::v3_quoter) — so a discovered
    /// PancakeSwap pool quotes against Pancake's quoter. `None` falls back to the
    /// caller's configured quoter. Factory discovery fills this from the
    /// fork's [`ClFactorySpec`](super::factory::ClFactorySpec) quoter.
    pub quoter: Option<Address>,
    /// Explicit V3 storage layout (slot bases + tick spacing). When unset it is
    /// derived from `tick_spacing` per the pool's family.
    pub storage_layout: Option<V3StorageLayout>,
    /// The ± radius, in tick-bitmap words, of the cold-start tick-warm window
    /// around the current word (`Strict`/`Eager` policies).
    ///
    /// `None` uses the crate default (`V3_TICK_WORD_RADIUS`, currently 2).
    /// `Some(0)` warms only the current word. Larger values pre-warm more tick
    /// data so wider tick-crossing swaps stay fully offline, at higher
    /// cold-start cost.
    pub warm_word_radius: Option<i16>,
}

impl V3Metadata {
    /// Set the pool's `token0` address.
    pub fn with_token0(mut self, token0: Address) -> Self {
        self.token0 = Some(token0);
        self
    }

    /// Set the pool's `token1` address.
    pub fn with_token1(mut self, token1: Address) -> Self {
        self.token1 = Some(token1);
        self
    }

    /// Set the pool fee in hundredths of a bip (e.g. `500` = 0.05%).
    pub fn with_fee(mut self, fee: u32) -> Self {
        self.fee = Some(fee);
        self
    }

    /// Set the pool's tick spacing.
    pub fn with_tick_spacing(mut self, tick_spacing: i32) -> Self {
        self.tick_spacing = Some(tick_spacing);
        self
    }

    /// Set the pool factory/deployer address.
    pub fn with_factory(mut self, factory: Address) -> Self {
        self.factory = Some(factory);
        self
    }

    /// Set the per-pool swap-quote target (see [`quoter`](Self::quoter)).
    pub fn with_quoter(mut self, quoter: Address) -> Self {
        self.quoter = Some(quoter);
        self
    }

    /// Set the pool's V3 storage layout descriptor.
    pub fn with_storage_layout(mut self, storage_layout: V3StorageLayout) -> Self {
        self.storage_layout = Some(storage_layout);
        self
    }

    /// Set the cold-start tick-warm ± word radius (see field docs).
    pub fn with_warm_word_radius(mut self, warm_word_radius: i16) -> Self {
        self.warm_word_radius = Some(warm_word_radius);
        self
    }
}

/// Metadata for a Solidly V2 (Aerodrome / Velodrome) reserves pool.
#[non_exhaustive]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SolidlyV2Metadata {
    /// The pool's `token0` (decoded from the config layout at cold-start).
    pub token0: Option<Address>,
    /// The pool's `token1` (decoded from the config layout at cold-start).
    pub token1: Option<Address>,
    /// `true` for stable (x³y+y³x) pools, `false` for volatile (xy=k). Config-
    /// supplied; preserved across cold-start.
    pub stable: Option<bool>,
    /// Fork-specific reserve/token storage layout (config-supplied; no default).
    pub storage_layout: Option<SolidlyStorageLayout>,
}

impl SolidlyV2Metadata {
    /// Set the pool's `token0` address.
    pub fn with_token0(mut self, token0: Address) -> Self {
        self.token0 = Some(token0);
        self
    }

    /// Set the pool's `token1` address.
    pub fn with_token1(mut self, token1: Address) -> Self {
        self.token1 = Some(token1);
        self
    }

    /// Set whether the pool is stable (`true`) or volatile (`false`).
    pub fn with_stable(mut self, stable: bool) -> Self {
        self.stable = Some(stable);
        self
    }

    /// Set the pool's Solidly storage layout descriptor (fork-specific slots).
    pub fn with_storage_layout(mut self, storage_layout: SolidlyStorageLayout) -> Self {
        self.storage_layout = Some(storage_layout);
        self
    }
}

/// Metadata for a Balancer V2 pool.
#[non_exhaustive]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BalancerV2Metadata {
    /// The Balancer `Vault` address (the swap/quote target).
    pub vault: Option<Address>,
    /// The pool's own contract address (distinct from the shared vault).
    pub pool_address: Option<Address>,
    /// The pool's registered token list (from `getPoolTokens`).
    pub tokens: Vec<Address>,
    /// Vault balance storage slots discovered during cold-start (the `(vault,
    /// slot)` pairs the `getPoolTokens` view-call SLOADed; recorded slot-only
    /// since they all live on `vault`).
    ///
    /// Persisting them here lets the reactive `Swap` path refresh (re-verify)
    /// exactly these slots — keeping the cached vault balances fresh for a
    /// subsequent `simulate_swap` — without reverse-engineering the vault's
    /// balance-mapping layout or doing lossy event-delta arithmetic. Empty
    /// until the discover→verify cold-start runs.
    pub balance_slots: Vec<U256>,
}

impl BalancerV2Metadata {
    /// Set the Balancer `Vault` address.
    pub fn with_vault(mut self, vault: Address) -> Self {
        self.vault = Some(vault);
        self
    }

    /// Set the pool's own contract address.
    pub fn with_pool_address(mut self, pool_address: Address) -> Self {
        self.pool_address = Some(pool_address);
        self
    }

    /// Set (replace) the pool's token list.
    pub fn with_tokens(mut self, tokens: impl IntoIterator<Item = Address>) -> Self {
        self.tokens = tokens.into_iter().collect();
        self
    }

    /// Set (replace) the discovered vault balance storage slots.
    pub fn with_balance_slots(mut self, balance_slots: impl IntoIterator<Item = U256>) -> Self {
        self.balance_slots = balance_slots.into_iter().collect();
        self
    }
}

/// Which Curve pool dialect a pool speaks — selects the `get_dy` / `TokenExchange`
/// index ABI (the slice-1 vs slice-2 axis).
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CurveVariant {
    /// Classic StableSwap **and** StableSwap-NG: `get_dy(int128,int128,uint256)`
    /// and `TokenExchange(address,int128,uint256,int128,uint256)`.
    #[default]
    StableSwap,
    /// CryptoSwap (Curve v2, e.g. tricrypto2): `get_dy(uint256,uint256,uint256)`
    /// and `TokenExchange(address,uint256,uint256,uint256,uint256)`.
    CryptoSwap,
    /// Tricrypto-NG (Curve's newest crypto pools, e.g. tricryptoUSDC/USDT): the
    /// SAME `uint256` `get_dy` as CryptoSwap, but EXTENDED events (a 7-arg
    /// `TokenExchange` with `fee`/`packed_price_scale`, a 5-arg `AddLiquidity`, a
    /// 6-arg `RemoveLiquidityOne`, plus `ClaimAdminFee`).
    CryptoSwapNG,
}

/// Metadata for a Curve plain pool.
///
/// `coins` is config-supplied (the pool's static coin ordering); it drives the
/// `simulate_swap` token→index mapping for `get_dy`. `discovered_slots` is the
/// storage read-set the cold-start discover pass captured from a `get_dy` call
/// (balances + amplification + fee, wherever the Vyper build placed them) — a
/// real Curve pool has no predictable balance-slot layout, so discovery, not a
/// hand-coded layout, identifies them. Persisting them lets the reactive
/// `TokenExchange`/liquidity path re-verify exactly those slots (a resync),
/// keeping cached state fresh for a later `simulate_swap`. Slot-only; all live
/// on the pool address. Empty until cold-start runs.
///
/// **Pre-populating `discovered_slots`** (from a prior discovery, a block trace,
/// or a MetaRegistry-backed source) turns the otherwise unavoidable
/// discover→verify cold start into a single verify round: `cold_start` skips the
/// local `get_dy` discovery entirely, and the pool becomes eligible for the fast
/// bundled [`cold_start_many`](super::AdapterRegistry::cold_start_many) /
/// [`storage_sync`](super::storage_sync) path — the same one-shot hydration
/// Uniswap V2/V3 use. A stale/incomplete set is safe: verify refreshes what it
/// has and the first `simulate_swap` lazily faults any missing slot.
///
/// `variant` selects the index ABI (`StableSwap`/NG use `int128`; `CryptoSwap`
/// uses `uint256`). Defaults to `StableSwap` (slice-1 + NG behavior).
///
/// `code_seed` is an **optional** caller-supplied canonical runtime bytecode for
/// the pool. Curve pools are per-pool Vyper builds with no shared template
/// (unlike Uniswap V2's shared pair runtime or V3's rendered template), so the
/// crate embeds no Curve seed — but a caller that already knows a pool's runtime
/// can attach it here. Cold-start verifies it once against the on-chain
/// `EXTCODEHASH` (a mismatch is purged, falling back to lazily fetching the real
/// code — never a correctness risk), removing the one lazy code fetch a Curve
/// pool otherwise pays on its first `simulate_swap`. Empty/`None` = lazy fetch.
#[non_exhaustive]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CurveMetadata {
    /// The pool's static coin ordering (drives the `get_dy` token→index map).
    pub coins: Vec<Address>,
    /// The `get_dy` read-set discovered at cold-start (balances + A + fee),
    /// re-verified by the reactive path. Empty until discovery runs. Pre-fill it
    /// to skip discovery (a verify-only cold start) and enable the fast bundled
    /// hydration path.
    pub discovered_slots: Vec<U256>,
    /// The pool dialect selecting the `get_dy` / `TokenExchange` index ABI.
    pub variant: CurveVariant,
    /// Optional caller-supplied canonical runtime bytecode for the pool, seeded
    /// and verified once against on-chain code at cold-start. `None` (the
    /// default) lazily fetches the real code on first simulate.
    pub code_seed: Option<Bytes>,
}

impl CurveMetadata {
    /// Set (replace) the pool's static coin ordering.
    pub fn with_coins(mut self, coins: impl IntoIterator<Item = Address>) -> Self {
        self.coins = coins.into_iter().collect();
        self
    }

    /// Set (replace) the discovered storage read-set slots.
    pub fn with_discovered_slots(
        mut self,
        discovered_slots: impl IntoIterator<Item = U256>,
    ) -> Self {
        self.discovered_slots = discovered_slots.into_iter().collect();
        self
    }

    /// Set the Curve pool dialect (index ABI) variant.
    pub fn with_variant(mut self, variant: CurveVariant) -> Self {
        self.variant = variant;
        self
    }

    /// Attach an optional canonical runtime bytecode seed for the pool.
    ///
    /// Cold-start verifies it once against the on-chain `EXTCODEHASH`; a mismatch
    /// is purged and the pool falls back to lazily fetching its real code, so a
    /// wrong seed is a latency question, never a correctness one.
    pub fn with_code_seed(mut self, code_seed: impl Into<Bytes>) -> Self {
        self.code_seed = Some(code_seed.into());
        self
    }
}

/// Lifecycle status for a tracked pool registration.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum PoolStatus {
    /// Registered but not yet cold-started.
    #[default]
    Pending,
    /// Cold-start in progress / partially warmed.
    Cold,
    /// Warmed and ready to simulate.
    Ready,
    /// Warmed but a repair target failed; state may be stale until a resync.
    Degraded,
    /// Explicitly disabled by the caller.
    Disabled,
    /// The protocol/layout is not supported for this pool.
    Unsupported,
}

/// Adapter-derived semantic event and cache mutations.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdapterEvent {
    /// The pool this event belongs to.
    pub pool: PoolKey,
    /// The log's emitter address.
    pub emitter: Address,
    /// The log's `topic0` signature hash.
    pub topic0: B256,
    /// The high-level event class.
    pub kind: AdapterEventKind,
    /// Cache mutations this event applies.
    pub updates: Vec<StateUpdate>,
    /// Quality of the emitted updates (exact vs. needs-repair).
    pub quality: UpdateQuality,
    /// Follow-up repair action to combine after applying `updates`.
    pub repair: RepairAction,
}

impl AdapterEvent {
    /// Construct an event with no state updates and no repair; chain
    /// [`with_updates`](Self::with_updates) / [`with_repair`](Self::with_repair)
    /// to add them.
    pub fn new(
        pool: PoolKey,
        emitter: Address,
        topic0: B256,
        kind: AdapterEventKind,
        quality: UpdateQuality,
    ) -> Self {
        Self {
            pool,
            emitter,
            topic0,
            kind,
            updates: Vec::new(),
            quality,
            repair: RepairAction::None,
        }
    }

    /// Set the cache mutations this event emits.
    pub fn with_updates(mut self, updates: impl IntoIterator<Item = StateUpdate>) -> Self {
        self.updates = updates.into_iter().collect();
        self
    }

    /// Set the follow-up repair action for this event.
    pub fn with_repair(mut self, repair: RepairAction) -> Self {
        self.repair = repair;
        self
    }
}

/// Structured result of routing, decoding, and applying one adapter event.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdapterEventReport {
    /// The pool the event routed to.
    pub pool: PoolKey,
    /// The decoded semantic event.
    pub event: AdapterEvent,
    /// The diff actually applied to the cache.
    pub applied: StateDiff,
    /// The combined follow-up repair (event repair + `after_apply`).
    pub repair: RepairAction,
}

/// High-level AMM event class.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AdapterEventKind {
    /// A swap (trade) event.
    Swap,
    /// Liquidity added (mint / add_liquidity).
    LiquidityAdded,
    /// Liquidity removed (burn / remove_liquidity).
    LiquidityRemoved,
    /// A reserves-sync event carrying absolute state (Uniswap V2 / Solidly).
    Sync,
    /// A deposit into a vault-style pool.
    Deposit,
    /// A withdrawal from a vault-style pool.
    Withdraw,
    /// An event the adapter recognized but does not classify further.
    Unknown,
}

/// Result of protocol adapter log decoding.
#[non_exhaustive]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AdapterEventResult {
    /// The decoded event, if the log was recognized and well-formed.
    pub event: Option<AdapterEvent>,
    /// A structured decode error, if the log was recognized but malformed.
    pub error: Option<AdapterEventError>,
}

impl AdapterEventResult {
    /// A successful decode carrying `event`.
    pub fn event(event: AdapterEvent) -> Self {
        Self {
            event: Some(event),
            error: None,
        }
    }

    /// The log was not for this adapter/pool — neither event nor error.
    pub fn ignored() -> Self {
        Self::default()
    }

    /// A recognized-but-malformed log carrying a structured `error`.
    pub fn error(error: AdapterEventError) -> Self {
        Self {
            event: None,
            error: Some(error),
        }
    }
}

/// Decode-time adapter error vocabulary.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AdapterEventError {
    /// The log matched a watched topic but its payload could not be decoded.
    MalformedLog(&'static str),
    /// Decoding needed cached state that was absent at `address`/`slot`.
    MissingState {
        /// The contract whose slot was needed.
        address: Address,
        /// The storage slot that was needed.
        slot: U256,
    },
    /// The event or its routing is unsupported for this adapter.
    Unsupported(UnsupportedReason),
    /// A protocol-specific decode failure.
    Custom(String),
}

impl fmt::Display for AdapterEventError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MalformedLog(what) => write!(f, "malformed log: {what}"),
            Self::MissingState { address, slot } => {
                write!(f, "missing state at {address}:{slot}")
            }
            Self::Unsupported(reason) => write!(f, "unsupported: {reason:?}"),
            Self::Custom(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for AdapterEventError {}

/// Quality of the cache update emitted for an adapter event.
///
/// Deliberately exhaustive (unlike most enums in this crate): this is a closed
/// quality ladder consumers are expected to match in full — a new rung changes
/// what callers must handle and warrants a breaking release.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum UpdateQuality {
    /// The updates are exact and unconditional.
    Exact,
    /// Exact **if** applied — some updates may be skipped on cold slots, in
    /// which case a resync follows.
    ExactIfApplied,
    /// The event carries deltas; the affected slots need a repair/resync.
    RequiresRepair,
    /// State could not be updated precisely; conservatively invalidate.
    ConservativeInvalidation,
    /// The event produced no state effect.
    Ignored,
}

/// Adapter-level follow-up work after cold-start or event application.
#[non_exhaustive]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum RepairAction {
    /// No follow-up needed.
    #[default]
    None,
    /// Re-verify (resync) the listed `(address, slot)` pairs.
    VerifySlots(Vec<(Address, U256)>),
    /// Invalidate all cached storage for an address.
    PurgeStorage(Address),
    /// Invalidate specific slots of an address.
    PurgeSlots {
        /// The contract whose slots to purge.
        address: Address,
        /// The slots to purge.
        slots: Vec<U256>,
    },
    /// Re-run cold-start for a pool under `policy` (a caller-side escalation).
    ColdStart {
        /// The pool to cold-start.
        pool: PoolKey,
        /// The policy to cold-start it under.
        policy: ColdStartPolicy,
    },
    /// Resync the storage a V3 liquidity event over `[tick_lower, tick_upper]`
    /// can dirty (boundary tick info, bitmap words, global liquidity).
    V3TickRange {
        /// The V3 pool.
        pool: PoolKey,
        /// The lower boundary tick of the liquidity range.
        tick_lower: i32,
        /// The upper boundary tick of the liquidity range.
        tick_upper: i32,
    },
    /// Escalation signal: an incremental V3 re-warm is warranted (hook-only).
    V3Incremental {
        /// The V3 pool.
        pool: PoolKey,
    },
    /// Escalation signal: a full V3 re-warm is warranted (hook-only).
    V3Full {
        /// The V3 pool.
        pool: PoolKey,
    },
}

impl RepairAction {
    /// Merge two repair intentions into one, preferring `other` on conflict.
    ///
    /// `None` is absorbing (`x.combine(None) == x`, `None.combine(x) == x`),
    /// matching same-shape variants are unioned (`VerifySlots` by slot,
    /// same-address `PurgeSlots` by slot), and any other pairing falls through
    /// to `other`.
    pub(crate) fn combine(self, other: RepairAction) -> RepairAction {
        match (self, other) {
            (RepairAction::None, repair) | (repair, RepairAction::None) => repair,
            (RepairAction::VerifySlots(mut left), RepairAction::VerifySlots(right)) => {
                for slot in right {
                    if !left.contains(&slot) {
                        left.push(slot);
                    }
                }
                RepairAction::VerifySlots(left)
            }
            (
                RepairAction::PurgeSlots {
                    address: left_address,
                    slots: mut left_slots,
                },
                RepairAction::PurgeSlots {
                    address: right_address,
                    slots: right_slots,
                },
            ) if left_address == right_address => {
                for slot in right_slots {
                    if !left_slots.contains(&slot) {
                        left_slots.push(slot);
                    }
                }
                RepairAction::PurgeSlots {
                    address: left_address,
                    slots: left_slots,
                }
            }
            (_, other) => other,
        }
    }
}

/// Cold-start strictness and cost policy.
///
/// Deliberately exhaustive (unlike most enums in this crate): every planner
/// must define behavior for every policy, so a new policy is a semantic
/// change to all adapters and warrants a breaking release.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ColdStartPolicy {
    /// Warm the full read-set. Currently identical to `Eager` (no adapter
    /// branches the two); reserved as a distinct policy for stricter future
    /// miss handling.
    Strict,
    /// Warm the full read-set — the common default.
    Eager,
    /// Warm only the hot slots now and defer the rest as [`DeferredWork`].
    Lazy,
    /// Warm only the minimal hot slots (e.g. slot0 + liquidity), no tick warming.
    HotSlotsOnly,
}

/// Result of attempting to cold-start a tracked pool.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ColdStartOutcome {
    /// Fully warmed and ready to simulate.
    Ready(ColdStartReport),
    /// Warmed enough to be ready, with `DeferredWork` left to run later (`Lazy`).
    ReadyWithDeferred(ColdStartReport, Vec<DeferredWork>),
    /// Warmed but a mandatory slot needs repair (e.g. an archive miss).
    NeedsRepair(ColdStartReport, RepairAction),
    /// The pool/protocol/layout is not supported.
    Unsupported(UnsupportedReason),
}

/// Inspectable summary of cold-start work performed.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColdStartReport {
    /// The pool this report is for.
    pub pool: PoolKey,
    /// The policy the cold-start ran under.
    pub policy: ColdStartPolicy,
    /// The pool's resulting status.
    pub status: PoolStatus,
    /// Every slot the run requested be verified.
    pub verified_slots: Vec<(Address, U256)>,
    /// The slots whose value changed and were injected.
    pub changed_slots: Vec<SlotChange>,
    /// The diff applied to the cache during the run.
    pub applied: StateDiff,
    /// Deferred work produced by a `Lazy` run (empty otherwise).
    pub deferred: Vec<DeferredWork>,
    /// Verified-code-seed results, when seeding ran for this cold-start (an
    /// account-fields fetcher was present, seeding was enabled, and the adapter
    /// produced at least one seed). `None` when no seeding was attempted.
    pub code_seeds: Option<crate::adapters::cold_start::CodeSeedReport>,
}

impl ColdStartReport {
    /// An empty report for `pool` under `policy` (status [`PoolStatus::Pending`]).
    pub fn new(pool: PoolKey, policy: ColdStartPolicy) -> Self {
        Self {
            pool,
            policy,
            status: PoolStatus::Pending,
            verified_slots: Vec::new(),
            changed_slots: Vec::new(),
            applied: StateDiff::default(),
            deferred: Vec::new(),
            code_seeds: None,
        }
    }
}

/// Deferred adapter work that can be scheduled after cold-start.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeferredWork {
    /// Warm (verify) these `(address, slot)` pairs when the consumer is ready.
    VerifySlots(Vec<(Address, U256)>),
    /// A repair action deferred for later execution.
    Repair(RepairAction),
    /// Re-cold-start a pool under `policy`, deferred to the caller.
    ColdStart {
        /// The pool to cold-start.
        pool: PoolKey,
        /// The policy to cold-start it under.
        policy: ColdStartPolicy,
    },
    /// Protocol-specific deferred work, described by a string tag.
    Custom(String),
}

/// Result of running deferred cold-start work via
/// [`AdapterRegistry::run_deferred`](super::AdapterRegistry::run_deferred).
///
/// `verified` accumulates the [`SlotChange`]s produced by warming
/// [`DeferredWork::VerifySlots`] (and `Repair(VerifySlots)`) entries.
/// `unhandled` collects, verbatim, any deferred work the driver does not execute
/// in this item (`ColdStart`, `Custom`, and non-`VerifySlots` repairs) so callers
/// can route them onward rather than have them silently dropped.
#[non_exhaustive]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeferredOutcome {
    /// Slot changes produced by warming the handled `VerifySlots` work.
    pub verified: Vec<SlotChange>,
    /// Deferred work this driver did not execute (pushed on verbatim).
    pub unhandled: Vec<DeferredWork>,
}

impl DeferredOutcome {
    /// Whether every deferred item was executed (nothing was deferred onward).
    pub fn is_fully_handled(&self) -> bool {
        self.unhandled.is_empty()
    }
}

/// Why a protocol state, event, or policy is not supported by the current adapter.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UnsupportedReason {
    /// No adapter is registered / implemented for this protocol.
    Protocol(ProtocolId),
    /// Required metadata (e.g. a storage layout) is missing.
    MissingMetadata(&'static str),
    /// The event uses adapter-defined routing that this path cannot resolve.
    AdapterDefinedRouting,
    /// A protocol-specific unsupported reason.
    Custom(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(byte: u8) -> Address {
        Address::repeat_byte(byte)
    }

    #[test]
    fn combine_none_is_absorbing() {
        let verify = RepairAction::VerifySlots(vec![(addr(0x11), U256::from(1))]);
        assert_eq!(RepairAction::None.combine(verify.clone()), verify);
        assert_eq!(verify.clone().combine(RepairAction::None), verify);
        assert_eq!(
            RepairAction::None.combine(RepairAction::None),
            RepairAction::None
        );
    }

    #[test]
    fn combine_verify_slots_unions_and_dedupes() {
        let a = addr(0x11);
        let left = RepairAction::VerifySlots(vec![(a, U256::from(1)), (a, U256::from(2))]);
        let right = RepairAction::VerifySlots(vec![(a, U256::from(2)), (a, U256::from(3))]);
        assert_eq!(
            left.combine(right),
            RepairAction::VerifySlots(vec![
                (a, U256::from(1)),
                (a, U256::from(2)),
                (a, U256::from(3)),
            ])
        );
    }

    #[test]
    fn combine_purge_slots_same_address_unions() {
        let a = addr(0x22);
        let left = RepairAction::PurgeSlots {
            address: a,
            slots: vec![U256::from(1), U256::from(2)],
        };
        let right = RepairAction::PurgeSlots {
            address: a,
            slots: vec![U256::from(2), U256::from(3)],
        };
        assert_eq!(
            left.combine(right),
            RepairAction::PurgeSlots {
                address: a,
                slots: vec![U256::from(1), U256::from(2), U256::from(3)],
            }
        );
    }

    #[test]
    fn combine_purge_slots_different_address_prefers_other() {
        let left = RepairAction::PurgeSlots {
            address: addr(0x22),
            slots: vec![U256::from(1)],
        };
        let right = RepairAction::PurgeSlots {
            address: addr(0x33),
            slots: vec![U256::from(9)],
        };
        assert_eq!(left.combine(right.clone()), right);
    }

    #[test]
    fn combine_fallthrough_prefers_other() {
        let left = RepairAction::VerifySlots(vec![(addr(0x11), U256::from(1))]);
        let right = RepairAction::PurgeStorage(addr(0x44));
        assert_eq!(left.combine(right.clone()), right);
    }
}
