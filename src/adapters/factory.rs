//! Factory-backed pool discovery.
//!
//! Discovery is intentionally read-only: factory drivers produce
//! [`PoolRegistration`] values, and callers still decide when to cold-start and
//! register them.
//!
//! ## Concentrated-liquidity presets: on-chain-verified constants
//!
//! Every preset's storage constants are verified against a forked archive node
//! by the gated (`#[ignore]`) `discovery_cl_rpc` / `discovery_solidly_rpc` parity
//! tests — discovered via evm-fork-cache's trace-based slot probe, not guessed:
//!
//! - **Uniswap V3** ([`ClFactorySpec::uniswap_v3`]): `getPool` / `feeAmountTickSpacing`
//!   base slots 5 / 4, canonical fee tiers.
//! - **PancakeSwap V3** ([`ClFactorySpec::pancake_v3`]): `getPool` base slot **2**
//!   and `feeAmountTickSpacing` base slot **1** (verified — they differ from
//!   Uniswap's 5 / 4), plus the `PoolDeployer`, pool init-code hash, and
//!   `QuoterV2`. `verify_derivations` is ON (the mapping answer is cross-checked
//!   against the CREATE2 derivation on first use).
//! - **Slipstream / Aerodrome CL** ([`ClFactorySpec::slipstream`]): `getPool`
//!   base slot **6** (verified). Discovery-only: `quoter` is `None` because the
//!   Slipstream quoter takes a `tickSpacing`-keyed struct, not the Uniswap
//!   `(…, fee, …)` struct this crate encodes — so its sim rides the caller's
//!   Uniswap-compatible quoter (see the [`ClFactorySpec::slipstream`] docs).
//! - **Solidly V2 / Aerodrome** ([`SolidlyFactoryConfig::aerodrome`]): `getPool`
//!   base slot **5** + the pool storage layout (reserves @ 20/21, tokens @ 13/14),
//!   verified on-chain.
//!
//! ## Out of scope: integrator-supplied discovery
//!
//! Two AMM shapes ship no built-in discovery factory; supply their pools via
//! explicit registration or a custom [`PoolFactory`] added through
//! [`PoolDiscovery::with_factory`]:
//! - **Balancer V2** — no on-chain token→pool index, so discovery is an async
//!   log scan rather than a cache read.
//! - **Curve** — its MetaRegistry `find_pools_for_coins` view reverts against a
//!   live node, so ViewCall discovery is not built in. The Curve *adapter* still
//!   simulates explicitly-registered Curve pools.

use std::collections::HashMap;
use std::fmt;

#[cfg(any(feature = "uniswap-v2", feature = "uniswap-v3"))]
#[cfg(any(feature = "uniswap-v2", feature = "uniswap-v3", feature = "solidly-v2"))]
use super::ProtocolMetadata;
#[cfg(feature = "solidly-v2")]
use super::SolidlyV2Metadata;
#[cfg(feature = "uniswap-v2")]
use super::UniswapV2Metadata;
#[cfg(feature = "uniswap-v3")]
use super::V3Metadata;
use super::{AdapterCache, AdapterRegistry, EventSource, PoolKey, PoolRegistration, ProtocolId};
#[cfg(feature = "solidly-v2")]
use crate::adapters::storage::SolidlyStorageLayout;
#[cfg(feature = "uniswap-v3")]
use crate::adapters::storage::V3StorageLayout;
#[cfg(any(feature = "uniswap-v2", feature = "uniswap-v3", feature = "solidly-v2"))]
use alloy_primitives::B256;
use alloy_primitives::{Address, Log, U256};
#[cfg(any(feature = "uniswap-v2", feature = "uniswap-v3", feature = "solidly-v2"))]
use alloy_sol_types::SolEvent;

/// Factory-level derivation helpers.
pub mod derive {
    use alloy_primitives::Address;
    #[cfg(any(feature = "uniswap-v2", feature = "uniswap-v3", feature = "solidly-v2"))]
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

    /// Compute the storage key for a tickSpacing-keyed
    /// `getPool[token0][token1][tickSpacing]` mapping (Slipstream / Aerodrome CL),
    /// where the innermost key is an `int24` tickSpacing.
    ///
    /// The tickSpacing is sign-extended to a full 256-bit word before hashing —
    /// the same two's-complement encoding Solidity uses for a signed mapping key —
    /// so a (non-negative in practice) spacing produces the exact storage slot the
    /// contract wrote. Mirrors [`v3_get_pool_slot`] but with a signed innermost
    /// key instead of a `uint24` fee.
    #[cfg(feature = "uniswap-v3")]
    pub fn v3_get_pool_slot_by_spacing(
        base_slot: U256,
        token0: Address,
        token1: Address,
        spacing: i32,
    ) -> U256 {
        let first = mapping_slot_address(base_slot, token0);
        let second = mapping_slot_address(first, token1);
        mapping_slot_u256(second, i24_to_word(spacing))
    }

    /// Compute the storage key for `UniswapV3Factory.feeAmountTickSpacing[fee]`.
    #[cfg(feature = "uniswap-v3")]
    pub fn v3_fee_amount_tick_spacing_slot(base_slot: U256, fee: u32) -> U256 {
        mapping_slot_u256(base_slot, U256::from(fee))
    }

    /// Compute a tick-spacing-keyed fee mapping slot.
    #[cfg(feature = "uniswap-v3")]
    pub fn v3_tick_spacing_fee_slot(base_slot: U256, spacing: i32) -> U256 {
        mapping_slot_u256(base_slot, i24_to_word(spacing))
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

    /// Compute a tickSpacing-keyed CL pool address via the standard V3 `abi.encode
    /// (token0, token1, int24 spacing)` salt (Slipstream/Aerodrome shape).
    ///
    /// Note: some tickSpacing-keyed forks deploy pools as minimal-proxy clones
    /// with a different salt/creation-code scheme; treat this as the standard-V3
    /// derivation and verify a specific fork's formula before enabling its
    /// CREATE2 cross-check.
    #[cfg(feature = "uniswap-v3")]
    pub fn v3_pool_address_by_spacing(
        deployer: Address,
        init_code_hash: B256,
        token0: Address,
        token1: Address,
        spacing: i32,
    ) -> Address {
        let mut salt_preimage = [0u8; 96];
        salt_preimage[12..32].copy_from_slice(token0.as_slice());
        salt_preimage[44..64].copy_from_slice(token1.as_slice());
        // int24 spacing, sign-extended into the trailing 32-byte word.
        salt_preimage[64..96].copy_from_slice(&i24_to_word(spacing).to_be_bytes::<32>());
        create2_address(deployer, keccak256(salt_preimage), init_code_hash)
    }

    /// Compute the storage key for a Solidly V2 (Aerodrome / Velodrome V2)
    /// `PoolFactory.getPool[token0][token1][stable]` entry.
    ///
    /// The factory keys pools by a nested
    /// `mapping(address => mapping(address => mapping(bool => address)))`: the two
    /// address levels descend exactly like Uniswap V3's `getPool[t0][t1]`, and the
    /// innermost `bool` key is encoded as a 32-byte word (`1` for stable, `0` for
    /// volatile) — the same encoding Solidity uses for a `bool` mapping key.
    #[cfg(feature = "solidly-v2")]
    pub fn solidly_get_pool_slot(
        base_slot: U256,
        token0: Address,
        token1: Address,
        stable: bool,
    ) -> U256 {
        let first = mapping_slot_address(base_slot, token0);
        let second = mapping_slot_address(first, token1);
        mapping_slot_u256(second, U256::from(stable as u8))
    }

    /// Compute a Solidly V2 pool address via the factory CREATE2 formula.
    ///
    /// The salt is `keccak256(abi.encodePacked(token0, token1, stable))` — a
    /// tightly packed 41-byte preimage (20 + 20 + 1), matching Aerodrome's
    /// `PoolFactory.createPool`. Used as an optional discovery cross-check against
    /// the `getPool` mapping answer.
    #[cfg(feature = "solidly-v2")]
    pub fn solidly_pool_address(
        deployer: Address,
        init_code_hash: B256,
        token0: Address,
        token1: Address,
        stable: bool,
    ) -> Address {
        let mut salt_preimage = [0u8; 41];
        salt_preimage[..20].copy_from_slice(token0.as_slice());
        salt_preimage[20..40].copy_from_slice(token1.as_slice());
        salt_preimage[40] = stable as u8;
        create2_address(deployer, keccak256(salt_preimage), init_code_hash)
    }

    #[cfg(feature = "uniswap-v2")]
    fn nested_address_mapping_slot(base_slot: U256, outer: Address, inner: Address) -> U256 {
        let outer_slot = mapping_slot_address(base_slot, outer);
        mapping_slot_address(outer_slot, inner)
    }

    #[cfg(any(feature = "uniswap-v2", feature = "uniswap-v3", feature = "solidly-v2"))]
    fn mapping_slot_address(base_slot: U256, key: Address) -> U256 {
        let mut preimage = [0u8; 64];
        preimage[12..32].copy_from_slice(key.as_slice());
        preimage[32..64].copy_from_slice(&base_slot.to_be_bytes::<32>());
        keccak256(preimage).into()
    }

    #[cfg(any(feature = "uniswap-v3", feature = "solidly-v2"))]
    fn mapping_slot_u256(base_slot: U256, key: U256) -> U256 {
        let mut preimage = [0u8; 64];
        preimage[..32].copy_from_slice(&key.to_be_bytes::<32>());
        preimage[32..64].copy_from_slice(&base_slot.to_be_bytes::<32>());
        keccak256(preimage).into()
    }

    /// Sign-extend an `int24` value into a 256-bit word, matching how Solidity
    /// encodes a signed mapping key (a negative value fills the high bytes with
    /// `0xff`). Used to key tickSpacing-mapped `getPool` slots.
    #[cfg(feature = "uniswap-v3")]
    fn i24_to_word(value: i32) -> U256 {
        // A 24-bit signed value fits in an i64 losslessly; `as u64` then produces
        // the two's-complement 64-bit pattern, and `U256::from(u64)` zero-extends
        // — so re-apply the sign across the full 256 bits for negatives.
        if value < 0 {
            // (-1 as U256) is all-ones; mask in the low 64-bit two's-complement.
            let low = U256::from(value as i64 as u64);
            let high_ones = (U256::MAX >> 64) << 64;
            low | high_ones
        } else {
            U256::from(value as u64)
        }
    }

    #[cfg(any(feature = "uniswap-v2", feature = "uniswap-v3", feature = "solidly-v2"))]
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
/// PancakeSwap V3 `getPool` + `feeAmountTickSpacing` mapping base slots —
/// VERIFIED on-chain (mainnet block 20_000_000) via evm-fork-cache trace-based
/// slot discovery. Pancake's factory storage layout differs from Uniswap's 5 / 4.
#[cfg(feature = "uniswap-v3")]
const PANCAKE_V3_GET_POOL_BASE_SLOT: U256 = U256::from_limbs([2, 0, 0, 0]);
#[cfg(feature = "uniswap-v3")]
const PANCAKE_V3_FEE_AMOUNT_TICK_SPACING_BASE_SLOT: U256 = U256::from_limbs([1, 0, 0, 0]);
/// Slipstream (Aerodrome CL) factory `getPool` mapping base slot — VERIFIED
/// on-chain (Base block 47_700_000) via trace-based slot discovery.
#[cfg(feature = "uniswap-v3")]
const SLIPSTREAM_GET_POOL_BASE_SLOT: U256 = U256::from_limbs([6, 0, 0, 0]);
#[cfg(feature = "uniswap-v3")]
const UNISWAP_V3_CANONICAL_FEE_TIERS: [u32; 4] = [100, 500, 3_000, 10_000];
/// PancakeSwap V3 fee tiers (hundredths of a bip): 0.01% / 0.05% / 0.25% / 1%.
#[cfg(feature = "uniswap-v3")]
const PANCAKE_V3_FEE_TIERS: [u32; 4] = [100, 500, 2_500, 10_000];
/// Aerodrome/Velodrome Slipstream tickSpacing tiers (int24). VERIFY against a
/// live CLFactory before relying on production discovery — see the gated parity
/// test. These are the commonly-deployed spacings but are not exhaustive.
#[cfg(feature = "uniswap-v3")]
const SLIPSTREAM_TICK_SPACINGS: [i32; 5] = [1, 50, 100, 200, 2_000];

// --- Real on-chain constants for the CL presets (Ethereum mainnet / Base). ---
// Verified against block-explorer / SDK sources; the storage-slot bases for
// forks whose factory layout was not confirmed on-chain are gated behind the
// `#[ignore]` RPC parity test rather than trusted blindly. See the module report.

/// PancakeSwap V3 `PancakeV3PoolDeployer` (the CREATE2 deployer; the factory is a
/// separate address). Same deterministic address across mainnet/BSC/Arbitrum.
#[cfg(feature = "uniswap-v3")]
const PANCAKE_V3_POOL_DEPLOYER: Address =
    alloy_primitives::address!("41ff9AA7e16B8B1a8a8dc4f0eFacd93D02d071c9");
/// PancakeSwap V3 pool init-code hash (CREATE2 salt cross-check). "Most chains"
/// value per the Pancake v3-sdk (mainnet included; zkSync differs).
#[cfg(feature = "uniswap-v3")]
const PANCAKE_V3_INIT_CODE_HASH: B256 =
    alloy_primitives::b256!("6ce8eb472fa82df5469c6ab6d485f17c3ad13c8cd7af59b3d4a8026c5ce0f7e2");
/// PancakeSwap V3 `QuoterV2` (Ethereum mainnet; deterministic across several EVM
/// chains). Uses the Uniswap-compatible `(…, fee, …)` quote struct.
#[cfg(feature = "uniswap-v3")]
const PANCAKE_V3_QUOTER_V2: Address =
    alloy_primitives::address!("B048Bbc1Ee6b733FFfCFb9e9CeF7375518e25997");

// --- Solidly V2 (Aerodrome / Velodrome V2) preset constants. ---
//
// VERIFIED on-chain (Aerodrome PoolFactory, Base block 47_700_000) via
// evm-fork-cache trace-based slot discovery: the `_getPool[token0][token1][stable]`
// mapping lives at base slot 5. No CREATE2 init-code hash is pinned (Solidly's
// salt is a packed `keccak(t0‖t1‖stable)`), so `verify_derivations` stays OFF;
// the gated `discovery_solidly_rpc` parity test re-checks the slot on Base.
#[cfg(feature = "solidly-v2")]
const SOLIDLY_GET_POOL_BASE_SLOT: U256 = U256::from_limbs([5, 0, 0, 0]);

/// Uniswap V2 `PairCreated` factory event (crate-internal), wrapped like
/// [`solidly_events`] so the binding stays out of the public API.
#[cfg(feature = "uniswap-v2")]
mod v2_factory_events {
    alloy_sol_types::sol! {
        event PairCreated(address indexed token0, address indexed token1, address pair, uint256 allPairsLength);
    }
}
#[cfg(feature = "uniswap-v2")]
use v2_factory_events::PairCreated;

/// CL-family factory pool-creation events (crate-internal, not public API).
#[cfg(feature = "uniswap-v3")]
mod cl_factory_events {
    alloy_sol_types::sol! {
        /// Uniswap/Pancake-style fee-keyed pool-creation event.
        event PoolCreated(address indexed token0, address indexed token1, uint24 indexed fee, int24 tickSpacing, address pool);

        /// Slipstream/Aerodrome CL tickSpacing-keyed pool-creation event. The
        /// indexed key is `tickSpacing` (int24) rather than a fee; the pool address
        /// and (unindexed) fee follow in the data.
        event PoolCreatedTickSpacing(address indexed token0, address indexed token1, int24 indexed tickSpacing, address pool, uint24 fee);
    }
}
#[cfg(feature = "uniswap-v3")]
use cl_factory_events::{PoolCreated, PoolCreatedTickSpacing};

/// Solidly V2 (Aerodrome / Velodrome V2) pool-creation event, wrapped in its own
/// module so the generated struct keeps the on-chain name `PoolCreated` (whose
/// `SIGNATURE_HASH` is the real topic0) without colliding with the Uniswap/Pancake
/// [`PoolCreated`] already defined above. The indexed keys are `token0`, `token1`,
/// and the `bool stable` flag; the pool address and the (unindexed) all-pools
/// length follow in the data. Verbatim from Aerodrome's `IPoolFactory` (the
/// trailing `uint256` is unnamed on-chain; named `allPoolsLength` here so the
/// generated decoder has a field).
#[cfg(feature = "solidly-v2")]
mod solidly_events {
    alloy_sol_types::sol! {
        event PoolCreated(address indexed token0, address indexed token1, bool indexed stable, address pool, uint256 allPoolsLength);
    }
}
#[cfg(feature = "solidly-v2")]
use solidly_events::PoolCreated as SolidlyPoolCreated;

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
    /// Uniswap V2 factories (canonical plus same-protocol forks).
    #[cfg(feature = "uniswap-v2")]
    pub uniswap_v2: Vec<UniswapV2FactoryConfig>,
    /// Concentrated-liquidity forks (Uniswap V3, SushiSwap V3, PancakeSwap V3,
    /// Slipstream). Every UniV3-mechanics fork is one [`ClFactorySpec`]; the
    /// V3-family adapter emits a [`ConcentratedLiquidityFactory`] per spec whose
    /// protocol it serves.
    #[cfg(feature = "uniswap-v3")]
    pub concentrated_liquidity: Vec<ClFactorySpec>,
    /// Solidly V2 forks (Aerodrome / Velodrome V2). Every fork is one
    /// [`SolidlyFactoryConfig`]; the Solidly adapter emits a [`SolidlyFactory`]
    /// per config.
    #[cfg(feature = "solidly-v2")]
    pub solidly: Vec<SolidlyFactoryConfig>,
    /// Global switch for the CREATE2 cross-check: a spec's derivation
    /// verification runs only when both this and the spec opt in. Defaults `true`.
    pub verify_derivations: bool,
}

impl Default for FactoryConfig {
    fn default() -> Self {
        Self {
            #[cfg(feature = "uniswap-v2")]
            uniswap_v2: Vec::new(),
            #[cfg(feature = "uniswap-v3")]
            concentrated_liquidity: Vec::new(),
            #[cfg(feature = "solidly-v2")]
            solidly: Vec::new(),
            verify_derivations: true,
        }
    }
}

impl FactoryConfig {
    /// Add a Uniswap V2 factory by its [`UniswapV2FactoryConfig`].
    #[cfg(feature = "uniswap-v2")]
    pub fn with_uniswap_v2(mut self, config: UniswapV2FactoryConfig) -> Self {
        self.uniswap_v2.push(config);
        self
    }

    /// Add a canonical Uniswap V2 factory at `factory`.
    #[cfg(feature = "uniswap-v2")]
    pub fn with_uniswap_v2_factory(self, factory: Address) -> Self {
        self.with_uniswap_v2(UniswapV2FactoryConfig::uniswap_v2(factory))
    }

    /// Add a concentrated-liquidity fork by its [`ClFactorySpec`] — the general
    /// entry point behind the per-fork conveniences below.
    #[cfg(feature = "uniswap-v3")]
    pub fn with_concentrated_liquidity(mut self, spec: ClFactorySpec) -> Self {
        self.concentrated_liquidity.push(spec);
        self
    }

    /// Add a concentrated-liquidity fork by its [`ClFactorySpec`].
    ///
    /// Alias for [`with_concentrated_liquidity`](Self::with_concentrated_liquidity),
    /// kept so existing call sites that passed a V3 factory config keep reading
    /// naturally now that the config is a `ClFactorySpec`.
    #[cfg(feature = "uniswap-v3")]
    pub fn with_uniswap_v3(self, spec: ClFactorySpec) -> Self {
        self.with_concentrated_liquidity(spec)
    }

    /// Add a canonical Uniswap V3 factory at `factory`.
    #[cfg(feature = "uniswap-v3")]
    pub fn with_uniswap_v3_factory(self, factory: Address) -> Self {
        self.with_concentrated_liquidity(ClFactorySpec::uniswap_v3(factory))
    }

    /// Add a PancakeSwap V3 factory at `factory` (Pancake preset).
    #[cfg(feature = "uniswap-v3")]
    pub fn with_pancake_v3_factory(self, factory: Address) -> Self {
        self.with_concentrated_liquidity(ClFactorySpec::pancake_v3(factory))
    }

    /// Add a Slipstream / Aerodrome CL factory at `factory` (Slipstream preset).
    #[cfg(feature = "uniswap-v3")]
    pub fn with_slipstream_factory(self, factory: Address) -> Self {
        self.with_concentrated_liquidity(ClFactorySpec::slipstream(factory))
    }

    /// Add a Solidly V2 fork (Aerodrome / Velodrome V2) by its
    /// [`SolidlyFactoryConfig`].
    #[cfg(feature = "solidly-v2")]
    pub fn with_solidly(mut self, config: SolidlyFactoryConfig) -> Self {
        self.solidly.push(config);
        self
    }

    /// Add a Solidly V2 factory at `factory` using the Aerodrome preset.
    #[cfg(feature = "solidly-v2")]
    pub fn with_solidly_factory(self, factory: Address) -> Self {
        self.with_solidly(SolidlyFactoryConfig::aerodrome(factory))
    }

    /// Toggle the global CREATE2 derivation cross-check (default `true`).
    pub fn with_verify_derivations(mut self, verify_derivations: bool) -> Self {
        self.verify_derivations = verify_derivations;
        self
    }
}

/// Configuration for one Uniswap V2 (or V2-fork) factory.
#[cfg(feature = "uniswap-v2")]
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UniswapV2FactoryConfig {
    /// The factory contract address.
    pub factory: Address,
    /// Base storage slot of the factory's `getPair[t0][t1]` mapping.
    pub get_pair_base_slot: U256,
    /// Optional pair init-code hash for the CREATE2 cross-check.
    pub init_code_hash: Option<B256>,
    /// Optional swap fee (basis points) carried into discovered pool metadata.
    pub fee_bps: Option<u32>,
}

#[cfg(feature = "uniswap-v2")]
impl UniswapV2FactoryConfig {
    /// A canonical Uniswap V2 factory preset at `factory`.
    pub fn uniswap_v2(factory: Address) -> Self {
        Self {
            factory,
            get_pair_base_slot: UNISWAP_V2_GET_PAIR_BASE_SLOT,
            init_code_hash: None,
            fee_bps: None,
        }
    }

    /// Override the `getPair` mapping base slot (for a non-canonical fork).
    pub fn with_get_pair_base_slot(mut self, slot: U256) -> Self {
        self.get_pair_base_slot = slot;
        self
    }

    /// Set the pair init-code hash (enables the CREATE2 cross-check).
    pub fn with_init_code_hash(mut self, hash: B256) -> Self {
        self.init_code_hash = Some(hash);
        self
    }

    /// Set the swap fee (basis points) for discovered pools.
    pub fn with_fee_bps(mut self, fee_bps: u32) -> Self {
        self.fee_bps = Some(fee_bps);
        self
    }
}

/// How a concentrated-liquidity factory keys its `getPool` mapping and where the
/// per-pool tick spacing comes from.
///
/// Two shapes cover every UniV3-mechanics fork this crate serves:
/// - [`Fee`](Self::Fee): the innermost `getPool` key is the fee tier
///   (`getPool[t0][t1][fee]`), and the spacing is read from
///   `feeAmountTickSpacing[fee]` — Uniswap V3, SushiSwap V3, PancakeSwap V3.
/// - [`TickSpacing`](Self::TickSpacing): the innermost key *is* the tick spacing
///   (`getPool[t0][t1][tickSpacing]`); there is no `feeAmountTickSpacing` read —
///   Slipstream / Aerodrome CL.
#[cfg(feature = "uniswap-v3")]
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClKeying {
    /// `getPool[t0][t1][fee]`; tick spacing comes from `feeAmountTickSpacing[fee]`.
    Fee {
        /// The fee tiers (hundredths of a bip) to probe per pair.
        tiers: Vec<u32>,
        /// Base slot of the `feeAmountTickSpacing` mapping.
        fee_amount_tick_spacing_base_slot: U256,
    },
    /// `getPool[t0][t1][tickSpacing]`; NO `feeAmountTickSpacing` read.
    TickSpacing {
        /// The int24 tick spacings to probe per pair.
        spacings: Vec<i32>,
    },
}

/// Where a discovered pool's swap `fee` (hundredths of a bip) is resolved from.
/// STATIC — resolved once at discovery time, never recomputed per-swap.
#[cfg(feature = "uniswap-v3")]
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FeeSource {
    /// The fee is the pool-key tier itself (only valid with [`ClKeying::Fee`]).
    Key,
    /// Read the fee from a factory mapping keyed by the pool key
    /// (`feeAmountTickSpacing`-style for fee keying, `tickSpacingToFee[spacing]`
    /// for tickSpacing keying) at `base_slot`.
    FactoryMapping {
        /// Base slot of the fee mapping.
        base_slot: U256,
    },
    /// A constant fee for every pool of this factory.
    Fixed(u32),
}

/// Optional CREATE2 cross-check for a concentrated-liquidity fork: when present
/// (and [`ClFactorySpec::verify_derivations`] is on), the factory re-derives the
/// pool address from the salt and compares it to the mapping answer, hard-failing
/// [`DiscoveryError::DerivationMismatch`] on disagreement — a guardrail against a
/// wrong init-code hash or base slot.
#[cfg(feature = "uniswap-v3")]
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClCreate2 {
    /// The CREATE2 deployer. `None` means "the factory itself" (Uniswap V3);
    /// forks that deploy from a separate contract (PancakeSwap) set it explicitly.
    pub deployer: Option<Address>,
    /// The pool init-code hash.
    pub init_code_hash: B256,
}

/// Data-driven specification of a concentrated-liquidity (UniV3-mechanics) fork,
/// driving [`ConcentratedLiquidityFactory`]. Construct with the
/// [`fee_keyed`](Self::fee_keyed) / [`tick_spacing_keyed`](Self::tick_spacing_keyed)
/// constructors or a fork preset ([`uniswap_v3`](Self::uniswap_v3),
/// [`sushi_v3`](Self::sushi_v3), [`pancake_v3`](Self::pancake_v3),
/// [`slipstream`](Self::slipstream)), then refine with the `with_*` builders.
#[cfg(feature = "uniswap-v3")]
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClFactorySpec {
    /// The protocol identity discovered pools register under (drives the
    /// [`PoolKey`]/[`ProtocolMetadata`] variant and storage-layout preset).
    pub protocol: ProtocolId,
    /// The factory contract address.
    pub factory: Address,
    /// Base slot of the `getPool` nested mapping.
    pub get_pool_base_slot: U256,
    /// How the `getPool` mapping is keyed and where tick spacing comes from.
    pub keying: ClKeying,
    /// Where a discovered pool's fee is resolved from.
    pub fee_source: FeeSource,
    /// Optional CREATE2 cross-check of the mapping answer.
    pub create2: Option<ClCreate2>,
    /// Per-fork swap-quote target. `None` => the caller's
    /// [`SimConfig::v3_quoter`](super::SimConfig::v3_quoter).
    pub quoter: Option<Address>,
    /// Push-discovery creation-event `topic0`. `None` => pull-only (no creation
    /// sources, `decode_creation` returns `Ok(None)`).
    pub creation_topic0: Option<B256>,
    /// Whether to run the CREATE2 cross-check (requires [`create2`](Self::create2)).
    pub verify_derivations: bool,
}

#[cfg(feature = "uniswap-v3")]
impl ClFactorySpec {
    /// A fee-keyed CL fork (Uniswap/Sushi/Pancake shape): `getPool[t0][t1][fee]`
    /// with tick spacing from `feeAmountTickSpacing[fee]`, `fee_source: Key`.
    pub fn fee_keyed(
        protocol: ProtocolId,
        factory: Address,
        get_pool_base_slot: U256,
        fee_amount_tick_spacing_base_slot: U256,
        tiers: impl IntoIterator<Item = u32>,
    ) -> Self {
        Self {
            protocol,
            factory,
            get_pool_base_slot,
            keying: ClKeying::Fee {
                tiers: tiers.into_iter().collect(),
                fee_amount_tick_spacing_base_slot,
            },
            fee_source: FeeSource::Key,
            create2: None,
            quoter: None,
            creation_topic0: None,
            verify_derivations: false,
        }
    }

    /// A tickSpacing-keyed CL fork (Slipstream shape): `getPool[t0][t1][spacing]`
    /// only — no `feeAmountTickSpacing` read. Fee defaults to `Fixed(0)`, the
    /// "no fee mapping" sentinel: discovered registrations leave `V3Metadata.fee`
    /// **unset** (so `simulate_swap` returns `MissingMetadata("V3 fee")` rather
    /// than quoting at fee 0 — these forks are discovery-only for quoting unless
    /// the caller supplies a compatible quoter). Set a real
    /// [`fee_source`](Self::with_fee_source) if the fork exposes one on-chain.
    pub fn tick_spacing_keyed(
        protocol: ProtocolId,
        factory: Address,
        get_pool_base_slot: U256,
        spacings: impl IntoIterator<Item = i32>,
    ) -> Self {
        Self {
            protocol,
            factory,
            get_pool_base_slot,
            keying: ClKeying::TickSpacing {
                spacings: spacings.into_iter().collect(),
            },
            fee_source: FeeSource::Fixed(0),
            create2: None,
            quoter: None,
            creation_topic0: None,
            verify_derivations: false,
        }
    }

    /// Set the per-fork swap-quote target (a fork's own QuoterV2).
    pub fn with_quoter(mut self, quoter: Address) -> Self {
        self.quoter = Some(quoter);
        self
    }

    /// Set the CREATE2 cross-check (`deployer: None` => the factory itself).
    pub fn with_create2(mut self, deployer: Option<Address>, init_code_hash: B256) -> Self {
        self.create2 = Some(ClCreate2 {
            deployer,
            init_code_hash,
        });
        self
    }

    /// Toggle the CREATE2 derivation cross-check (needs [`with_create2`](Self::with_create2)).
    pub fn with_verify_derivations(mut self, verify: bool) -> Self {
        self.verify_derivations = verify;
        self
    }

    /// Set the push-discovery creation-event `topic0`.
    pub fn with_creation_topic0(mut self, topic0: B256) -> Self {
        self.creation_topic0 = Some(topic0);
        self
    }

    /// Set where a discovered pool's fee is resolved from.
    pub fn with_fee_source(mut self, fee_source: FeeSource) -> Self {
        self.fee_source = fee_source;
        self
    }

    /// Replace the probed fee tiers (fee keying) — no-op under tickSpacing keying.
    pub fn with_fee_tiers(mut self, fee_tiers: impl IntoIterator<Item = u32>) -> Self {
        if let ClKeying::Fee { tiers, .. } = &mut self.keying {
            *tiers = fee_tiers.into_iter().collect();
        }
        self
    }

    /// Replace the probed tick spacings (tickSpacing keying) — no-op under fee keying.
    pub fn with_tick_spacings(mut self, spacings: impl IntoIterator<Item = i32>) -> Self {
        if let ClKeying::TickSpacing { spacings: s } = &mut self.keying {
            *s = spacings.into_iter().collect();
        }
        self
    }

    /// The `feeAmountTickSpacing` mapping base slot for a fee-keyed spec; `None`
    /// for a tickSpacing-keyed spec (which has no such mapping).
    pub fn fee_amount_tick_spacing_base_slot(&self) -> Option<U256> {
        match &self.keying {
            ClKeying::Fee {
                fee_amount_tick_spacing_base_slot,
                ..
            } => Some(*fee_amount_tick_spacing_base_slot),
            ClKeying::TickSpacing { .. } => None,
        }
    }

    /// The fee tiers probed per pair under fee keying; empty under tickSpacing
    /// keying (which probes spacings, not fees — see [`tick_spacings`](Self::tick_spacings)).
    pub fn fee_tiers(&self) -> &[u32] {
        match &self.keying {
            ClKeying::Fee { tiers, .. } => tiers,
            ClKeying::TickSpacing { .. } => &[],
        }
    }

    /// The tick spacings probed per pair under tickSpacing keying; empty under
    /// fee keying.
    pub fn tick_spacings(&self) -> &[i32] {
        match &self.keying {
            ClKeying::TickSpacing { spacings } => spacings,
            ClKeying::Fee { .. } => &[],
        }
    }

    // --- Fork presets ---

    /// Canonical Uniswap V3 factory: base slot 5, fee tiers `[100, 500, 3000,
    /// 10000]` with `feeAmountTickSpacing` at slot 4, canonical `PoolCreated`
    /// push topic. No CREATE2 hash by default (add one with
    /// [`with_create2`](Self::with_create2) to enable the cross-check); quoter
    /// left to the caller.
    pub fn uniswap_v3(factory: Address) -> Self {
        Self::fee_keyed(
            ProtocolId::UniswapV3,
            factory,
            UNISWAP_V3_GET_POOL_BASE_SLOT,
            UNISWAP_V3_FEE_AMOUNT_TICK_SPACING_BASE_SLOT,
            UNISWAP_V3_CANONICAL_FEE_TIERS,
        )
        .with_creation_topic0(PoolCreated::SIGNATURE_HASH)
    }

    /// SushiSwap V3 — byte-identical Uniswap V3 fork. Registers as
    /// [`ProtocolId::UniswapV3`] (same PoolKey/adapter); differs only by factory
    /// address and (optionally) quoter. Its init-code hash is left unset (pin it
    /// with [`with_create2`](Self::with_create2); the gated parity test verifies).
    pub fn sushi_v3(factory: Address) -> Self {
        // Same shape as Uniswap V3 (Sushi V3 pools register as UniswapV3).
        Self::uniswap_v3(factory)
    }

    /// PancakeSwap V3 factory. Protocol [`ProtocolId::PancakeV3`], fee tiers
    /// `[100, 500, 2500, 10000]`, CREATE2 via the Pancake `PoolDeployer` +
    /// init-code hash, and the Pancake `QuoterV2` (Uniswap-compatible quote ABI).
    ///
    /// The `getPool` (slot 2) and `feeAmountTickSpacing` (slot 1) base slots are
    /// VERIFIED on-chain — they differ from Uniswap's 5 / 4 — and the CREATE2
    /// deployer + init-code hash reproduce the getter's pool, so
    /// `verify_derivations` is ON (the mapping answer is cross-checked against the
    /// CREATE2 derivation on first use).
    pub fn pancake_v3(factory: Address) -> Self {
        Self::fee_keyed(
            ProtocolId::PancakeV3,
            factory,
            PANCAKE_V3_GET_POOL_BASE_SLOT,
            PANCAKE_V3_FEE_AMOUNT_TICK_SPACING_BASE_SLOT,
            PANCAKE_V3_FEE_TIERS,
        )
        .with_create2(Some(PANCAKE_V3_POOL_DEPLOYER), PANCAKE_V3_INIT_CODE_HASH)
        .with_quoter(PANCAKE_V3_QUOTER_V2)
        .with_creation_topic0(PoolCreated::SIGNATURE_HASH)
        .with_verify_derivations(true)
    }

    /// Slipstream / Aerodrome CL factory. Protocol [`ProtocolId::Slipstream`],
    /// tickSpacing-keyed, spacings `[1, 50, 100, 200, 2000]`.
    ///
    /// The `getPool` base slot is VERIFIED on-chain (Base CLFactory). `create2`
    /// and `quoter` are left `None` on purpose:
    /// - the pool init-code hash + full spacing table are not pinned here (the
    ///   gated parity test covers the base slot);
    /// - the Slipstream quoter takes a `tickSpacing`-keyed struct, NOT the
    ///   Uniswap `(…, fee, …)` struct this crate encodes, so wiring it as the V3
    ///   quote target would send malformed calldata. Slipstream is therefore
    ///   discovery-only for quoting: its discovered `fee` is left unset, so
    ///   `simulate_swap` returns `MissingMetadata("V3 fee")` unless the caller
    ///   supplies a Slipstream-compatible quoter and fee.
    pub fn slipstream(factory: Address) -> Self {
        Self::tick_spacing_keyed(
            ProtocolId::Slipstream,
            factory,
            SLIPSTREAM_GET_POOL_BASE_SLOT,
            SLIPSTREAM_TICK_SPACINGS,
        )
        .with_creation_topic0(PoolCreatedTickSpacing::SIGNATURE_HASH)
    }
}

/// Backwards-compatible name for the canonical V3-family factory spec.
///
/// Existing call sites can keep using `UniswapV3FactoryConfig::uniswap_v3(...)`
/// while the implementation handles broader concentrated-liquidity forks via
/// [`ClFactorySpec`].
#[cfg(feature = "uniswap-v3")]
pub type UniswapV3FactoryConfig = ClFactorySpec;

/// Aerodrome / Velodrome V2 pool storage layout used by the `aerodrome`/
/// `velodrome` presets — VERIFIED on-chain (Aerodrome WETH/USDC pool, Base block
/// 47_700_000): reserve0 @ 20, reserve1 @ 21, token0 @ 13, token1 @ 14. Velodrome
/// V2 shares Aerodrome's contract, so the same layout applies. `new(..)` callers
/// supply their own layout for other forks.
#[cfg(feature = "solidly-v2")]
const SOLIDLY_AERODROME_LAYOUT: SolidlyStorageLayout = SolidlyStorageLayout::new(
    U256::from_limbs([20, 0, 0, 0]),
    U256::from_limbs([21, 0, 0, 0]),
    U256::from_limbs([13, 0, 0, 0]),
    U256::from_limbs([14, 0, 0, 0]),
);

/// Declarative configuration for a Solidly V2 (Aerodrome / Velodrome V2) factory.
///
/// Solidly pools are discovered by a [`DerivedSlot`] read of the factory's
/// `getPool[token0][token1][bool stable]` nested mapping (see
/// [`derive::solidly_get_pool_slot`]) — each pair yields up to TWO pools, a
/// stable and a volatile one. Discovery does NOT resolve the swap fee (Solidly
/// pools self-quote via `getAmountOut`, and the fee is a factory `getFee` read at
/// sim time), so there is no quoter to wire.
///
/// Construct with [`new`](Self::new) or a fork preset ([`aerodrome`](Self::aerodrome),
/// [`velodrome`](Self::velodrome)), then refine with the `with_*` builders.
///
/// [`DerivedSlot`]: crate::adapters::factory
#[cfg(feature = "solidly-v2")]
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SolidlyFactoryConfig {
    /// The `PoolFactory` contract address.
    pub factory: Address,
    /// Base slot of the `getPool[t0][t1][stable]` nested mapping.
    pub get_pool_base_slot: U256,
    /// The fork's reserve/token storage layout, attached to every discovered
    /// pool so cold-start can warm the right slots.
    pub storage_layout: SolidlyStorageLayout,
    /// Optional CREATE2 cross-check of the mapping answer. Reuses [`ClCreate2`]
    /// (`deployer` + `init_code_hash`); the salt scheme is Solidly-specific (see
    /// [`derive::solidly_pool_address`]).
    #[cfg(feature = "uniswap-v3")]
    pub create2: Option<ClCreate2>,
    /// Optional CREATE2 cross-check of the mapping answer (deployer +
    /// init-code hash); the salt scheme is Solidly-specific (see
    /// [`derive::solidly_pool_address`]).
    ///
    /// Mirrors the `uniswap-v3`-gated [`ClCreate2`] shape so the Solidly cfg
    /// compiles in isolation (without `uniswap-v3`).
    #[cfg(not(feature = "uniswap-v3"))]
    pub create2: Option<SolidlyCreate2>,
    /// Push-discovery creation-event `topic0`. `None` => pull-only (no creation
    /// sources, `decode_creation` returns `Ok(None)`).
    pub creation_topic0: Option<B256>,
    /// Whether to run the CREATE2 cross-check (requires [`create2`](Self::create2)).
    pub verify_derivations: bool,
}

/// Optional CREATE2 cross-check parameters for a Solidly V2 factory when the
/// `uniswap-v3` feature (which owns [`ClCreate2`]) is not enabled.
///
/// Field-identical to [`ClCreate2`]; exists only so the Solidly discovery path
/// compiles in isolation. When `uniswap-v3` is on, [`SolidlyFactoryConfig`]
/// reuses [`ClCreate2`] directly as the spec requires.
#[cfg(all(feature = "solidly-v2", not(feature = "uniswap-v3")))]
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SolidlyCreate2 {
    /// The CREATE2 deployer. `None` means "the factory itself".
    pub deployer: Option<Address>,
    /// The pool init-code hash.
    pub init_code_hash: B256,
}

/// The CREATE2 cross-check type [`SolidlyFactoryConfig`] carries: [`ClCreate2`]
/// when `uniswap-v3` is enabled (as the spec requires), else the standalone
/// [`SolidlyCreate2`] mirror so the Solidly cfg builds in isolation.
#[cfg(all(feature = "solidly-v2", feature = "uniswap-v3"))]
type SolidlyCreate2Params = ClCreate2;
#[cfg(all(feature = "solidly-v2", not(feature = "uniswap-v3")))]
type SolidlyCreate2Params = SolidlyCreate2;

#[cfg(feature = "solidly-v2")]
impl SolidlyFactoryConfig {
    /// A Solidly V2 factory with an explicit `getPool` base slot and storage
    /// layout. `create2` is `None` and `verify_derivations` is off; add a
    /// cross-check with [`with_create2`](Self::with_create2).
    pub fn new(
        factory: Address,
        get_pool_base_slot: U256,
        storage_layout: SolidlyStorageLayout,
    ) -> Self {
        Self {
            factory,
            get_pool_base_slot,
            storage_layout,
            create2: None,
            creation_topic0: None,
            verify_derivations: false,
        }
    }

    /// Aerodrome (Base) preset.
    ///
    /// The `getPool` base slot (5) and pool storage layout are VERIFIED on-chain
    /// (Base) via trace-based slot discovery; the gated `discovery_solidly_rpc`
    /// parity test re-checks them against the live factory. No CREATE2 init-code
    /// hash is pinned (Solidly's salt is packed), so `verify_derivations` stays OFF.
    pub fn aerodrome(factory: Address) -> Self {
        Self::new(
            factory,
            SOLIDLY_GET_POOL_BASE_SLOT,
            SOLIDLY_AERODROME_LAYOUT,
        )
        .with_creation_topic0(SolidlyPoolCreated::SIGNATURE_HASH)
    }

    /// Velodrome (Optimism) preset. Byte-identical Solidly-V2 shape to Aerodrome;
    /// it reuses Aerodrome's base slot + layout and `PoolCreated` push topic.
    /// Those constants are confirmed on Base for Aerodrome but are NOT yet
    /// verified for Velodrome on Optimism — treat them as provisional and run the
    /// gated parity check against an Optimism endpoint before relying on this
    /// preset in production.
    pub fn velodrome(factory: Address) -> Self {
        Self::aerodrome(factory)
    }

    /// Set the CREATE2 cross-check (`deployer: None` => the factory itself).
    pub fn with_create2(mut self, deployer: Option<Address>, init_code_hash: B256) -> Self {
        self.create2 = Some(SolidlyCreate2Params {
            deployer,
            init_code_hash,
        });
        self
    }

    /// Toggle the CREATE2 derivation cross-check (needs [`with_create2`](Self::with_create2)).
    pub fn with_verify_derivations(mut self, verify: bool) -> Self {
        self.verify_derivations = verify;
        self
    }

    /// Set the push-discovery creation-event `topic0`.
    pub fn with_creation_topic0(mut self, topic0: B256) -> Self {
        self.creation_topic0 = Some(topic0);
        self
    }
}

/// Source of a discovered pool.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DiscoverySource {
    /// Resolved by a factory-storage / view query (a [`PoolQuery`]).
    Query,
    /// Decoded from a factory creation log.
    CreationEvent {
        /// The creation log's block number, if known.
        block_number: Option<u64>,
        /// The creation log's index within the block, if known.
        log_index: Option<u64>,
    },
}

/// Context supplied alongside a factory creation log.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CreationLogContext {
    /// The log's block number, if known.
    pub block_number: Option<u64>,
    /// The log's index within the block, if known.
    pub log_index: Option<u64>,
}

impl CreationLogContext {
    /// A context from an optional block number and log index.
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
    /// The discovered pool's key.
    pub key: PoolKey,
    /// A cold-start-ready registration for the pool.
    pub registration: PoolRegistration,
    /// How the pool was discovered.
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
    /// A `.on(protocol)`-scoped query named a protocol with no configured factory.
    MissingFactory(ProtocolId),
    /// The factory query (storage read / view call) failed.
    Factory(Box<dyn std::error::Error + Send + Sync + 'static>),
    /// A factory response could not be decoded.
    Malformed(&'static str),
    /// The factory mapping answer disagrees with the CREATE2 derivation.
    DerivationMismatch {
        /// The pool address the factory mapping returned.
        mapping: Address,
        /// The pool address derived from CREATE2 (init-code hash + salt).
        derived: Address,
    },
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
    /// The protocol whose pools this factory resolves.
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

    /// The log sources (factory address + creation topic) to subscribe for
    /// live pool-creation discovery.
    fn creation_sources(&self) -> Vec<EventSource>;

    /// Decode a factory creation log into a discovered pool, or `None` if the
    /// log is not a creation event this factory handles.
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
    /// A discovery front-end over an explicit set of factory drivers
    /// (de-duplicated by `(protocol, factory_address)`, insertion order kept).
    pub fn new(factories: impl IntoIterator<Item = Box<dyn PoolFactory>>) -> Self {
        let mut discovery = Self {
            factories: Vec::new(),
        };
        for factory in factories {
            discovery.push_unique(factory);
        }
        discovery
    }

    /// Build discovery by fanning `config` out to every registered adapter's
    /// `pool_factories`, collecting the resulting drivers.
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

    /// The union of every registered factory's creation-log sources, for live
    /// pool-creation discovery.
    pub fn creation_sources(&self) -> Vec<EventSource> {
        self.factories
            .iter()
            .flat_map(|factory| factory.creation_sources())
            .collect()
    }

    /// Decode a factory creation `log` into a discovered pool by trying each
    /// registered factory; `Ok(None)` if none handled it.
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

/// Data-driven concentrated-liquidity (UniV3-mechanics) factory driver.
///
/// One [`ClFactorySpec`] configures the whole family: fee-keyed forks (Uniswap
/// V3 / SushiSwap V3 / PancakeSwap V3) and tickSpacing-keyed forks (Slipstream /
/// Aerodrome). Replaces the former Uniswap-V3-only factory.
#[cfg(feature = "uniswap-v3")]
#[derive(Debug)]
pub struct ConcentratedLiquidityFactory {
    spec: ClFactorySpec,
    derivation_verified: std::sync::OnceLock<()>,
}

/// The innermost `getPool` key for one probe: a fee tier (fee keying) or an int24
/// tick spacing (tickSpacing keying).
#[cfg(feature = "uniswap-v3")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ClLookupKey {
    Fee(u32),
    TickSpacing(i32),
}

#[cfg(feature = "uniswap-v3")]
impl ConcentratedLiquidityFactory {
    /// Build a driver from a fully-specified [`ClFactorySpec`].
    pub fn new(spec: ClFactorySpec) -> Self {
        Self {
            spec,
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
        let storage_layout = match self.spec.protocol {
            ProtocolId::UniswapV3 => V3StorageLayout::uniswap(tick_spacing),
            ProtocolId::PancakeV3 => V3StorageLayout::pancake(tick_spacing),
            ProtocolId::Slipstream => V3StorageLayout::slipstream(tick_spacing),
            _ => V3StorageLayout::uniswap(tick_spacing),
        };
        let mut metadata = V3Metadata::default()
            .with_token0(token0)
            .with_token1(token1)
            .with_tick_spacing(tick_spacing)
            .with_storage_layout(storage_layout)
            .with_factory(self.spec.factory);
        // A resolved fee of 0 is the tickSpacing-keyed "no fee mapping" sentinel
        // (Slipstream / Aerodrome CL have no on-chain fee→pool mapping and set
        // `FeeSource::Fixed(0)`): leave `fee` UNSET rather than record a bogus 0,
        // so `simulate_swap` surfaces `MissingMetadata("V3 fee")` — Slipstream is
        // discovery-only for quoting — instead of silently quoting at fee 0.
        // Fee-keyed forks always resolve a real, non-zero tier.
        if fee != 0 {
            metadata = metadata.with_fee(fee);
        }
        let metadata = if let Some(quoter) = self.spec.quoter {
            metadata.with_quoter(quoter)
        } else {
            metadata
        };
        let key = match self.spec.protocol {
            ProtocolId::PancakeV3 => PoolKey::PancakeV3(pool),
            ProtocolId::Slipstream => PoolKey::Slipstream(pool),
            _ => PoolKey::UniswapV3(pool),
        };
        let protocol_metadata = match self.spec.protocol {
            ProtocolId::PancakeV3 => ProtocolMetadata::PancakeV3(metadata),
            ProtocolId::Slipstream => ProtocolMetadata::Slipstream(metadata),
            _ => ProtocolMetadata::UniswapV3(metadata),
        };
        let registration = PoolRegistration::new(key)
            .with_state_address(pool)
            .with_metadata(protocol_metadata);
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
        key: ClLookupKey,
    ) -> Result<(), DiscoveryError> {
        if !self.spec.verify_derivations || self.derivation_verified.get().is_some() {
            return Ok(());
        }
        let Some(create2) = self.spec.create2 else {
            return Ok(());
        };
        let deployer = create2.deployer.unwrap_or(self.spec.factory);
        let derived = match key {
            ClLookupKey::Fee(fee) => {
                derive::v3_pool_address(deployer, create2.init_code_hash, token0, token1, fee)
            }
            ClLookupKey::TickSpacing(spacing) => derive::v3_pool_address_by_spacing(
                deployer,
                create2.init_code_hash,
                token0,
                token1,
                spacing,
            ),
        };
        if derived != mapping {
            return Err(DiscoveryError::DerivationMismatch { mapping, derived });
        }
        let _ = self.derivation_verified.set(());
        Ok(())
    }

    fn fee_for_key(
        &self,
        key: ClLookupKey,
        values: &HashMap<(Address, U256), U256>,
    ) -> Result<u32, DiscoveryError> {
        match self.spec.fee_source {
            FeeSource::Key => match key {
                ClLookupKey::Fee(fee) => Ok(fee),
                ClLookupKey::TickSpacing(_) => Err(DiscoveryError::Malformed(
                    "fee source Key is not valid for tickSpacing-keyed factories",
                )),
            },
            FeeSource::Fixed(fee) => Ok(fee),
            FeeSource::FactoryMapping { base_slot } => {
                let slot = match key {
                    ClLookupKey::Fee(fee) => {
                        derive::v3_fee_amount_tick_spacing_slot(base_slot, fee)
                    }
                    ClLookupKey::TickSpacing(spacing) => {
                        derive::v3_tick_spacing_fee_slot(base_slot, spacing)
                    }
                };
                let word = values
                    .get(&(self.spec.factory, slot))
                    .copied()
                    .unwrap_or_default();
                Ok(u32_from_word(word))
            }
        }
    }
}

#[cfg(feature = "uniswap-v3")]
impl PoolFactory for ConcentratedLiquidityFactory {
    fn protocol(&self) -> ProtocolId {
        self.spec.protocol
    }

    fn factory_address(&self) -> Address {
        self.spec.factory
    }

    fn candidate_reads(&self, pairs: &[(Address, Address)]) -> Vec<(Address, U256)> {
        let mut slots = Vec::new();
        match &self.spec.keying {
            ClKeying::Fee {
                tiers,
                fee_amount_tick_spacing_base_slot,
            } => {
                // `feeAmountTickSpacing[fee]` is per-fee, not per-pair — read each once.
                for &fee in tiers {
                    slots.push((
                        self.spec.factory,
                        derive::v3_fee_amount_tick_spacing_slot(
                            *fee_amount_tick_spacing_base_slot,
                            fee,
                        ),
                    ));
                }
                for (token0, token1) in pairs {
                    for &fee in tiers {
                        slots.push((
                            self.spec.factory,
                            derive::v3_get_pool_slot(
                                self.spec.get_pool_base_slot,
                                *token0,
                                *token1,
                                fee,
                            ),
                        ));
                    }
                }
            }
            ClKeying::TickSpacing { spacings } => {
                for (token0, token1) in pairs {
                    for &spacing in spacings {
                        slots.push((
                            self.spec.factory,
                            derive::v3_get_pool_slot_by_spacing(
                                self.spec.get_pool_base_slot,
                                *token0,
                                *token1,
                                spacing,
                            ),
                        ));
                        if let FeeSource::FactoryMapping { base_slot } = self.spec.fee_source {
                            slots.push((
                                self.spec.factory,
                                derive::v3_tick_spacing_fee_slot(base_slot, spacing),
                            ));
                        }
                    }
                }
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
            match &self.spec.keying {
                ClKeying::Fee {
                    tiers,
                    fee_amount_tick_spacing_base_slot,
                } => {
                    for &fee in tiers {
                        let key = ClLookupKey::Fee(fee);
                        let pool_slot = derive::v3_get_pool_slot(
                            self.spec.get_pool_base_slot,
                            *token0,
                            *token1,
                            fee,
                        );
                        let Some(&word) = values.get(&(self.spec.factory, pool_slot)) else {
                            continue;
                        };
                        let pool = address_from_word(word)?;
                        if pool == Address::ZERO {
                            continue;
                        }
                        self.ensure_derivation_matches(pool, *token0, *token1, key)?;
                        let tick_slot = derive::v3_fee_amount_tick_spacing_slot(
                            *fee_amount_tick_spacing_base_slot,
                            fee,
                        );
                        let tick_spacing = i24_from_word(
                            values
                                .get(&(self.spec.factory, tick_slot))
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
                            self.fee_for_key(key, values)?,
                            tick_spacing,
                            DiscoverySource::Query,
                        ));
                    }
                }
                ClKeying::TickSpacing { spacings } => {
                    for &spacing in spacings {
                        let key = ClLookupKey::TickSpacing(spacing);
                        let pool_slot = derive::v3_get_pool_slot_by_spacing(
                            self.spec.get_pool_base_slot,
                            *token0,
                            *token1,
                            spacing,
                        );
                        let Some(&word) = values.get(&(self.spec.factory, pool_slot)) else {
                            continue;
                        };
                        let pool = address_from_word(word)?;
                        if pool == Address::ZERO {
                            continue;
                        }
                        self.ensure_derivation_matches(pool, *token0, *token1, key)?;
                        found.push(self.registration(
                            pool,
                            *token0,
                            *token1,
                            self.fee_for_key(key, values)?,
                            spacing,
                            DiscoverySource::Query,
                        ));
                    }
                }
            }
        }
        Ok(found)
    }

    fn creation_sources(&self) -> Vec<EventSource> {
        self.spec
            .creation_topic0
            .map(|topic| EventSource::adapter_defined(self.spec.factory, vec![topic]))
            .into_iter()
            .collect()
    }

    fn decode_creation(
        &self,
        log: &Log,
        context: CreationLogContext,
    ) -> Result<Option<DiscoveredPool>, DiscoveryError> {
        let Some(topic0) = self.spec.creation_topic0 else {
            return Ok(None);
        };
        if log.address != self.spec.factory || log.topics().first() != Some(&topic0) {
            return Ok(None);
        }
        if topic0 == PoolCreated::SIGNATURE_HASH {
            let event = PoolCreated::decode_log(log)
                .map_err(|_| DiscoveryError::Malformed("PoolCreated failed to decode"))?
                .data;
            let fee: u32 = event.fee.to();
            let tick_spacing: i32 = event.tickSpacing.as_i32();
            self.ensure_derivation_matches(
                event.pool,
                event.token0,
                event.token1,
                ClLookupKey::Fee(fee),
            )?;
            return Ok(Some(self.registration(
                event.pool,
                event.token0,
                event.token1,
                fee,
                tick_spacing,
                DiscoverySource::CreationEvent {
                    block_number: context.block_number,
                    log_index: context.log_index,
                },
            )));
        }
        if topic0 == PoolCreatedTickSpacing::SIGNATURE_HASH {
            let event = PoolCreatedTickSpacing::decode_log(log)
                .map_err(|_| DiscoveryError::Malformed("PoolCreatedTickSpacing failed to decode"))?
                .data;
            let fee: u32 = event.fee.to();
            let tick_spacing: i32 = event.tickSpacing.as_i32();
            self.ensure_derivation_matches(
                event.pool,
                event.token0,
                event.token1,
                ClLookupKey::TickSpacing(tick_spacing),
            )?;
            return Ok(Some(self.registration(
                event.pool,
                event.token0,
                event.token1,
                fee,
                tick_spacing,
                DiscoverySource::CreationEvent {
                    block_number: context.block_number,
                    log_index: context.log_index,
                },
            )));
        }
        Ok(None)
    }
}

/// Solidly V2 (Aerodrome / Velodrome V2) factory driver.
///
/// Resolves pools by a batched [`DerivedSlot`] read of the factory's
/// `getPool[token0][token1][bool stable]` nested mapping: each pair contributes
/// two candidate slots (volatile + stable), and a non-zero mapping answer yields
/// a [`PoolKey::SolidlyV2`] pool carrying the fork's [`SolidlyStorageLayout`]. No
/// quoter is wired — Solidly pools self-quote (`getAmountOut`) and their fee is a
/// factory read at sim time — so discovery resolves only the pool set, not fees.
///
/// [`DerivedSlot`]: crate::adapters::factory
#[cfg(feature = "solidly-v2")]
#[derive(Debug)]
pub struct SolidlyFactory {
    config: SolidlyFactoryConfig,
    derivation_verified: std::sync::OnceLock<()>,
}

/// The two Solidly pool variants a pair can resolve to.
#[cfg(feature = "solidly-v2")]
const SOLIDLY_VARIANTS: [bool; 2] = [false, true];

#[cfg(feature = "solidly-v2")]
impl SolidlyFactory {
    /// Build a driver from a fully-specified [`SolidlyFactoryConfig`].
    pub fn new(config: SolidlyFactoryConfig) -> Self {
        Self {
            config,
            derivation_verified: std::sync::OnceLock::new(),
        }
    }

    fn registration(
        &self,
        pool: Address,
        token0: Address,
        token1: Address,
        stable: bool,
        source: DiscoverySource,
    ) -> DiscoveredPool {
        let metadata = SolidlyV2Metadata::default()
            .with_token0(token0)
            .with_token1(token1)
            .with_stable(stable)
            .with_storage_layout(self.config.storage_layout);
        let registration = PoolRegistration::new(PoolKey::SolidlyV2(pool))
            .with_state_address(pool)
            .with_metadata(ProtocolMetadata::SolidlyV2(metadata));
        let adapter = crate::adapters::SolidlyV2Adapter::default();
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
        stable: bool,
    ) -> Result<(), DiscoveryError> {
        if !self.config.verify_derivations || self.derivation_verified.get().is_some() {
            return Ok(());
        }
        let Some(create2) = self.config.create2 else {
            return Ok(());
        };
        let deployer = create2.deployer.unwrap_or(self.config.factory);
        let derived =
            derive::solidly_pool_address(deployer, create2.init_code_hash, token0, token1, stable);
        if derived != mapping {
            return Err(DiscoveryError::DerivationMismatch { mapping, derived });
        }
        let _ = self.derivation_verified.set(());
        Ok(())
    }
}

#[cfg(feature = "solidly-v2")]
impl PoolFactory for SolidlyFactory {
    fn protocol(&self) -> ProtocolId {
        ProtocolId::SolidlyV2
    }

    fn factory_address(&self) -> Address {
        self.config.factory
    }

    fn candidate_reads(&self, pairs: &[(Address, Address)]) -> Vec<(Address, U256)> {
        let mut slots = Vec::with_capacity(pairs.len() * SOLIDLY_VARIANTS.len());
        for (token0, token1) in pairs {
            for stable in SOLIDLY_VARIANTS {
                slots.push((
                    self.config.factory,
                    derive::solidly_get_pool_slot(
                        self.config.get_pool_base_slot,
                        *token0,
                        *token1,
                        stable,
                    ),
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
            for stable in SOLIDLY_VARIANTS {
                let slot = derive::solidly_get_pool_slot(
                    self.config.get_pool_base_slot,
                    *token0,
                    *token1,
                    stable,
                );
                let Some(&word) = values.get(&(self.config.factory, slot)) else {
                    continue;
                };
                let pool = address_from_word(word)?;
                if pool == Address::ZERO {
                    continue;
                }
                self.ensure_derivation_matches(pool, *token0, *token1, stable)?;
                found.push(self.registration(
                    pool,
                    *token0,
                    *token1,
                    stable,
                    DiscoverySource::Query,
                ));
            }
        }
        Ok(found)
    }

    fn creation_sources(&self) -> Vec<EventSource> {
        self.config
            .creation_topic0
            .map(|topic| EventSource::adapter_defined(self.config.factory, vec![topic]))
            .into_iter()
            .collect()
    }

    fn decode_creation(
        &self,
        log: &Log,
        context: CreationLogContext,
    ) -> Result<Option<DiscoveredPool>, DiscoveryError> {
        let Some(topic0) = self.config.creation_topic0 else {
            return Ok(None);
        };
        if log.address != self.config.factory || log.topics().first() != Some(&topic0) {
            return Ok(None);
        }
        if topic0 != SolidlyPoolCreated::SIGNATURE_HASH {
            return Ok(None);
        }
        let event = SolidlyPoolCreated::decode_log(log)
            .map_err(|_| DiscoveryError::Malformed("SolidlyPoolCreated failed to decode"))?
            .data;
        self.ensure_derivation_matches(event.pool, event.token0, event.token1, event.stable)?;
        Ok(Some(self.registration(
            event.pool,
            event.token0,
            event.token1,
            event.stable,
            DiscoverySource::CreationEvent {
                block_number: context.block_number,
                log_index: context.log_index,
            },
        )))
    }
}

#[cfg(feature = "uniswap-v3")]
fn u32_from_word(word: U256) -> u32 {
    let bytes = word.to_be_bytes::<32>();
    u32::from_be_bytes([bytes[28], bytes[29], bytes[30], bytes[31]])
}

#[cfg(any(feature = "uniswap-v2", feature = "uniswap-v3", feature = "solidly-v2"))]
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

#[cfg(all(test, feature = "solidly-v2"))]
mod solidly_tests {
    use super::*;
    use alloy_primitives::keccak256;

    /// The Solidly `PoolCreated` push topic0 must be the REAL on-chain event
    /// signature `keccak256("PoolCreated(address,address,bool,address,uint256)")`
    /// — NOT the Rust alias name. This guards the module-wrapping that keeps the
    /// generated struct named `PoolCreated` (renaming it silently produces the
    /// wrong topic0, so real creation logs would never match).
    #[test]
    fn solidly_pool_created_topic0_matches_onchain_signature() {
        let expected = keccak256(b"PoolCreated(address,address,bool,address,uint256)");
        assert_eq!(SolidlyPoolCreated::SIGNATURE_HASH, expected);
        assert_eq!(
            SolidlyPoolCreated::SIGNATURE,
            "PoolCreated(address,address,bool,address,uint256)"
        );
    }

    /// `solidly_get_pool_slot` descends the two address levels exactly like the
    /// V3 `getPool[t0][t1]` mapping, then keys the innermost `bool` as 0/1 — so
    /// the stable and volatile variants land on distinct, non-zero slots.
    #[test]
    fn solidly_get_pool_slot_distinguishes_stable_flag() {
        let base = U256::from(3);
        let t0 = Address::repeat_byte(0x0a);
        let t1 = Address::repeat_byte(0x0b);
        let volatile = derive::solidly_get_pool_slot(base, t0, t1, false);
        let stable = derive::solidly_get_pool_slot(base, t0, t1, true);
        assert_ne!(volatile, stable);
        assert_ne!(volatile, U256::ZERO);
        assert_ne!(stable, U256::ZERO);
    }
}
