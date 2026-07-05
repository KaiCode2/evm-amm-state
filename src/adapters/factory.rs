//! Factory-backed pool discovery.
//!
//! Discovery is intentionally read-only: factory drivers produce
//! [`PoolRegistration`] values, and callers still decide when to cold-start and
//! register them.

use std::collections::HashMap;
use std::fmt;

#[cfg(any(feature = "uniswap-v2", feature = "uniswap-v3"))]
use alloy_primitives::B256;
use alloy_primitives::{Address, Log, U256};
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
    /// expands into (see [`PoolQuery::basket`]).
    ///
    /// [`PoolQuery::basket`]: super::PoolQuery::basket
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

/// A declarative pool-discovery query: which token pairs to resolve, optionally
/// scoped to a single protocol.
///
/// Construct one with [`pair`](Self::pair), [`basket`](Self::basket), or
/// [`pairs`](Self::pairs) — each normalizes token order (via
/// [`derive::sort_tokens`]) and de-duplicates — then optionally narrow it with
/// [`on`](Self::on). An unscoped query (no protocol) spans every matching
/// factory in a single batched read; `.on(p)` restricts it to protocol `p` (and
/// [`PoolDiscovery::find`] errors [`DiscoveryError::MissingFactory`] if no
/// factory is registered for `p`).
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PoolQuery {
    /// Normalized, de-duplicated `(token0, token1)` pairs to resolve.
    pairs: Vec<(Address, Address)>,
    /// The protocol to scope discovery to; `None` spans all matching factories.
    protocol: Option<ProtocolId>,
}

impl PoolQuery {
    /// A single token pair, normalized to `(token0, token1)` order.
    pub fn pair(a: Address, b: Address) -> Self {
        Self {
            pairs: vec![derive::sort_tokens(a, b)],
            protocol: None,
        }
    }

    /// Every unordered pair drawn from a token basket — the `C(n, 2)`
    /// combinations, each normalized and de-duplicated (see
    /// [`derive::pairs_among`]).
    pub fn basket(tokens: impl IntoIterator<Item = Address>) -> Self {
        let tokens: Vec<Address> = tokens.into_iter().collect();
        Self {
            pairs: derive::pairs_among(&tokens),
            protocol: None,
        }
    }

    /// An explicit set of pairs, each normalized to `(token0, token1)` order,
    /// then sorted and de-duplicated.
    pub fn pairs(pairs: impl IntoIterator<Item = (Address, Address)>) -> Self {
        let mut pairs: Vec<(Address, Address)> = pairs
            .into_iter()
            .map(|(a, b)| derive::sort_tokens(a, b))
            .collect();
        pairs.sort_unstable();
        pairs.dedup();
        Self {
            pairs,
            protocol: None,
        }
    }

    /// Scope discovery to a single protocol. Without this, the query spans every
    /// registered factory whose protocol has a matching pool.
    pub fn on(mut self, protocol: ProtocolId) -> Self {
        self.protocol = Some(protocol);
        self
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
    Factory(Box<dyn std::error::Error + Send + Sync + 'static>),
    Malformed(&'static str),
    DerivationMismatch { mapping: Address, derived: Address },
}

impl fmt::Display for DiscoveryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingFactory(protocol) => write!(f, "missing factory for {protocol:?}"),
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

    /// Resolve a single normalized `(token0, token1)` pair to its pool(s).
    ///
    /// The default derives from the batched
    /// [`candidate_reads`](Self::candidate_reads) /
    /// [`assemble_pairs`](Self::assemble_pairs) pair — so a factory that
    /// implements those participates in single-pair discovery for free.
    /// [`PoolDiscovery::find`] calls this only as a per-pair fallback for
    /// factories that opt out of batching (empty `candidate_reads`); such
    /// external factories override it directly.
    fn find_pools(
        &self,
        cache: &mut dyn AdapterCache,
        pair: (Address, Address),
    ) -> Result<Vec<DiscoveredPool>, DiscoveryError> {
        let slots = self.candidate_reads(&[pair]);
        if slots.is_empty() {
            return Ok(Vec::new());
        }
        let values = cache.read_storage_slots(&slots)?;
        let resolved = slots.into_iter().zip(values).collect();
        self.assemble_pairs(&[pair], &resolved)
    }

    /// The storage reads (`(factory_address, slot)`) this factory needs to
    /// resolve every pair in `pairs`. Returning them instead of executing lets
    /// [`PoolDiscovery::find`] gather the candidate slots of *all* pairs across
    /// *all* factories and resolve them in a single batched
    /// [`AdapterCache::read_storage_slots`] call.
    ///
    /// Defaults to none: a factory that does not implement batched multi-pair
    /// discovery simply contributes no pools to a batched query (and its
    /// [`find_pools`](Self::find_pools) override runs per pair instead).
    fn candidate_reads(&self, pairs: &[(Address, Address)]) -> Vec<(Address, U256)> {
        let _ = pairs;
        Vec::new()
    }

    /// Assemble discovered pools from already-resolved candidate reads
    /// (`values`, keyed by `(factory_address, slot)`) — the batched counterpart
    /// to [`find_pools`](Self::find_pools). Must read only slots this factory
    /// previously returned from [`candidate_reads`](Self::candidate_reads).
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

    /// Shared batched resolution core behind [`find`](Self::find).
    ///
    /// Considers only factories matching `protocol` (all factories when
    /// `protocol` is `None`) and partitions them into two groups by whether they
    /// opt into batched discovery:
    ///
    /// - *Batchable* factories (non-empty [`candidate_reads`]) contribute their
    ///   candidate slots to a single shared set. The whole set is de-duplicated
    ///   and resolved with exactly ONE [`AdapterCache::read_storage_slots`] call
    ///   — regardless of factory or pair count — then handed back to each
    ///   factory's [`assemble_pairs`] to build its pools. The built-in Uniswap V2
    ///   and V3 factories are batchable.
    /// - *Legacy* factories (default empty [`candidate_reads`], i.e. they only
    ///   implement [`find_pools`]) fall back to a per-pair [`find_pools`] call, so
    ///   externally-implemented factories keep working.
    ///
    /// [`candidate_reads`]: PoolFactory::candidate_reads
    /// [`assemble_pairs`]: PoolFactory::assemble_pairs
    /// [`find_pools`]: PoolFactory::find_pools
    /// Discover pools for several [`PoolQuery`]s at once, resolving the candidate
    /// slots of *all* of them in a single batched read and returning the
    /// de-duplicated union.
    ///
    /// Each query is scoped independently, so one call can mix protocols across
    /// pairs — some pairs only on Uniswap V2, others only on V3 — without extra
    /// round-trips. Batchable factories (the built-in V2/V3) contribute their
    /// candidate mapping slots to ONE [`AdapterCache::read_storage_slots`] call
    /// (a single bulk `eth_call` on an `EvmCache`); external factories that only
    /// implement [`find_pools`](PoolFactory::find_pools) fall back per pair. A
    /// query scoped with [`PoolQuery::on`] to a protocol with no registered
    /// factory yields [`DiscoveryError::MissingFactory`]; an empty query list is
    /// `Ok(vec![])`.
    pub fn find_many(
        &self,
        cache: &mut dyn AdapterCache,
        queries: impl IntoIterator<Item = PoolQuery>,
    ) -> Result<Vec<DiscoveredPool>, DiscoveryError> {
        let queries: Vec<PoolQuery> = queries.into_iter().collect();

        // An explicit `.on(protocol)` naming an unregistered protocol is an error.
        for query in &queries {
            if let Some(protocol) = query.protocol
                && !self
                    .factories
                    .iter()
                    .any(|factory| factory.protocol() == protocol)
            {
                return Err(DiscoveryError::MissingFactory(protocol));
            }
        }

        // Gather the candidate slots of every (query, matching batchable factory)
        // into one set, then resolve them all in a single batched read.
        let mut slots: Vec<(Address, U256)> = Vec::new();
        for query in &queries {
            for factory in self
                .factories
                .iter()
                .filter(|factory| query.protocol.is_none_or(|p| factory.protocol() == p))
            {
                slots.extend(factory.candidate_reads(&query.pairs));
            }
        }
        slots.sort_unstable();
        slots.dedup();

        let resolved: HashMap<(Address, U256), U256> = if slots.is_empty() {
            HashMap::new()
        } else {
            let values = cache.read_storage_slots(&slots)?;
            slots.into_iter().zip(values).collect()
        };

        // Assemble, de-duplicating by pool key across (possibly overlapping) queries.
        let mut found = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for query in &queries {
            for factory in self
                .factories
                .iter()
                .filter(|factory| query.protocol.is_none_or(|p| factory.protocol() == p))
            {
                let pools = if factory.candidate_reads(&query.pairs).is_empty() {
                    // Legacy factory (no `candidate_reads`): per-pair fallback.
                    let mut out = Vec::new();
                    for pair in &query.pairs {
                        out.extend(factory.find_pools(cache, *pair)?);
                    }
                    out
                } else {
                    factory.assemble_pairs(&query.pairs, &resolved)?
                };
                for pool in pools {
                    if seen.insert(pool.key.clone()) {
                        found.push(pool);
                    }
                }
            }
        }

        Ok(found)
    }

    /// Resolve a [`PoolQuery`] into discovered pools in a single batched read.
    ///
    /// Every pair in the query — a single pair, a token basket's `C(n, 2)`
    /// combinations, or an explicit pair set — is resolved across all matching
    /// factories at once: batchable factories (the built-in Uniswap V2 and V3)
    /// contribute their candidate mapping slots to ONE
    /// [`AdapterCache::read_storage_slots`] call (a single bulk `eth_call` on an
    /// `EvmCache`), so request count scales with factory count, not with pairs,
    /// fee tiers, or basket size. External factories that only implement
    /// [`find_pools`](PoolFactory::find_pools) fall back to a per-pair call.
    ///
    /// An unscoped query spans every matching factory. If the query is scoped
    /// with [`PoolQuery::on`] to a protocol that has no registered factory, this
    /// returns [`DiscoveryError::MissingFactory`]; an unscoped query never errors
    /// on missing factories (empty pairs simply yield `Ok(vec![])`).
    pub fn find(
        &self,
        cache: &mut dyn AdapterCache,
        query: PoolQuery,
    ) -> Result<Vec<DiscoveredPool>, DiscoveryError> {
        self.find_many(cache, std::iter::once(query))
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
                let tick_spacing = i24_from_word(
                    values
                        .get(&(self.config.factory, tick_slot))
                        .copied()
                        .unwrap_or_default(),
                );
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
