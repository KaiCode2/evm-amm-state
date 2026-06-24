//! Protocol storage constants and storage-key helpers.

use alloy_primitives::{Address, U256, keccak256};

use super::types::{PoolRegistration, ProtocolId, ProtocolMetadata, V3Metadata};

/// Storage slot for the Uniswap V2 pair `token0` address.
pub const V2_TOKEN0_SLOT: U256 = U256::from_limbs([6, 0, 0, 0]);

/// Storage slot for the Uniswap V2 pair `token1` address.
pub const V2_TOKEN1_SLOT: U256 = U256::from_limbs([7, 0, 0, 0]);

/// Storage slot for Uniswap V2 pair reserves.
pub const V2_RESERVES_SLOT: U256 = U256::from_limbs([8, 0, 0, 0]);

/// Storage slot for Uniswap V3 `slot0`.
pub const V3_SLOT0_SLOT: U256 = U256::ZERO;

/// Storage slot for Uniswap V3 global liquidity.
pub const V3_LIQUIDITY_SLOT: U256 = U256::from_limbs([4, 0, 0, 0]);

/// Base storage slot for Uniswap V3 `ticks` mapping.
pub const V3_TICKS_BASE_SLOT: U256 = U256::from_limbs([5, 0, 0, 0]);

/// Base storage slot for Uniswap V3 `tickBitmap` mapping.
pub const V3_TICK_BITMAP_BASE_SLOT: U256 = U256::from_limbs([6, 0, 0, 0]);

/// Storage slot for PancakeSwap V3 global liquidity.
pub const PANCAKE_V3_LIQUIDITY_SLOT: U256 = U256::from_limbs([5, 0, 0, 0]);

/// Base storage slot for PancakeSwap V3 `ticks` mapping.
pub const PANCAKE_V3_TICKS_BASE_SLOT: U256 = U256::from_limbs([6, 0, 0, 0]);

/// Base storage slot for PancakeSwap V3 `tickBitmap` mapping.
pub const PANCAKE_V3_TICK_BITMAP_BASE_SLOT: U256 = U256::from_limbs([7, 0, 0, 0]);

/// Storage slot for Slipstream CL `slot0`.
pub const SLIPSTREAM_SLOT0_SLOT: U256 = U256::from_limbs([6, 0, 0, 0]);

/// Storage slot for Slipstream CL global liquidity.
pub const SLIPSTREAM_LIQUIDITY_SLOT: U256 = U256::from_limbs([17, 0, 0, 0]);

/// Base storage slot for Slipstream CL `ticks` mapping.
pub const SLIPSTREAM_TICKS_BASE_SLOT: U256 = U256::from_limbs([19, 0, 0, 0]);

/// Base storage slot for Slipstream CL `tickBitmap` mapping.
pub const SLIPSTREAM_TICK_BITMAP_BASE_SLOT: U256 = U256::from_limbs([18, 0, 0, 0]);

/// Storage layout for a V3-style concentrated-liquidity pool.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct V3StorageLayout {
    pub slot0_slot: U256,
    pub liquidity_slot: U256,
    pub ticks_base_slot: U256,
    pub tick_bitmap_base_slot: U256,
    pub tick_spacing: i32,
}

impl V3StorageLayout {
    /// Build a V3-style layout from explicit storage slots.
    pub const fn new(
        slot0_slot: U256,
        liquidity_slot: U256,
        ticks_base_slot: U256,
        tick_bitmap_base_slot: U256,
        tick_spacing: i32,
    ) -> Self {
        Self {
            slot0_slot,
            liquidity_slot,
            ticks_base_slot,
            tick_bitmap_base_slot,
            tick_spacing,
        }
    }

    /// Uniswap V3 canonical storage layout.
    pub const fn uniswap(tick_spacing: i32) -> Self {
        Self::new(
            V3_SLOT0_SLOT,
            V3_LIQUIDITY_SLOT,
            V3_TICKS_BASE_SLOT,
            V3_TICK_BITMAP_BASE_SLOT,
            tick_spacing,
        )
    }

    /// PancakeSwap V3 storage layout.
    pub const fn pancake(tick_spacing: i32) -> Self {
        Self::new(
            V3_SLOT0_SLOT,
            PANCAKE_V3_LIQUIDITY_SLOT,
            PANCAKE_V3_TICKS_BASE_SLOT,
            PANCAKE_V3_TICK_BITMAP_BASE_SLOT,
            tick_spacing,
        )
    }

    /// Slipstream CL storage layout.
    pub const fn slipstream(tick_spacing: i32) -> Self {
        Self::new(
            SLIPSTREAM_SLOT0_SLOT,
            SLIPSTREAM_LIQUIDITY_SLOT,
            SLIPSTREAM_TICKS_BASE_SLOT,
            SLIPSTREAM_TICK_BITMAP_BASE_SLOT,
            tick_spacing,
        )
    }
}

/// Storage layout for a Solidly V2 (Aerodrome / Velodrome V2) reserves pool.
///
/// Reserves are two separate `uint256` slots (not packed like Uniswap V2). Slot
/// indices are fork-specific and config-supplied — there is no derivable default,
/// so validate a fork's layout with the gated RPC-parity test before relying on
/// it in production.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SolidlyStorageLayout {
    pub reserve0_slot: U256,
    pub reserve1_slot: U256,
    pub token0_slot: U256,
    pub token1_slot: U256,
}

impl SolidlyStorageLayout {
    /// Build a Solidly V2 layout from explicit storage slots.
    pub const fn new(
        reserve0_slot: U256,
        reserve1_slot: U256,
        token0_slot: U256,
        token1_slot: U256,
    ) -> Self {
        Self {
            reserve0_slot,
            reserve1_slot,
            token0_slot,
            token1_slot,
        }
    }
}

/// Decode an EVM address from a right-aligned 32-byte storage word (the low 20
/// bytes), as used for the `token0`/`token1` address slots of a Uniswap V2 pair.
pub fn decode_address_slot(word: U256) -> Address {
    Address::from_slice(&word.to_be_bytes::<32>()[12..])
}

/// Compute the `tickBitmap` word position for a tick, matching the V3 contract
/// storage layout: `floor(tick / tick_spacing)` then `.div_euclid(256)`.
///
/// `div_euclid` is used for both divisions so negative ticks floor toward
/// negative infinity exactly as the on-chain `TickBitmap` library does.
pub fn v3_word_position(tick: i32, tick_spacing: i32) -> i16 {
    tick.div_euclid(tick_spacing).div_euclid(256) as i16
}

/// Compute the storage key for a Uniswap V3 tick bitmap word.
pub fn v3_tick_bitmap_storage_key(word_position: i16) -> U256 {
    v3_tick_bitmap_storage_key_with_base(word_position, V3_TICK_BITMAP_BASE_SLOT)
}

/// Compute the storage key for a V3-style tick bitmap word with a custom base slot.
pub fn v3_tick_bitmap_storage_key_with_base(word_position: i16, base_slot: U256) -> U256 {
    let word_i256 = i256_from_i16(word_position);
    let mut preimage = [0u8; 64];
    preimage[..32].copy_from_slice(&word_i256);
    preimage[32..64].copy_from_slice(&base_slot.to_be_bytes::<32>());
    keccak256(preimage).into()
}

/// Compute the four storage keys occupied by a Uniswap V3 `Tick.Info` struct.
pub fn v3_tick_info_storage_keys(tick: i32) -> [U256; 4] {
    v3_tick_info_storage_keys_with_base(tick, V3_TICKS_BASE_SLOT)
}

/// Compute the four storage keys occupied by a V3-style `Tick.Info` struct.
pub fn v3_tick_info_storage_keys_with_base(tick: i32, ticks_slot: U256) -> [U256; 4] {
    let tick_i256 = i256_from_i24(tick);
    let mut preimage = [0u8; 64];
    preimage[..32].copy_from_slice(&tick_i256);
    preimage[32..64].copy_from_slice(&ticks_slot.to_be_bytes::<32>());
    let base: U256 = keccak256(preimage).into();
    [
        base,
        base + U256::from(1),
        base + U256::from(2),
        base + U256::from(3),
    ]
}

fn i256_from_i16(value: i16) -> [u8; 32] {
    let mut result = if value < 0 { [0xFF; 32] } else { [0x00; 32] };
    let bytes = value.to_be_bytes();
    result[30] = bytes[0];
    result[31] = bytes[1];
    result
}

fn i256_from_i24(value: i32) -> [u8; 32] {
    let masked = value & 0x00FF_FFFF;
    let is_negative = (masked & 0x0080_0000) != 0;

    let mut result = if is_negative { [0xFF; 32] } else { [0x00; 32] };
    result[29] = ((masked >> 16) & 0xFF) as u8;
    result[30] = ((masked >> 8) & 0xFF) as u8;
    result[31] = (masked & 0xFF) as u8;
    result
}

/// Resolve the V3-style storage layout for a pool from its protocol metadata.
///
/// Lives here (always-on) rather than in the `uniswap-v3`-gated adapter module
/// so the always-on repair/reactive path can resolve layouts without depending
/// on a feature-gated adapter.
pub(crate) fn layout_for(pool: &PoolRegistration) -> Option<V3StorageLayout> {
    match &pool.metadata {
        ProtocolMetadata::UniswapV3(metadata) => {
            layout_from_metadata(metadata, ProtocolId::UniswapV3)
        }
        ProtocolMetadata::PancakeV3(metadata) => {
            layout_from_metadata(metadata, ProtocolId::PancakeV3)
        }
        ProtocolMetadata::Slipstream(metadata) => {
            layout_from_metadata(metadata, ProtocolId::Slipstream)
        }
        _ => None,
    }
}

fn layout_from_metadata(metadata: &V3Metadata, protocol: ProtocolId) -> Option<V3StorageLayout> {
    metadata.storage_layout.or_else(|| {
        let spacing = metadata.tick_spacing?;
        match protocol {
            ProtocolId::UniswapV3 => Some(V3StorageLayout::uniswap(spacing)),
            ProtocolId::PancakeV3 => Some(V3StorageLayout::pancake(spacing)),
            ProtocolId::Slipstream => Some(V3StorageLayout::slipstream(spacing)),
            _ => None,
        }
    })
}
