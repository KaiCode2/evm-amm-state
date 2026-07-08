//! Runtime-bytecode seed artifacts for AMM pool contracts.
//!
//! `evm-fork-cache` calls this operation "seeding" when the bytecode is a
//! canonical on-chain claim to be verified: the plain functions here return
//! `seed_account_code` inputs, not local-divergence `etch_account_code` inputs.

use std::fmt;
use std::sync::LazyLock;

use alloy_primitives::{Address, B256, Bytes, U256};
use revm::state::Bytecode;

use super::types::V3Metadata;

const UNISWAP_V3_MIN_TICK: i32 = -887272;
const UNISWAP_V3_MAX_TICK: i32 = 887272;

const UNISWAP_V2_PAIR_RUNTIME_BYTES: &[u8] =
    include_bytes!("bytecodes/uniswap_v2_pair_runtime.bin");
const UNISWAP_V3_POOL_TEMPLATE_BYTES: &[u8] =
    include_bytes!("bytecodes/uniswap_v3_pool_template.bin");

static UNISWAP_V2_PAIR_RUNTIME: LazyLock<Bytes> =
    LazyLock::new(|| Bytes::from_static(UNISWAP_V2_PAIR_RUNTIME_BYTES));
static UNISWAP_V2_PAIR_RUNTIME_CODE_HASH: LazyLock<B256> =
    LazyLock::new(|| runtime_code_hash(&UNISWAP_V2_PAIR_RUNTIME));
static UNISWAP_V3_POOL_TEMPLATE: LazyLock<V3RuntimeBytecodeTemplate> = LazyLock::new(|| {
    V3RuntimeBytecodeTemplate::new(
        Bytes::from_static(UNISWAP_V3_POOL_TEMPLATE_BYTES),
        UNISWAP_V3_IMMUTABLE_PATCHES,
    )
});

// The byte offsets below are the immutable-word locations in the canonical
// mainnet Uniswap V3 pool runtime build (the `UNISWAP_V3_POOL_TEMPLATE_BYTES`
// artifact with those words zeroed). `tests/bytecode_golden.rs` pins them to
// chain-truth `EXTCODEHASH` values across three tickSpacings / two token pairs,
// so a wrong offset cannot pass by coincidence. If the compiler build ever
// changes, regenerate them by diffing two deployed pools' runtime bytecode.
const V3_POOL_ADDRESS_PATCHES: [BytecodePatch; 1] = [BytecodePatch::new(11259, 32)];
const V3_FACTORY_PATCHES: [BytecodePatch; 3] = [
    BytecodePatch::new(8315, 32),
    BytecodePatch::new(8829, 32),
    BytecodePatch::new(10457, 32),
];
const V3_TOKEN0_PATCHES: [BytecodePatch; 6] = [
    BytecodePatch::new(2258, 32),
    BytecodePatch::new(4853, 32),
    BytecodePatch::new(6740, 32),
    BytecodePatch::new(7822, 32),
    BytecodePatch::new(9150, 32),
    BytecodePatch::new(15650, 32),
];
const V3_TOKEN1_PATCHES: [BytecodePatch; 6] = [
    BytecodePatch::new(4551, 32),
    BytecodePatch::new(6789, 32),
    BytecodePatch::new(7924, 32),
    BytecodePatch::new(9284, 32),
    BytecodePatch::new(10529, 32),
    BytecodePatch::new(15979, 32),
];
const V3_FEE_PATCHES: [BytecodePatch; 4] = [
    BytecodePatch::new(3311, 32),
    BytecodePatch::new(6603, 32),
    BytecodePatch::new(6658, 32),
    BytecodePatch::new(10565, 32),
];
const V3_TICK_SPACING_PATCHES: [BytecodePatch; 4] = [
    BytecodePatch::new(3072, 32),
    BytecodePatch::new(10493, 32),
    BytecodePatch::new(19402, 32),
    BytecodePatch::new(19452, 32),
];
const V3_MAX_LIQUIDITY_PER_TICK_PATCHES: [BytecodePatch; 3] = [
    BytecodePatch::new(8174, 32),
    BytecodePatch::new(19295, 32),
    BytecodePatch::new(19350, 32),
];
const UNISWAP_V3_IMMUTABLE_PATCHES: V3ImmutablePatches = V3ImmutablePatches {
    pool_address: &V3_POOL_ADDRESS_PATCHES,
    factory: &V3_FACTORY_PATCHES,
    token0: &V3_TOKEN0_PATCHES,
    token1: &V3_TOKEN1_PATCHES,
    fee: &V3_FEE_PATCHES,
    tick_spacing: &V3_TICK_SPACING_PATCHES,
    max_liquidity_per_tick: &V3_MAX_LIQUIDITY_PER_TICK_PATCHES,
};

/// Runtime bytecode to seed for one on-chain account before cold-start.
///
/// `#[non_exhaustive]`: Construct via [`AdapterCodeSeed::new`] or
/// [`AdapterCodeSeed::with_code_hash`].
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdapterCodeSeed {
    /// Account whose canonical runtime bytecode is being seeded.
    pub address: Address,
    /// Expected deployed runtime bytecode for [`address`](Self::address).
    pub runtime_bytecode: Bytes,
    /// Keccak256 hash of [`runtime_bytecode`](Self::runtime_bytecode).
    pub code_hash: B256,
}

impl AdapterCodeSeed {
    /// A seed for `address`, computing the code hash from `runtime_bytecode`.
    pub fn new(address: Address, runtime_bytecode: impl Into<Bytes>) -> Self {
        let runtime_bytecode = runtime_bytecode.into();
        let code_hash = runtime_code_hash(&runtime_bytecode);
        Self {
            address,
            runtime_bytecode,
            code_hash,
        }
    }

    /// A seed with a precomputed `code_hash` (skips re-hashing the bytecode).
    pub fn with_code_hash(
        address: Address,
        runtime_bytecode: impl Into<Bytes>,
        code_hash: B256,
    ) -> Self {
        Self {
            address,
            runtime_bytecode: runtime_bytecode.into(),
            code_hash,
        }
    }
}

/// Canonical runtime-bytecode seed for a Uniswap V2 pair at `address`.
///
/// All V2 pairs share one runtime (the immutables live in storage, not code),
/// so the seed is the embedded pair runtime plus its precomputed code hash —
/// no per-pool metadata is needed.
pub fn uniswap_v2_pair_code_seed(address: Address) -> AdapterCodeSeed {
    AdapterCodeSeed::with_code_hash(
        address,
        uniswap_v2_pair_runtime_bytecode(),
        uniswap_v2_pair_runtime_code_hash(),
    )
}

/// Render a canonical Uniswap V3 pool runtime-bytecode seed for `address` from
/// its [`V3Metadata`], if the metadata carries enough immutables to derive it.
///
/// Returns `Ok(None)` when `factory` / `token0` / `token1` / `fee` /
/// `tick_spacing` (falling back to the `storage_layout` tick spacing) are not
/// all known — such a pool is simply not seedable, not an error. Returns
/// `Err(BytecodeTemplateError)` only if rendering the template itself fails.
pub fn v3_code_seed_from_metadata(
    address: Address,
    metadata: &V3Metadata,
) -> Result<Option<AdapterCodeSeed>, BytecodeTemplateError> {
    let Some(factory) = metadata.factory else {
        return Ok(None);
    };
    let Some(token0) = metadata.token0 else {
        return Ok(None);
    };
    let Some(token1) = metadata.token1 else {
        return Ok(None);
    };
    let Some(fee) = metadata.fee else {
        return Ok(None);
    };
    let Some(tick_spacing) = metadata
        .tick_spacing
        .or_else(|| metadata.storage_layout.map(|layout| layout.tick_spacing))
    else {
        return Ok(None);
    };
    uniswap_v3_code_seed(
        address,
        &V3ImmutablePatchValues {
            pool_address: Some(address),
            factory: Some(factory),
            token0: Some(token0),
            token1: Some(token1),
            fee: Some(fee),
            tick_spacing: Some(tick_spacing),
            max_liquidity_per_tick: uniswap_v3_max_liquidity_per_tick(tick_spacing),
        },
    )
    .map(Some)
}

/// Render a Uniswap V3 runtime bytecode seed from explicit immutable values.
pub fn uniswap_v3_code_seed(
    address: Address,
    values: &V3ImmutablePatchValues,
) -> Result<AdapterCodeSeed, BytecodeTemplateError> {
    let runtime = uniswap_v3_pool_template().render(values)?;
    Ok(AdapterCodeSeed::new(address, runtime))
}

/// Embedded canonical Uniswap V2 pair runtime bytecode.
pub fn uniswap_v2_pair_runtime_bytecode() -> Bytes {
    UNISWAP_V2_PAIR_RUNTIME.clone()
}

/// Keccak256 hash of the embedded canonical Uniswap V2 pair runtime bytecode.
pub fn uniswap_v2_pair_runtime_code_hash() -> B256 {
    *UNISWAP_V2_PAIR_RUNTIME_CODE_HASH
}

/// Embedded Uniswap V3 pool runtime template with immutable words zeroed.
pub fn uniswap_v3_pool_template() -> V3RuntimeBytecodeTemplate {
    UNISWAP_V3_POOL_TEMPLATE.clone()
}

/// The Uniswap V3 `Tick.tickSpacingToMaxLiquidityPerTick` formula.
pub fn uniswap_v3_max_liquidity_per_tick(tick_spacing: i32) -> Option<U256> {
    if tick_spacing <= 0 {
        return None;
    }
    let tick_spacing = tick_spacing as i128;
    let min_tick = (UNISWAP_V3_MIN_TICK as i128 / tick_spacing) * tick_spacing;
    let max_tick = (UNISWAP_V3_MAX_TICK as i128 / tick_spacing) * tick_spacing;
    let tick_count = ((max_tick - min_tick) / tick_spacing) + 1;
    if tick_count <= 0 {
        return None;
    }
    Some(U256::from(u128::MAX) / U256::from(tick_count as u128))
}

fn runtime_code_hash(runtime_bytecode: &Bytes) -> B256 {
    Bytecode::new_raw(runtime_bytecode.clone()).hash_slow()
}

/// A byte range in deployed runtime bytecode occupied by one immutable value.
///
/// `#[non_exhaustive]`: Construct via [`BytecodePatch::new`] (`const`, so `static` patch tables
/// keep working).
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BytecodePatch {
    /// Byte offset into the runtime bytecode.
    pub offset: usize,
    /// Number of bytes occupied by the immutable value.
    pub length: usize,
}

impl BytecodePatch {
    /// A patch covering `length` bytes at `offset` in the runtime bytecode.
    pub const fn new(offset: usize, length: usize) -> Self {
        Self { offset, length }
    }
}

/// Immutable byte ranges for a V3-style pool runtime template.
///
/// Each field lists the byte ranges in the template occupied by that Solidity
/// immutable, patched per-pool at render time.
///
/// `#[non_exhaustive]`: Construct via `Default` plus the `with_*` builders.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct V3ImmutablePatches {
    /// Ranges holding the pool's own address (`NoDelegateCall` self-address).
    pub pool_address: &'static [BytecodePatch],
    /// Ranges holding the factory/deployer address.
    pub factory: &'static [BytecodePatch],
    /// Ranges holding `token0`.
    pub token0: &'static [BytecodePatch],
    /// Ranges holding `token1`.
    pub token1: &'static [BytecodePatch],
    /// Ranges holding the fee.
    pub fee: &'static [BytecodePatch],
    /// Ranges holding the tick spacing.
    pub tick_spacing: &'static [BytecodePatch],
    /// Ranges holding `maxLiquidityPerTick`.
    pub max_liquidity_per_tick: &'static [BytecodePatch],
}

impl V3ImmutablePatches {
    /// Set the pool-address patch ranges.
    pub fn with_pool_address(mut self, patches: &'static [BytecodePatch]) -> Self {
        self.pool_address = patches;
        self
    }

    /// Set the factory patch ranges.
    pub fn with_factory(mut self, patches: &'static [BytecodePatch]) -> Self {
        self.factory = patches;
        self
    }

    /// Set the `token0` patch ranges.
    pub fn with_token0(mut self, patches: &'static [BytecodePatch]) -> Self {
        self.token0 = patches;
        self
    }

    /// Set the `token1` patch ranges.
    pub fn with_token1(mut self, patches: &'static [BytecodePatch]) -> Self {
        self.token1 = patches;
        self
    }

    /// Set the fee patch ranges.
    pub fn with_fee(mut self, patches: &'static [BytecodePatch]) -> Self {
        self.fee = patches;
        self
    }

    /// Set the tick-spacing patch ranges.
    pub fn with_tick_spacing(mut self, patches: &'static [BytecodePatch]) -> Self {
        self.tick_spacing = patches;
        self
    }

    /// Set the `maxLiquidityPerTick` patch ranges.
    pub fn with_max_liquidity_per_tick(mut self, patches: &'static [BytecodePatch]) -> Self {
        self.max_liquidity_per_tick = patches;
        self
    }
}

/// A V3-style pool runtime template plus immutable patch locations.
///
/// `#[non_exhaustive]`: Construct via [`V3RuntimeBytecodeTemplate::new`].
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct V3RuntimeBytecodeTemplate {
    /// Deployed runtime bytecode before per-pool immutable replacement.
    pub runtime_bytecode: Bytes,
    /// Byte ranges to replace with per-pool immutable values.
    pub immutables: V3ImmutablePatches,
}

impl V3RuntimeBytecodeTemplate {
    /// A template from `runtime_bytecode` and its immutable patch locations.
    pub fn new(runtime_bytecode: impl Into<Bytes>, immutables: V3ImmutablePatches) -> Self {
        Self {
            runtime_bytecode: runtime_bytecode.into(),
            immutables,
        }
    }

    /// Render the expected deployed bytecode for one pool by patching immutable
    /// values into the template.
    pub fn render(&self, values: &V3ImmutablePatchValues) -> Result<Bytes, BytecodeTemplateError> {
        let mut bytecode = self.runtime_bytecode.to_vec();

        patch_optional_address(
            &mut bytecode,
            "pool_address",
            self.immutables.pool_address,
            values.pool_address,
        )?;
        patch_optional_address(
            &mut bytecode,
            "factory",
            self.immutables.factory,
            values.factory,
        )?;
        patch_optional_address(
            &mut bytecode,
            "token0",
            self.immutables.token0,
            values.token0,
        )?;
        patch_optional_address(
            &mut bytecode,
            "token1",
            self.immutables.token1,
            values.token1,
        )?;
        patch_optional_u32(&mut bytecode, "fee", self.immutables.fee, values.fee)?;
        patch_optional_i32(
            &mut bytecode,
            "tick_spacing",
            self.immutables.tick_spacing,
            values.tick_spacing,
        )?;
        patch_optional_u256(
            &mut bytecode,
            "max_liquidity_per_tick",
            self.immutables.max_liquidity_per_tick,
            values.max_liquidity_per_tick,
        )?;

        Ok(Bytes::from(bytecode))
    }
}

/// Per-pool immutable values used to render a V3 runtime bytecode template.
///
/// `#[non_exhaustive]`: Construct via `Default` plus the `with_*` builders (fields stay `pub`
/// for direct assignment).
#[non_exhaustive]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct V3ImmutablePatchValues {
    /// The pool's own address.
    pub pool_address: Option<Address>,
    /// The factory/deployer address.
    pub factory: Option<Address>,
    /// The pool's `token0`.
    pub token0: Option<Address>,
    /// The pool's `token1`.
    pub token1: Option<Address>,
    /// The pool fee.
    pub fee: Option<u32>,
    /// The pool's tick spacing.
    pub tick_spacing: Option<i32>,
    /// The pool's `maxLiquidityPerTick`.
    pub max_liquidity_per_tick: Option<U256>,
}

impl V3ImmutablePatchValues {
    /// Set the pool's own address.
    pub fn with_pool_address(mut self, pool_address: Address) -> Self {
        self.pool_address = Some(pool_address);
        self
    }

    /// Set the factory/deployer address.
    pub fn with_factory(mut self, factory: Address) -> Self {
        self.factory = Some(factory);
        self
    }

    /// Set `token0`.
    pub fn with_token0(mut self, token0: Address) -> Self {
        self.token0 = Some(token0);
        self
    }

    /// Set `token1`.
    pub fn with_token1(mut self, token1: Address) -> Self {
        self.token1 = Some(token1);
        self
    }

    /// Set the pool fee.
    pub fn with_fee(mut self, fee: u32) -> Self {
        self.fee = Some(fee);
        self
    }

    /// Set the tick spacing.
    pub fn with_tick_spacing(mut self, tick_spacing: i32) -> Self {
        self.tick_spacing = Some(tick_spacing);
        self
    }

    /// Set `maxLiquidityPerTick`.
    pub fn with_max_liquidity_per_tick(mut self, max_liquidity_per_tick: U256) -> Self {
        self.max_liquidity_per_tick = Some(max_liquidity_per_tick);
        self
    }
}

/// Why rendering a V3 runtime bytecode template failed.
///
/// `#[non_exhaustive]` — an open error vocabulary.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BytecodeTemplateError {
    /// A patch range was declared for `field` but no value was supplied.
    MissingImmutable {
        /// The immutable whose value was missing.
        field: &'static str,
    },
    /// A patch range's `length` cannot hold the immutable's encoded value.
    InvalidPatchLength {
        /// The immutable being patched.
        field: &'static str,
        /// The declared patch length that is too small.
        length: usize,
    },
    /// A patch range falls outside the template bytecode.
    PatchOutOfBounds {
        /// The immutable being patched.
        field: &'static str,
        /// The patch's byte offset.
        offset: usize,
        /// The patch's byte length.
        length: usize,
        /// The template bytecode's length.
        bytecode_len: usize,
    },
}

impl fmt::Display for BytecodeTemplateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingImmutable { field } => {
                write!(f, "missing V3 bytecode immutable `{field}`")
            }
            Self::InvalidPatchLength { field, length } => {
                write!(
                    f,
                    "invalid V3 bytecode patch length for `{field}`: {length}"
                )
            }
            Self::PatchOutOfBounds {
                field,
                offset,
                length,
                bytecode_len,
            } => write!(
                f,
                "V3 bytecode patch for `{field}` is out of bounds: offset {offset}, length {length}, bytecode length {bytecode_len}"
            ),
        }
    }
}

impl std::error::Error for BytecodeTemplateError {}

fn patch_optional_address(
    bytecode: &mut [u8],
    field: &'static str,
    patches: &[BytecodePatch],
    value: Option<Address>,
) -> Result<(), BytecodeTemplateError> {
    if patches.is_empty() {
        return Ok(());
    }
    let value = value.ok_or(BytecodeTemplateError::MissingImmutable { field })?;
    let mut word = [0u8; 32];
    word[12..].copy_from_slice(value.as_slice());
    patch_word(bytecode, field, patches, word)
}

fn patch_optional_u32(
    bytecode: &mut [u8],
    field: &'static str,
    patches: &[BytecodePatch],
    value: Option<u32>,
) -> Result<(), BytecodeTemplateError> {
    if patches.is_empty() {
        return Ok(());
    }
    let value = value.ok_or(BytecodeTemplateError::MissingImmutable { field })?;
    patch_word(
        bytecode,
        field,
        patches,
        U256::from(value).to_be_bytes::<32>(),
    )
}

fn patch_optional_i32(
    bytecode: &mut [u8],
    field: &'static str,
    patches: &[BytecodePatch],
    value: Option<i32>,
) -> Result<(), BytecodeTemplateError> {
    if patches.is_empty() {
        return Ok(());
    }
    let value = value.ok_or(BytecodeTemplateError::MissingImmutable { field })?;
    let mut word = if value < 0 { [0xFF; 32] } else { [0u8; 32] };
    word[28..].copy_from_slice(&value.to_be_bytes());
    patch_word(bytecode, field, patches, word)
}

fn patch_optional_u256(
    bytecode: &mut [u8],
    field: &'static str,
    patches: &[BytecodePatch],
    value: Option<U256>,
) -> Result<(), BytecodeTemplateError> {
    if patches.is_empty() {
        return Ok(());
    }
    let value = value.ok_or(BytecodeTemplateError::MissingImmutable { field })?;
    patch_word(bytecode, field, patches, value.to_be_bytes::<32>())
}

fn patch_word(
    bytecode: &mut [u8],
    field: &'static str,
    patches: &[BytecodePatch],
    word: [u8; 32],
) -> Result<(), BytecodeTemplateError> {
    for patch in patches {
        if patch.length == 0 || patch.length > 32 {
            return Err(BytecodeTemplateError::InvalidPatchLength {
                field,
                length: patch.length,
            });
        }
        let end = patch.offset.checked_add(patch.length).ok_or(
            BytecodeTemplateError::PatchOutOfBounds {
                field,
                offset: patch.offset,
                length: patch.length,
                bytecode_len: bytecode.len(),
            },
        )?;
        if end > bytecode.len() {
            return Err(BytecodeTemplateError::PatchOutOfBounds {
                field,
                offset: patch.offset,
                length: patch.length,
                bytecode_len: bytecode.len(),
            });
        }

        let src = 32 - patch.length;
        bytecode[patch.offset..end].copy_from_slice(&word[src..]);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn computes_uniswap_v3_max_liquidity_per_tick() {
        assert_eq!(
            uniswap_v3_max_liquidity_per_tick(10),
            Some(U256::from(1917569901783203986719870431555990_u128))
        );
        assert_eq!(
            uniswap_v3_max_liquidity_per_tick(60),
            Some(U256::from(11505743598341114571880798222544994_u128))
        );
        assert_eq!(uniswap_v3_max_liquidity_per_tick(0), None);
    }

    #[test]
    fn embedded_artifacts_decode() {
        assert_eq!(uniswap_v2_pair_runtime_bytecode().len(), 11293);
        assert_eq!(uniswap_v3_pool_template().runtime_bytecode.len(), 22142);
    }
}
