//! Factory-backed pool discovery.
//!
//! Discovery is intentionally read-only: factory drivers produce
//! [`PoolRegistration`] values, and callers still decide when to cold-start and
//! register them.

use std::fmt;

#[cfg(any(feature = "uniswap-v2", feature = "uniswap-v3"))]
use std::collections::HashMap;

use alloy_primitives::{Address, Log};
#[cfg(any(feature = "uniswap-v2", feature = "uniswap-v3"))]
use alloy_primitives::{B256, U256};
#[cfg(any(feature = "uniswap-v2", feature = "uniswap-v3"))]
use alloy_sol_types::{SolEvent, sol};

#[cfg(any(feature = "uniswap-v2", feature = "uniswap-v3"))]
use super::ProtocolMetadata;
#[cfg(feature = "uniswap-v2")]
use super::UniswapV2Metadata;
#[cfg(feature = "uniswap-v3")]
use super::V3Metadata;
use super::{AdapterCache, AdapterRegistry, EventSource, PoolKey, PoolRegistration, ProtocolId};
#[cfg(feature = "uniswap-v3")]
use crate::adapters::storage::V3StorageLayout;

/// Factory-level derivation helpers.
pub mod derive {
    use alloy_primitives::Address;
    #[cfg(any(feature = "uniswap-v2", feature = "uniswap-v3"))]
    use alloy_primitives::{B256, U256, keccak256};

    /// Sort two token addresses into the canonical `(token0, token1)` order used
    /// by Uniswap-style factories.
    pub fn sort_tokens(a: Address, b: Address) -> (Address, Address) {
        if a.as_slice() <= b.as_slice() {
            (a, b)
        } else {
            (b, a)
        }
    }

    /// Every unordered token pair drawn from `tokens`, each normalized to
    /// `(token0, token1)` order and de-duplicated. For `n` distinct tokens this
    /// is the `C(n, 2)` combinations — the pair set a token-basket query
    /// expands into (see [`PoolDiscovery::find_pairs_among`]).
    ///
    /// [`PoolDiscovery::find_pairs_among`]: super::PoolDiscovery::find_pairs_among
    #[cfg(any(feature = "uniswap-v2", feature = "uniswap-v3"))]
    pub fn pairs_among(tokens: &[Address]) -> Vec<(Address, Address)> {
        let mut pairs = Vec::new();
        for (i, &a) in tokens.iter().enumerate() {
            for &b in &tokens[i + 1..] {
                if a != b {
                    pairs.push(sort_tokens(a, b));
                }
            }
        }
        pairs.sort_unstable();
        pairs.dedup();
        pairs
    }

    /// Compute the storage key for `UniswapV2Factory.getPair[token0][token1]`.
    #[cfg(feature = "uniswap-v2")]
    pub fn v2_get_pair_slot(base_slot: U256, token0: Address, token1: Address) -> U256 {
        nested_address_mapping_slot(base_slot, token0, token1)
    }

    /// Compute the storage key for `UniswapV3Factory.getPool[token0][token1][fee]`.
    #[cfg(feature = "uniswap-v3")]
    pub fn v3_get_pool_slot(base_slot: U256, token0: Address, token1: Address, fee: u32) -> U256 {
        let first = mapping_slot_address(base_slot, token0);
        let second = mapping_slot_address(first, token1);
        mapping_slot_u256(second, U256::from(fee))
    }

    /// Compute the storage key for `UniswapV3Factory.feeAmountTickSpacing[fee]`.
    #[cfg(feature = "uniswap-v3")]
    pub fn v3_fee_amount_tick_spacing_slot(base_slot: U256, fee: u32) -> U256 {
        mapping_slot_u256(base_slot, U256::from(fee))
    }

    /// Compute a Uniswap V2 pair address using the factory CREATE2 formula.
    #[cfg(feature = "uniswap-v2")]
    pub fn v2_pair_address(
        factory: Address,
        init_code_hash: B256,
        token0: Address,
        token1: Address,
    ) -> Address {
        let mut salt_preimage = [0u8; 40];
        salt_preimage[..20].copy_from_slice(token0.as_slice());
        salt_preimage[20..].copy_from_slice(token1.as_slice());
        create2_address(factory, keccak256(salt_preimage), init_code_hash)
    }

    /// Compute a Uniswap V3 pool address using the canonical `PoolAddress` salt.
    #[cfg(feature = "uniswap-v3")]
    pub fn v3_pool_address(
        deployer: Address,
        init_code_hash: B256,
        token0: Address,
        token1: Address,
        fee: u32,
    ) -> Address {
        let mut salt_preimage = [0u8; 96];
        salt_preimage[12..32].copy_from_slice(token0.as_slice());
        salt_preimage[44..64].copy_from_slice(token1.as_slice());
        salt_preimage[92..96].copy_from_slice(&fee.to_be_bytes());
        create2_address(deployer, keccak256(salt_preimage), init_code_hash)
    }

    #[cfg(feature = "uniswap-v2")]
    fn nested_address_mapping_slot(base_slot: U256, outer: Address, inner: Address) -> U256 {
        let outer_slot = mapping_slot_address(base_slot, outer);
        mapping_slot_address(outer_slot, inner)
    }

    #[cfg(any(feature = "uniswap-v2", feature = "uniswap-v3"))]
    fn mapping_slot_address(base_slot: U256, key: Address) -> U256 {
        let mut preimage = [0u8; 64];
        preimage[12..32].copy_from_slice(key.as_slice());
        preimage[32..64].copy_from_slice(&base_slot.to_be_bytes::<32>());
        keccak256(preimage).into()
    }

    #[cfg(feature = "uniswap-v3")]
    fn mapping_slot_u256(base_slot: U256, key: U256) -> U256 {
        let mut preimage = [0u8; 64];
        preimage[..32].copy_from_slice(&key.to_be_bytes::<32>());
        preimage[32..64].copy_from_slice(&base_slot.to_be_bytes::<32>());
        keccak256(preimage).into()
    }

    #[cfg(any(feature = "uniswap-v2", feature = "uniswap-v3"))]
    fn create2_address(deployer: Address, salt: B256, init_code_hash: B256) -> Address {
        let mut preimage = [0u8; 85];
        preimage[0] = 0xff;
        preimage[1..21].copy_from_slice(deployer.as_slice());
        preimage[21..53].copy_from_slice(salt.as_slice());
        preimage[53..85].copy_from_slice(init_code_hash.as_slice());
        let hash = keccak256(preimage);
        Address::from_slice(&hash.as_slice()[12..])
    }
}

#[cfg(feature = "uniswap-v2")]
const UNISWAP_V2_GET_PAIR_BASE_SLOT: U256 = U256::from_limbs([2, 0, 0, 0]);
#[cfg(feature = "uniswap-v3")]
const UNISWAP_V3_FEE_AMOUNT_TICK_SPACING_BASE_SLOT: U256 = U256::from_limbs([4, 0, 0, 0]);
#[cfg(feature = "uniswap-v3")]
const UNISWAP_V3_GET_POOL_BASE_SLOT: U256 = U256::from_limbs([5, 0, 0, 0]);
#[cfg(feature = "uniswap-v3")]
const UNISWAP_V3_CANONICAL_FEE_TIERS: [u32; 4] = [100, 500, 3_000, 10_000];

#[cfg(feature = "uniswap-v2")]
sol! {
    event PairCreated(address indexed token0, address indexed token1, address pair, uint256 allPairsLength);
}

#[cfg(feature = "uniswap-v3")]
sol! {
    event PoolCreated(address indexed token0, address indexed token1, uint24 indexed fee, int24 tickSpacing, address pool);
}

/// What to resolve from a protocol factory.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PoolQuery {
    pub tokens: (Address, Address),
}

impl PoolQuery {
    /// Build a pair query, normalizing token order.
    pub fn pair(a: Address, b: Address) -> Self {
        Self {
            tokens: derive::sort_tokens(a, b),
        }
    }
}

/// Uniswap V3-specific pool query.
#[cfg(feature = "uniswap-v3")]
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UniswapV3PoolQuery {
    pub tokens: (Address, Address),
    pub variant: UniswapV3PoolVariant,
}

#[cfg(feature = "uniswap-v3")]
impl UniswapV3PoolQuery {
    /// Build a V3 pair query, normalizing token order and resolving all
    /// configured fee tiers by default.
    pub fn pair(a: Address, b: Address) -> Self {
        Self {
            tokens: derive::sort_tokens(a, b),
            variant: UniswapV3PoolVariant::Any,
        }
    }

    pub fn with_fee_tier(mut self, fee: u32) -> Self {
        self.variant = UniswapV3PoolVariant::FeeTier(fee);
        self
    }
}

#[cfg(feature = "uniswap-v3")]
impl From<PoolQuery> for UniswapV3PoolQuery {
    fn from(query: PoolQuery) -> Self {
        Self {
            tokens: query.tokens,
            variant: UniswapV3PoolVariant::Any,
        }
    }
}

/// Uniswap V3-specific variant selector.
#[cfg(feature = "uniswap-v3")]
#[non_exhaustive]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum UniswapV3PoolVariant {
    #[default]
    Any,
    FeeTier(u32),
}

#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FactoryQuery {
    Pair(PoolQuery),
    #[cfg(feature = "uniswap-v3")]
    UniswapV3(UniswapV3PoolQuery),
}

impl From<PoolQuery> for FactoryQuery {
    fn from(query: PoolQuery) -> Self {
        Self::Pair(query)
    }
}

#[cfg(feature = "uniswap-v3")]
impl From<UniswapV3PoolQuery> for FactoryQuery {
    fn from(query: UniswapV3PoolQuery) -> Self {
        Self::UniswapV3(query)
    }
}

/// Factory configuration. Defaults are intentionally empty; callers must opt in
/// to concrete factory addresses or explicit presets. Each protocol accepts
/// multiple factory configs (e.g. Uniswap plus a same-protocol fork), which are
/// resolved in insertion order.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FactoryConfig {
    #[cfg(feature = "uniswap-v2")]
    pub uniswap_v2: Vec<UniswapV2FactoryConfig>,
    #[cfg(feature = "uniswap-v3")]
    pub uniswap_v3: Vec<UniswapV3FactoryConfig>,
    pub verify_derivations: bool,
}

impl Default for FactoryConfig {
    fn default() -> Self {
        Self {
            #[cfg(feature = "uniswap-v2")]
            uniswap_v2: Vec::new(),
            #[cfg(feature = "uniswap-v3")]
            uniswap_v3: Vec::new(),
            verify_derivations: true,
        }
    }
}

impl FactoryConfig {
    #[cfg(feature = "uniswap-v2")]
    pub fn with_uniswap_v2(mut self, config: UniswapV2FactoryConfig) -> Self {
        self.uniswap_v2.push(config);
        self
    }

    #[cfg(feature = "uniswap-v2")]
    pub fn with_uniswap_v2_factory(self, factory: Address) -> Self {
        self.with_uniswap_v2(UniswapV2FactoryConfig::uniswap_v2(factory))
    }

    #[cfg(feature = "uniswap-v3")]
    pub fn with_uniswap_v3(mut self, config: UniswapV3FactoryConfig) -> Self {
        self.uniswap_v3.push(config);
        self
    }

    #[cfg(feature = "uniswap-v3")]
    pub fn with_uniswap_v3_factory(self, factory: Address) -> Self {
        self.with_uniswap_v3(UniswapV3FactoryConfig::uniswap_v3(factory))
    }

    pub fn with_verify_derivations(mut self, verify_derivations: bool) -> Self {
        self.verify_derivations = verify_derivations;
        self
    }
}

#[cfg(feature = "uniswap-v2")]
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UniswapV2FactoryConfig {
    pub factory: Address,
    pub get_pair_base_slot: U256,
    pub init_code_hash: Option<B256>,
    pub fee_bps: Option<u32>,
}

#[cfg(feature = "uniswap-v2")]
impl UniswapV2FactoryConfig {
    pub fn uniswap_v2(factory: Address) -> Self {
        Self {
            factory,
            get_pair_base_slot: UNISWAP_V2_GET_PAIR_BASE_SLOT,
            init_code_hash: None,
            fee_bps: None,
        }
    }

    pub fn with_get_pair_base_slot(mut self, slot: U256) -> Self {
        self.get_pair_base_slot = slot;
        self
    }

    pub fn with_init_code_hash(mut self, hash: B256) -> Self {
        self.init_code_hash = Some(hash);
        self
    }

    pub fn with_fee_bps(mut self, fee_bps: u32) -> Self {
        self.fee_bps = Some(fee_bps);
        self
    }
}

#[cfg(feature = "uniswap-v3")]
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UniswapV3FactoryConfig {
    pub factory: Address,
    pub get_pool_base_slot: U256,
    pub fee_amount_tick_spacing_base_slot: U256,
    pub init_code_hash: Option<B256>,
    pub fee_tiers: Vec<u32>,
}

#[cfg(feature = "uniswap-v3")]
impl UniswapV3FactoryConfig {
    pub fn uniswap_v3(factory: Address) -> Self {
        Self {
            factory,
            get_pool_base_slot: UNISWAP_V3_GET_POOL_BASE_SLOT,
            fee_amount_tick_spacing_base_slot: UNISWAP_V3_FEE_AMOUNT_TICK_SPACING_BASE_SLOT,
            init_code_hash: None,
            fee_tiers: UNISWAP_V3_CANONICAL_FEE_TIERS.to_vec(),
        }
    }

    pub fn with_get_pool_base_slot(mut self, slot: U256) -> Self {
        self.get_pool_base_slot = slot;
        self
    }

    pub fn with_fee_amount_tick_spacing_base_slot(mut self, slot: U256) -> Self {
        self.fee_amount_tick_spacing_base_slot = slot;
        self
    }

    pub fn with_init_code_hash(mut self, hash: B256) -> Self {
        self.init_code_hash = Some(hash);
        self
    }

    pub fn with_fee_tiers(mut self, fee_tiers: impl IntoIterator<Item = u32>) -> Self {
        self.fee_tiers = fee_tiers.into_iter().collect();
        self
    }
}

/// Source of a discovered pool.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DiscoverySource {
    Query,
    CreationEvent {
        block_number: Option<u64>,
        log_index: Option<u64>,
    },
}

/// Context supplied alongside a factory creation log.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CreationLogContext {
    pub block_number: Option<u64>,
    pub log_index: Option<u64>,
}

impl CreationLogContext {
    pub const fn new(block_number: Option<u64>, log_index: Option<u64>) -> Self {
        Self {
            block_number,
            log_index,
        }
    }
}

/// A pool located by factory query or creation log.
///
/// `#[non_exhaustive]` (matching the rest of the discovery vocabulary): external
/// [`PoolFactory`] implementations construct this via [`DiscoveredPool::new`]
/// (the open-channel `with_factory` path), so new fields can be added without a
/// breaking change.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct DiscoveredPool {
    pub key: PoolKey,
    pub registration: PoolRegistration,
    pub source: DiscoverySource,
}

impl DiscoveredPool {
    /// Assemble a discovered pool from its key, registration, and provenance.
    pub fn new(key: PoolKey, registration: PoolRegistration, source: DiscoverySource) -> Self {
        Self {
            key,
            registration,
            source,
        }
    }
}

/// Errors produced by factory-backed discovery.
#[derive(Debug)]
#[non_exhaustive]
pub enum DiscoveryError {
    MissingFactory(ProtocolId),
    UnsupportedQuery(&'static str),
    Factory(Box<dyn std::error::Error + Send + Sync + 'static>),
    Malformed(&'static str),
    DerivationMismatch { mapping: Address, derived: Address },
}

impl fmt::Display for DiscoveryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingFactory(protocol) => write!(f, "missing factory for {protocol:?}"),
            Self::UnsupportedQuery(message) => write!(f, "unsupported factory query: {message}"),
            Self::Factory(err) => write!(f, "factory query failed: {err}"),
            Self::Malformed(message) => write!(f, "malformed factory response: {message}"),
            Self::DerivationMismatch { mapping, derived } => write!(
                f,
                "factory mapping answer {mapping:?} disagrees with CREATE2 derivation {derived:?}"
            ),
        }
    }
}

impl std::error::Error for DiscoveryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Factory(err) => Some(&**err as &(dyn std::error::Error + 'static)),
            _ => None,
        }
    }
}

impl From<super::CacheError> for DiscoveryError {
    fn from(err: super::CacheError) -> Self {
        Self::Factory(Box::new(err))
    }
}

/// Per-protocol factory driver.
pub trait PoolFactory: Send + Sync {
    fn protocol(&self) -> ProtocolId;

    /// Address of the factory contract this driver resolves against. Used
    /// together with [`PoolFactory::protocol`] as the identity for
    /// de-duplication, so distinct addresses of the same protocol are all kept.
    fn factory_address(&self) -> Address;

    fn find_pools(
        &self,
        cache: &mut dyn AdapterCache,
        query: &FactoryQuery,
    ) -> Result<Vec<DiscoveredPool>, DiscoveryError>;

    /// The storage reads (`(factory_address, slot)`) this factory needs to
    /// resolve every pair in `pairs`. Returning them instead of executing lets
    /// [`PoolDiscovery::find_pairs_among`] gather the candidate slots of *all*
    /// pairs across *all* factories and resolve them in a single batched
    /// [`AdapterCache::read_storage_slots`] call.
    ///
    /// Defaults to none: a factory that does not implement batched multi-pair
    /// discovery simply contributes no pools to a basket query.
    #[cfg(any(feature = "uniswap-v2", feature = "uniswap-v3"))]
    fn candidate_reads(&self, pairs: &[(Address, Address)]) -> Vec<(Address, U256)> {
        let _ = pairs;
        Vec::new()
    }

    /// Assemble discovered pools from already-resolved candidate reads
    /// (`values`, keyed by `(factory_address, slot)`) — the batched counterpart
    /// to [`find_pools`](Self::find_pools). Must read only slots this factory
    /// previously returned from [`candidate_reads`](Self::candidate_reads).
    #[cfg(any(feature = "uniswap-v2", feature = "uniswap-v3"))]
    fn assemble_pairs(
        &self,
        pairs: &[(Address, Address)],
        values: &HashMap<(Address, U256), U256>,
    ) -> Result<Vec<DiscoveredPool>, DiscoveryError> {
        let _ = (pairs, values);
        Ok(Vec::new())
    }

    fn creation_sources(&self) -> Vec<EventSource>;

    fn decode_creation(
        &self,
        log: &Log,
        context: CreationLogContext,
    ) -> Result<Option<DiscoveredPool>, DiscoveryError>;
}

/// Front-end that fans discovery across registered adapter factory drivers.
///
/// Factories are held in insertion order, so iteration (and thus the order of
/// concatenated results) is deterministic. Multiple factories may share a
/// [`ProtocolId`] — e.g. Uniswap and a same-protocol fork — and every matching
/// factory is consulted. Identity is `(protocol, factory_address)`; exact
/// duplicates are dropped (first wins) so a repeated factory does not run twice.
#[derive(Default)]
pub struct PoolDiscovery {
    factories: Vec<Box<dyn PoolFactory>>,
}

impl PoolDiscovery {
    pub fn new(factories: impl IntoIterator<Item = Box<dyn PoolFactory>>) -> Self {
        let mut discovery = Self {
            factories: Vec::new(),
        };
        for factory in factories {
            discovery.push_unique(factory);
        }
        discovery
    }

    pub fn for_registry(registry: &AdapterRegistry, config: FactoryConfig) -> Self {
        let mut factories = Vec::new();
        let mut seen = Vec::new();
        for adapter in registry.adapters() {
            let ptr = std::sync::Arc::as_ptr(adapter);
            if seen.contains(&ptr) {
                continue;
            }
            seen.push(ptr);
            factories.extend(adapter.pool_factories(&config));
        }
        Self::new(factories)
    }

    /// Open channel for extending discovery with an externally-implemented
    /// [`PoolFactory`]. Appends the factory, applying the same
    /// `(protocol, factory_address)` de-duplication as [`PoolDiscovery::new`].
    pub fn with_factory(mut self, factory: Box<dyn PoolFactory>) -> Self {
        self.push_unique(factory);
        self
    }

    fn push_unique(&mut self, factory: Box<dyn PoolFactory>) {
        let identity = (factory.protocol(), factory.factory_address());
        if self
            .factories
            .iter()
            .any(|existing| (existing.protocol(), existing.factory_address()) == identity)
        {
            return;
        }
        self.factories.push(factory);
    }

    pub fn find(
        &self,
        cache: &mut dyn AdapterCache,
        protocol: ProtocolId,
        query: PoolQuery,
    ) -> Result<Vec<DiscoveredPool>, DiscoveryError> {
        let query = FactoryQuery::from(query);
        let mut matched = false;
        let mut found = Vec::new();
        for factory in &self.factories {
            if factory.protocol() != protocol {
                continue;
            }
            matched = true;
            found.extend(factory.find_pools(cache, &query)?);
        }
        if !matched {
            return Err(DiscoveryError::MissingFactory(protocol));
        }
        Ok(found)
    }

    #[cfg(feature = "uniswap-v3")]
    pub fn find_uniswap_v3(
        &self,
        cache: &mut dyn AdapterCache,
        query: UniswapV3PoolQuery,
    ) -> Result<Vec<DiscoveredPool>, DiscoveryError> {
        let query = FactoryQuery::from(query);
        let mut matched = false;
        let mut found = Vec::new();
        for factory in &self.factories {
            if factory.protocol() != ProtocolId::UniswapV3 {
                continue;
            }
            matched = true;
            found.extend(factory.find_pools(cache, &query)?);
        }
        if !matched {
            return Err(DiscoveryError::MissingFactory(ProtocolId::UniswapV3));
        }
        Ok(found)
    }

    pub fn find_all(
        &self,
        cache: &mut dyn AdapterCache,
        query: PoolQuery,
    ) -> Result<Vec<DiscoveredPool>, DiscoveryError> {
        let mut found = Vec::new();
        let query = FactoryQuery::Pair(query);
        for factory in &self.factories {
            found.extend(factory.find_pools(cache, &query)?);
        }
        Ok(found)
    }

    /// Discover **every** pool joining any pair of `tokens`, across all
    /// registered factories, in a single batched storage read.
    ///
    /// This is the declarative "give me all pools among this token basket"
    /// query. It expands `tokens` into all `C(n, 2)` pairs (see
    /// [`derive::pairs_among`]), collects the candidate mapping-slot reads of
    /// every factory for every pair (for Uniswap V3, every configured fee tier
    /// plus the shared `feeAmountTickSpacing` slots), and resolves them all with
    /// ONE [`AdapterCache::read_storage_slots`] call — which, on an `EvmCache`,
    /// is a single bulk `eth_call`. Request count scales with the number of
    /// factories, not with pairs, fee tiers, or basket size.
    ///
    /// Only factories that implement [`PoolFactory::candidate_reads`] /
    /// [`assemble_pairs`](PoolFactory::assemble_pairs) participate (the built-in
    /// Uniswap V2 and V3 factories do); others contribute nothing here — use
    /// [`find`](Self::find) for those.
    #[cfg(any(feature = "uniswap-v2", feature = "uniswap-v3"))]
    pub fn find_pairs_among(
        &self,
        cache: &mut dyn AdapterCache,
        tokens: &[Address],
    ) -> Result<Vec<DiscoveredPool>, DiscoveryError> {
        let pairs = derive::pairs_among(tokens);
        if pairs.is_empty() || self.factories.is_empty() {
            return Ok(Vec::new());
        }

        // Gather every factory's candidate slots, de-duplicate, and resolve the
        // whole set in one batched read.
        let mut slots: Vec<(Address, U256)> = Vec::new();
        for factory in &self.factories {
            slots.extend(factory.candidate_reads(&pairs));
        }
        slots.sort_unstable();
        slots.dedup();
        if slots.is_empty() {
            return Ok(Vec::new());
        }

        let values = cache.read_storage_slots(&slots)?;
        let resolved: HashMap<(Address, U256), U256> = slots.into_iter().zip(values).collect();

        let mut found = Vec::new();
        for factory in &self.factories {
            found.extend(factory.assemble_pairs(&pairs, &resolved)?);
        }
        Ok(found)
    }

    pub fn creation_sources(&self) -> Vec<EventSource> {
        self.factories
            .iter()
            .flat_map(|factory| factory.creation_sources())
            .collect()
    }

    pub fn decode_creation(
        &self,
        log: &Log,
        context: CreationLogContext,
    ) -> Result<Option<DiscoveredPool>, DiscoveryError> {
        for factory in &self.factories {
            if let Some(pool) = factory.decode_creation(log, context)? {
                return Ok(Some(pool));
            }
        }
        Ok(None)
    }
}

#[cfg(feature = "uniswap-v2")]
#[derive(Debug)]
pub(crate) struct UniswapV2Factory {
    config: UniswapV2FactoryConfig,
    verify_derivations: bool,
    derivation_verified: std::sync::OnceLock<()>,
}

#[cfg(feature = "uniswap-v2")]
impl UniswapV2Factory {
    pub(crate) fn new(config: UniswapV2FactoryConfig, verify_derivations: bool) -> Self {
        Self {
            config,
            verify_derivations,
            derivation_verified: std::sync::OnceLock::new(),
        }
    }

    fn registration(
        &self,
        pair: Address,
        token0: Address,
        token1: Address,
        source: DiscoverySource,
    ) -> DiscoveredPool {
        let mut metadata = UniswapV2Metadata::default()
            .with_token0(token0)
            .with_token1(token1);
        if let Some(fee_bps) = self.config.fee_bps {
            metadata = metadata.with_fee_bps(fee_bps);
        }
        let registration = PoolRegistration::new(PoolKey::UniswapV2(pair))
            .with_state_address(pair)
            .with_metadata(ProtocolMetadata::UniswapV2(metadata));
        let adapter = crate::adapters::UniswapV2Adapter::default();
        let sources = super::AmmAdapter::event_sources(&adapter, &registration);
        let registration = registration.with_event_sources(sources);
        DiscoveredPool {
            key: registration.key.clone(),
            registration,
            source,
        }
    }

    fn ensure_derivation_matches(
        &self,
        mapping: Address,
        token0: Address,
        token1: Address,
    ) -> Result<(), DiscoveryError> {
        if !self.verify_derivations || self.derivation_verified.get().is_some() {
            return Ok(());
        }
        let Some(init_code_hash) = self.config.init_code_hash else {
            return Ok(());
        };
        let derived = derive::v2_pair_address(self.config.factory, init_code_hash, token0, token1);
        if derived != mapping {
            return Err(DiscoveryError::DerivationMismatch { mapping, derived });
        }
        let _ = self.derivation_verified.set(());
        Ok(())
    }
}

#[cfg(feature = "uniswap-v2")]
impl PoolFactory for UniswapV2Factory {
    fn protocol(&self) -> ProtocolId {
        ProtocolId::UniswapV2
    }

    fn factory_address(&self) -> Address {
        self.config.factory
    }

    fn find_pools(
        &self,
        cache: &mut dyn AdapterCache,
        query: &FactoryQuery,
    ) -> Result<Vec<DiscoveredPool>, DiscoveryError> {
        // With `uniswap-v3` off, `FactoryQuery` has only the `Pair` variant, so
        // the match is single-arm; allow the lint in that build only.
        #[cfg_attr(
            not(feature = "uniswap-v3"),
            allow(clippy::infallible_destructuring_match)
        )]
        let query = match query {
            FactoryQuery::Pair(query) => query,
            #[cfg(feature = "uniswap-v3")]
            FactoryQuery::UniswapV3(_) => {
                return Err(DiscoveryError::UnsupportedQuery(
                    "Uniswap V2 supports only common pair queries",
                ));
            }
        };

        let (token0, token1) = query.tokens;
        let slot = derive::v2_get_pair_slot(self.config.get_pair_base_slot, token0, token1);
        let pair = address_from_word(cache.read_storage_slot(self.config.factory, slot)?)?;
        if pair == Address::ZERO {
            return Ok(Vec::new());
        }
        self.ensure_derivation_matches(pair, token0, token1)?;
        Ok(vec![self.registration(
            pair,
            token0,
            token1,
            DiscoverySource::Query,
        )])
    }

    fn candidate_reads(&self, pairs: &[(Address, Address)]) -> Vec<(Address, U256)> {
        pairs
            .iter()
            .map(|(token0, token1)| {
                (
                    self.config.factory,
                    derive::v2_get_pair_slot(self.config.get_pair_base_slot, *token0, *token1),
                )
            })
            .collect()
    }

    fn assemble_pairs(
        &self,
        pairs: &[(Address, Address)],
        values: &HashMap<(Address, U256), U256>,
    ) -> Result<Vec<DiscoveredPool>, DiscoveryError> {
        let mut found = Vec::new();
        for (token0, token1) in pairs {
            let slot = derive::v2_get_pair_slot(self.config.get_pair_base_slot, *token0, *token1);
            let Some(&word) = values.get(&(self.config.factory, slot)) else {
                continue;
            };
            let pair = address_from_word(word)?;
            if pair == Address::ZERO {
                continue;
            }
            self.ensure_derivation_matches(pair, *token0, *token1)?;
            found.push(self.registration(pair, *token0, *token1, DiscoverySource::Query));
        }
        Ok(found)
    }

    fn creation_sources(&self) -> Vec<EventSource> {
        vec![EventSource::adapter_defined(
            self.config.factory,
            vec![PairCreated::SIGNATURE_HASH],
        )]
    }

    fn decode_creation(
        &self,
        log: &Log,
        context: CreationLogContext,
    ) -> Result<Option<DiscoveredPool>, DiscoveryError> {
        if log.address != self.config.factory
            || log.topics().first() != Some(&PairCreated::SIGNATURE_HASH)
        {
            return Ok(None);
        }
        let event = PairCreated::decode_log(log)
            .map_err(|_| DiscoveryError::Malformed("PairCreated failed to decode"))?
            .data;
        self.ensure_derivation_matches(event.pair, event.token0, event.token1)?;
        Ok(Some(self.registration(
            event.pair,
            event.token0,
            event.token1,
            DiscoverySource::CreationEvent {
                block_number: context.block_number,
                log_index: context.log_index,
            },
        )))
    }
}

#[cfg(feature = "uniswap-v3")]
#[derive(Debug)]
pub(crate) struct UniswapV3Factory {
    config: UniswapV3FactoryConfig,
    verify_derivations: bool,
    derivation_verified: std::sync::OnceLock<()>,
}

#[cfg(feature = "uniswap-v3")]
impl UniswapV3Factory {
    pub(crate) fn new(config: UniswapV3FactoryConfig, verify_derivations: bool) -> Self {
        Self {
            config,
            verify_derivations,
            derivation_verified: std::sync::OnceLock::new(),
        }
    }

    fn fees_for_query(&self, variant: &UniswapV3PoolVariant) -> Vec<u32> {
        match variant {
            UniswapV3PoolVariant::Any => self.config.fee_tiers.clone(),
            UniswapV3PoolVariant::FeeTier(fee) => vec![*fee],
        }
    }

    fn registration(
        &self,
        pool: Address,
        token0: Address,
        token1: Address,
        fee: u32,
        tick_spacing: i32,
        source: DiscoverySource,
    ) -> DiscoveredPool {
        let metadata = V3Metadata::default()
            .with_token0(token0)
            .with_token1(token1)
            .with_fee(fee)
            .with_tick_spacing(tick_spacing)
            .with_storage_layout(V3StorageLayout::uniswap(tick_spacing))
            .with_factory(self.config.factory);
        let registration = PoolRegistration::new(PoolKey::UniswapV3(pool))
            .with_state_address(pool)
            .with_metadata(ProtocolMetadata::UniswapV3(metadata));
        let adapter = crate::adapters::ConcentratedLiquidityAdapter::default();
        let sources = super::AmmAdapter::event_sources(&adapter, &registration);
        let registration = registration.with_event_sources(sources);
        DiscoveredPool {
            key: registration.key.clone(),
            registration,
            source,
        }
    }

    fn ensure_derivation_matches(
        &self,
        mapping: Address,
        token0: Address,
        token1: Address,
        fee: u32,
    ) -> Result<(), DiscoveryError> {
        if !self.verify_derivations || self.derivation_verified.get().is_some() {
            return Ok(());
        }
        let Some(init_code_hash) = self.config.init_code_hash else {
            return Ok(());
        };
        let derived =
            derive::v3_pool_address(self.config.factory, init_code_hash, token0, token1, fee);
        if derived != mapping {
            return Err(DiscoveryError::DerivationMismatch { mapping, derived });
        }
        let _ = self.derivation_verified.set(());
        Ok(())
    }
}

#[cfg(feature = "uniswap-v3")]
impl PoolFactory for UniswapV3Factory {
    fn protocol(&self) -> ProtocolId {
        ProtocolId::UniswapV3
    }

    fn factory_address(&self) -> Address {
        self.config.factory
    }

    fn find_pools(
        &self,
        cache: &mut dyn AdapterCache,
        query: &FactoryQuery,
    ) -> Result<Vec<DiscoveredPool>, DiscoveryError> {
        let query = match query {
            FactoryQuery::Pair(query) => UniswapV3PoolQuery::from(query.clone()),
            FactoryQuery::UniswapV3(query) => query.clone(),
        };
        let (token0, token1) = query.tokens;
        let fees = self.fees_for_query(&query.variant);

        // Collect every candidate slot up front — for each fee tier both its
        // `getPool[token0][token1][fee]` slot and its `feeAmountTickSpacing[fee]`
        // slot — so the whole discovery resolves in ONE batched read. Slots are
        // laid out `[pool_0, tick_0, pool_1, tick_1, ...]`, so tier `i` reads
        // indices `2*i` (pool) and `2*i + 1` (tick spacing).
        let mut slots = Vec::with_capacity(fees.len() * 2);
        for &fee in &fees {
            let pool_slot =
                derive::v3_get_pool_slot(self.config.get_pool_base_slot, token0, token1, fee);
            let tick_slot = derive::v3_fee_amount_tick_spacing_slot(
                self.config.fee_amount_tick_spacing_base_slot,
                fee,
            );
            slots.push((self.config.factory, pool_slot));
            slots.push((self.config.factory, tick_slot));
        }

        let values = cache.read_storage_slots(&slots)?;

        let mut found = Vec::new();
        for (i, &fee) in fees.iter().enumerate() {
            let pool = address_from_word(values[2 * i])?;
            if pool == Address::ZERO {
                continue;
            }
            self.ensure_derivation_matches(pool, token0, token1, fee)?;
            // `feeAmountTickSpacing[fee]` is a factory-wide fee→spacing config
            // independent of pool existence; only tiers with a non-zero pool
            // produce a registration. Mirror `read_tick_spacing`'s validation.
            let tick_spacing = i24_from_word(values[2 * i + 1]);
            if tick_spacing <= 0 {
                return Err(DiscoveryError::Malformed(
                    "V3 feeAmountTickSpacing returned a non-positive spacing",
                ));
            }
            found.push(self.registration(
                pool,
                token0,
                token1,
                fee,
                tick_spacing,
                DiscoverySource::Query,
            ));
        }
        Ok(found)
    }

    fn candidate_reads(&self, pairs: &[(Address, Address)]) -> Vec<(Address, U256)> {
        let mut slots = Vec::with_capacity(self.config.fee_tiers.len() * (pairs.len() + 1));
        // `feeAmountTickSpacing[fee]` is per-fee, not per-pair — read each once.
        for &fee in &self.config.fee_tiers {
            slots.push((
                self.config.factory,
                derive::v3_fee_amount_tick_spacing_slot(
                    self.config.fee_amount_tick_spacing_base_slot,
                    fee,
                ),
            ));
        }
        for (token0, token1) in pairs {
            for &fee in &self.config.fee_tiers {
                slots.push((
                    self.config.factory,
                    derive::v3_get_pool_slot(self.config.get_pool_base_slot, *token0, *token1, fee),
                ));
            }
        }
        slots
    }

    fn assemble_pairs(
        &self,
        pairs: &[(Address, Address)],
        values: &HashMap<(Address, U256), U256>,
    ) -> Result<Vec<DiscoveredPool>, DiscoveryError> {
        let mut found = Vec::new();
        for (token0, token1) in pairs {
            for &fee in &self.config.fee_tiers {
                let pool_slot =
                    derive::v3_get_pool_slot(self.config.get_pool_base_slot, *token0, *token1, fee);
                let Some(&word) = values.get(&(self.config.factory, pool_slot)) else {
                    continue;
                };
                let pool = address_from_word(word)?;
                if pool == Address::ZERO {
                    continue;
                }
                self.ensure_derivation_matches(pool, *token0, *token1, fee)?;
                let tick_slot = derive::v3_fee_amount_tick_spacing_slot(
                    self.config.fee_amount_tick_spacing_base_slot,
                    fee,
                );
                let tick_spacing =
                    i24_from_word(values.get(&(self.config.factory, tick_slot)).copied().unwrap_or_default());
                if tick_spacing <= 0 {
                    return Err(DiscoveryError::Malformed(
                        "V3 feeAmountTickSpacing returned a non-positive spacing",
                    ));
                }
                found.push(self.registration(
                    pool,
                    *token0,
                    *token1,
                    fee,
                    tick_spacing,
                    DiscoverySource::Query,
                ));
            }
        }
        Ok(found)
    }

    fn creation_sources(&self) -> Vec<EventSource> {
        vec![EventSource::adapter_defined(
            self.config.factory,
            vec![PoolCreated::SIGNATURE_HASH],
        )]
    }

    fn decode_creation(
        &self,
        log: &Log,
        context: CreationLogContext,
    ) -> Result<Option<DiscoveredPool>, DiscoveryError> {
        if log.address != self.config.factory
            || log.topics().first() != Some(&PoolCreated::SIGNATURE_HASH)
        {
            return Ok(None);
        }
        let event = PoolCreated::decode_log(log)
            .map_err(|_| DiscoveryError::Malformed("PoolCreated failed to decode"))?
            .data;
        let fee: u32 = event.fee.to();
        let tick_spacing: i32 = event.tickSpacing.as_i32();
        self.ensure_derivation_matches(event.pool, event.token0, event.token1, fee)?;
        Ok(Some(self.registration(
            event.pool,
            event.token0,
            event.token1,
            fee,
            tick_spacing,
            DiscoverySource::CreationEvent {
                block_number: context.block_number,
                log_index: context.log_index,
            },
        )))
    }
}

#[cfg(any(feature = "uniswap-v2", feature = "uniswap-v3"))]
fn address_from_word(word: U256) -> Result<Address, DiscoveryError> {
    let bytes = word.to_be_bytes::<32>();
    if bytes[..12].iter().any(|b| *b != 0) {
        return Err(DiscoveryError::Malformed(
            "factory address word has non-zero high bytes",
        ));
    }
    Ok(Address::from_slice(&bytes[12..]))
}

#[cfg(feature = "uniswap-v3")]
fn i24_from_word(word: U256) -> i32 {
    let bytes = word.to_be_bytes::<32>();
    let raw = u32::from_be_bytes([0, bytes[29], bytes[30], bytes[31]]);
    if (raw & 0x0080_0000) != 0 {
        (raw | 0xff00_0000) as i32
    } else {
        raw as i32
    }
}
