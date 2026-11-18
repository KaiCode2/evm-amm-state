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
pub const SLIPSTREAM_LIQUIDITY_SLOT: U256 = U256::from_limbs([16, 0, 0, 0]);

/// Additional pool-local parent cells for reviewed legacy Slipstream
/// transitions: factory identity, fee/reward growth, reward timing, and staked
/// liquidity. Keep bulk and windowed preparation on the same footprint,
/// including zeros.
#[cfg(feature = "uniswap-v3")]
pub(super) const SLIPSTREAM_TRANSITION_GLOBAL_SLOTS: [u64; 11] =
    [0, 3, 4, 7, 8, 9, 10, 11, 12, 14, 15];

/// Base storage slot for Slipstream CL `ticks` mapping.
pub const SLIPSTREAM_TICKS_BASE_SLOT: U256 = U256::from_limbs([17, 0, 0, 0]);

/// Base storage slot for Slipstream CL `tickBitmap` mapping.
pub const SLIPSTREAM_TICK_BITMAP_BASE_SLOT: U256 = U256::from_limbs([18, 0, 0, 0]);

/// Storage slot of `observations[0]` on the legacy Slipstream CL layout.
pub const SLIPSTREAM_OBSERVATIONS_SLOT: U256 = U256::from_limbs([20, 0, 0, 0]);

/// Storage slot for Aero (Dromos) CL `slot0`.
pub const AERO_CL_SLOT0_SLOT: U256 = U256::from_limbs([6, 0, 0, 0]);

/// Storage slot of the packed Aero CL `stakedLiquidity` (low 128 bits),
/// `lastUpdated` (`uint48` at bits 128..176) and `tickSpacing` (`int24` at
/// bits 176..200) word.
pub const AERO_CL_STAKED_LIQUIDITY_SLOT: U256 = U256::from_limbs([12, 0, 0, 0]);

/// Storage slot for Aero CL global liquidity (low 128 bits) packed with
/// `maxLiquidityPerTick` (high 128 bits).
pub const AERO_CL_LIQUIDITY_SLOT: U256 = U256::from_limbs([13, 0, 0, 0]);

/// Base storage slot for the Aero CL `ticks` mapping (six words per tick, in
/// the same order as legacy Slipstream).
pub const AERO_CL_TICKS_BASE_SLOT: U256 = U256::from_limbs([14, 0, 0, 0]);

/// Base storage slot for the Aero CL `tickBitmap` mapping.
pub const AERO_CL_TICK_BITMAP_BASE_SLOT: U256 = U256::from_limbs([15, 0, 0, 0]);

/// Storage slot of `observations[0]` on the Aero CL layout.
pub const AERO_CL_OBSERVATIONS_SLOT: U256 = U256::from_limbs([17, 0, 0, 0]);

/// Storage slots of Aero CL `feeGrowthGlobal0X128` and `feeGrowthGlobal1X128`.
pub const AERO_CL_FEE_GROWTH_0_SLOT: U256 = U256::from_limbs([7, 0, 0, 0]);
/// See [`AERO_CL_FEE_GROWTH_0_SLOT`].
pub const AERO_CL_FEE_GROWTH_1_SLOT: U256 = U256::from_limbs([8, 0, 0, 0]);

/// Storage slot of Aero CL `rewardGrowthGlobalX128`, credited by the pool's
/// gauge settlement (`_settleToBlock`).
pub const AERO_CL_REWARD_GROWTH_SLOT: U256 = U256::from_limbs([9, 0, 0, 0]);

/// Storage slot of the packed Aero CL `gaugeFees` word (`token0` low 128 bits,
/// `token1` high 128 bits).
pub const AERO_CL_GAUGE_FEES_SLOT: U256 = U256::from_limbs([10, 0, 0, 0]);

/// Storage slot of Aero CL `rollover`: emissions settled while nothing was
/// staked.
pub const AERO_CL_ROLLOVER_SLOT: U256 = U256::from_limbs([11, 0, 0, 0]);

/// Storage slot of Aero CL `secondsPerStakedLiquidityCumulativeX128`, the
/// first state variable after the fixed-size observation array.
pub const AERO_CL_SECONDS_PER_STAKED_LIQUIDITY_SLOT: U256 = U256::from_limbs([65_552, 0, 0, 0]);

/// Pool-local parent cells for reviewed Aero CL transitions and quotes:
/// factory, token and gauge/NFT identity (0, 1, 2, 3, 4), fee growth (7, 8),
/// reward growth (9), gauge fees (10), rollover (11), the packed
/// staked-liquidity/`lastUpdated`/spacing word (12) and the post-observation
/// staked-seconds accumulator (65552). The Metaquoter, the deployed hook and
/// the pool itself read these on the quote path, so bulk and windowed
/// preparation both keep them resident, including zeros, and a strict
/// (no-fetch) snapshot quote resolves every pool read locally.
#[cfg(feature = "uniswap-v3")]
pub(super) const AERO_CL_TRANSITION_GLOBAL_SLOTS: [u64; 12] =
    [0, 1, 2, 3, 4, 7, 8, 9, 10, 11, 12, 65_552];

/// Which reviewed Slipstream-family storage layout a [`V3StorageLayout`]
/// describes.
///
/// Both families share the six-word `Tick.Info` shape and the `slot0` word,
/// but place liquidity, the tick mappings, the observation ring, and the
/// staked-liquidity accounting at different slots. Exact event replay and the
/// one-shot sync program select their slot surface from this classification
/// rather than from the protocol id alone.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SlipstreamLayoutFamily {
    /// Legacy Aerodrome/Velodrome Slipstream (`liquidity` 16, `ticks` 17,
    /// `tickBitmap` 18, `observations` 20).
    Legacy,
    /// Aero (Dromos) CL (`liquidity` 13, `ticks` 14, `tickBitmap` 15,
    /// `observations` 17).
    AeroCl,
}

/// Storage layout for a V3-style concentrated-liquidity pool.
///
/// `#[non_exhaustive]`: construct via [`V3StorageLayout::new`] (or the
/// `uniswap`/`pancake`/`slipstream`/`aero_cl` presets) so future layout fields
/// (e.g. a new fork's fee-growth base slots) can be added without a breaking
/// change.
///
/// The optional [`observations_slot`](Self::observations_slot) describes where
/// the oracle ring starts. [`new`](Self::new) leaves it unset so a layout built
/// from the four core slots keeps comparing equal to the matching preset; the
/// presets derive it (see [`observations_slot`](Self::observations_slot)) and
/// [`with_observations_slot`](Self::with_observations_slot) pins it explicitly.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct V3StorageLayout {
    /// Storage slot of packed `slot0` (sqrtPriceX96 + current tick).
    pub slot0_slot: U256,
    /// Storage slot of the global `liquidity`.
    pub liquidity_slot: U256,
    /// Base slot of the `ticks` mapping (`Tick.Info` structs).
    pub ticks_base_slot: U256,
    /// Base slot of the `tickBitmap` mapping.
    pub tick_bitmap_base_slot: U256,
    /// The pool's tick spacing (must be positive).
    pub tick_spacing: i32,
    /// Explicit storage slot of `observations[0]`, when pinned. `None` lets
    /// the presets derive it from the core slots (see
    /// [`observations_slot`](Self::observations_slot)).
    pub observations_slot: Option<U256>,
}

impl V3StorageLayout {
    /// Build a V3-style layout from explicit storage slots.
    ///
    /// The observation-ring slot is left unset; chain
    /// [`with_observations_slot`](Self::with_observations_slot) to pin it for
    /// a layout the presets do not recognize.
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
            observations_slot: None,
        }
    }

    /// Pin the storage slot of `observations[0]`.
    pub const fn with_observations_slot(mut self, observations_slot: U256) -> Self {
        self.observations_slot = Some(observations_slot);
        self
    }

    /// The storage slot of `observations[0]`: the explicitly pinned slot, else
    /// the slot implied by a recognized preset (canonical Uniswap V3 → 8,
    /// PancakeSwap V3 → 9, legacy Slipstream → 20, Aero CL → 17), else `None`.
    pub fn observations_slot(&self) -> Option<U256> {
        if self.observations_slot.is_some() {
            return self.observations_slot;
        }
        if self.core_matches(&Self::uniswap(self.tick_spacing)) {
            Some(U256::from(8))
        } else if self.core_matches(&Self::pancake(self.tick_spacing)) {
            Some(U256::from(9))
        } else {
            self.slipstream_family().map(|family| match family {
                SlipstreamLayoutFamily::Legacy => SLIPSTREAM_OBSERVATIONS_SLOT,
                SlipstreamLayoutFamily::AeroCl => AERO_CL_OBSERVATIONS_SLOT,
            })
        }
    }

    /// Whether the four core slots and tick spacing of two layouts agree,
    /// ignoring an explicitly pinned observation slot.
    pub fn core_matches(&self, other: &Self) -> bool {
        self.slot0_slot == other.slot0_slot
            && self.liquidity_slot == other.liquidity_slot
            && self.ticks_base_slot == other.ticks_base_slot
            && self.tick_bitmap_base_slot == other.tick_bitmap_base_slot
            && self.tick_spacing == other.tick_spacing
    }

    /// Classify this layout as a reviewed Slipstream-family deployment layout.
    ///
    /// Returns [`SlipstreamLayoutFamily::Legacy`] for the deployed
    /// Aerodrome/Velodrome core slots and [`SlipstreamLayoutFamily::AeroCl`]
    /// for the Aero (Dromos) core slots, in both cases only when any pinned
    /// observation slot agrees with that family's ring position. Any other
    /// slot combination is `None`: exact event replay then fails closed.
    pub fn slipstream_family(&self) -> Option<SlipstreamLayoutFamily> {
        let (family, ring) = if self.core_matches(&Self::slipstream(self.tick_spacing)) {
            (SlipstreamLayoutFamily::Legacy, SLIPSTREAM_OBSERVATIONS_SLOT)
        } else if self.core_matches(&Self::aero_cl(self.tick_spacing)) {
            (SlipstreamLayoutFamily::AeroCl, AERO_CL_OBSERVATIONS_SLOT)
        } else {
            return None;
        };
        match self.observations_slot {
            None => Some(family),
            Some(pinned) if pinned == ring => Some(family),
            Some(_) => None,
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

    /// Deployed Aerodrome/Velodrome Slipstream CL core layout.
    ///
    /// This four-field descriptor covers the quote/search geometry. Exact event
    /// replay additionally validates a reviewed deployment identity and keeps
    /// its active/staked-liquidity, oracle, bitmap, and six-word tick search
    /// state coherent. Runtime-bound fee evidence opts into the stronger fee,
    /// reward, gauge, and crossed-tick accounting replay.
    ///
    /// The observation ring (slot 20) is derived, not pinned, so this preset
    /// stays equal to the same four slots passed to [`new`](Self::new).
    pub const fn slipstream(tick_spacing: i32) -> Self {
        Self::new(
            SLIPSTREAM_SLOT0_SLOT,
            SLIPSTREAM_LIQUIDITY_SLOT,
            SLIPSTREAM_TICKS_BASE_SLOT,
            SLIPSTREAM_TICK_BITMAP_BASE_SLOT,
            tick_spacing,
        )
    }

    /// Deployed Aero (Dromos) CL core layout, confirmed by raw slot reads at
    /// Ethereum block 26292020 and Base block 53205395: `slot0` 6, packed
    /// `liquidity`/`maxLiquidityPerTick` 13, `ticks` 14, `tickBitmap` 15,
    /// `observations` 17. The observation ring is derived, so this preset
    /// equals the same four slots passed to [`new`](Self::new).
    ///
    /// Register Aero CL pools under [`PoolKey::Slipstream`](super::PoolKey) with
    /// this layout; the Slipstream adapter classifies the family from the
    /// layout and applies the Aero slot surface for cold start and exact
    /// event replay.
    pub const fn aero_cl(tick_spacing: i32) -> Self {
        Self::new(
            AERO_CL_SLOT0_SLOT,
            AERO_CL_LIQUIDITY_SLOT,
            AERO_CL_TICKS_BASE_SLOT,
            AERO_CL_TICK_BITMAP_BASE_SLOT,
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
///
/// `#[non_exhaustive]`: construct via [`SolidlyStorageLayout::new`] so future
/// layout fields can be added without a breaking change.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SolidlyStorageLayout {
    /// Storage slot of `reserve0` (a full `uint256` word).
    pub reserve0_slot: U256,
    /// Storage slot of `reserve1` (a full `uint256` word).
    pub reserve1_slot: U256,
    /// Storage slot of the `token0` address.
    pub token0_slot: U256,
    /// Storage slot of the `token1` address.
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

    /// Deployed Aero (Dromos) basic-pool layout, confirmed against the pool
    /// getters at Optimism block 158800699: `reserve0` 196619, `reserve1`
    /// 196620, `token0` 7, `token1` 8. The ERC-20 state and the 65535-entry
    /// observation buffer precede the reserves, which is why they sit far
    /// above the legacy Velodrome V2 slots (20/21/13/14).
    pub const fn aero_volatile() -> Self {
        Self::new(
            U256::from_limbs([196_619, 0, 0, 0]),
            U256::from_limbs([196_620, 0, 0, 0]),
            U256::from_limbs([7, 0, 0, 0]),
            U256::from_limbs([8, 0, 0, 0]),
        )
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
///
/// # Panics
///
/// Panics if `tick_spacing` is not positive. Layouts resolved from pool
/// metadata are validated before they reach this function; the assert guards
/// direct callers passing a hand-built layout.
pub fn v3_word_position(tick: i32, tick_spacing: i32) -> i16 {
    assert!(
        tick_spacing > 0,
        "V3 tick_spacing must be positive, got {tick_spacing}"
    );
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
    let base = v3_tick_info_storage_base(tick, ticks_slot);
    [
        base,
        base + U256::from(1),
        base + U256::from(2),
        base + U256::from(3),
    ]
}

/// Compute the six storage keys occupied by deployed Slipstream `Tick.Info`.
pub fn slipstream_tick_info_storage_keys_with_base(tick: i32, ticks_slot: U256) -> [U256; 6] {
    let base = v3_tick_info_storage_base(tick, ticks_slot);
    [
        base,
        base + U256::from(1),
        base + U256::from(2),
        base + U256::from(3),
        base + U256::from(4),
        base + U256::from(5),
    ]
}

fn v3_tick_info_storage_base(tick: i32, ticks_slot: U256) -> U256 {
    let tick_i256 = i256_from_i24(tick);
    let mut preimage = [0u8; 64];
    preimage[..32].copy_from_slice(&tick_i256);
    preimage[32..64].copy_from_slice(&ticks_slot.to_be_bytes::<32>());
    keccak256(preimage).into()
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
    let layout = metadata.storage_layout.or_else(|| {
        let spacing = metadata.tick_spacing?;
        match protocol {
            ProtocolId::UniswapV3 => Some(V3StorageLayout::uniswap(spacing)),
            ProtocolId::PancakeV3 => Some(V3StorageLayout::pancake(spacing)),
            ProtocolId::Slipstream => Some(V3StorageLayout::slipstream(spacing)),
            _ => None,
        }
    })?;
    // A non-positive tick spacing cannot describe a real V3-family pool and
    // would divide by zero in the bitmap-word math (`v3_word_position`); treat
    // the layout as unresolvable so cold-start reports Unsupported and the
    // repair path falls back to conservative whole-storage invalidation.
    (layout.tick_spacing > 0).then_some(layout)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::types::{PoolKey, PoolRegistration, ProtocolMetadata, V3Metadata};

    fn v3_pool(metadata: V3Metadata) -> PoolRegistration {
        PoolRegistration::new(PoolKey::UniswapV3(Address::repeat_byte(0x11)))
            .with_metadata(ProtocolMetadata::UniswapV3(metadata))
    }

    #[test]
    fn layout_rejects_zero_and_negative_derived_spacing() {
        assert!(layout_for(&v3_pool(V3Metadata::default().with_tick_spacing(0))).is_none());
        assert!(layout_for(&v3_pool(V3Metadata::default().with_tick_spacing(-10))).is_none());
        assert!(layout_for(&v3_pool(V3Metadata::default().with_tick_spacing(10))).is_some());
    }

    #[test]
    fn layout_rejects_zero_spacing_in_explicit_layout() {
        let explicit = V3Metadata::default().with_storage_layout(V3StorageLayout::uniswap(0));
        assert!(layout_for(&v3_pool(explicit)).is_none());
    }

    #[test]
    #[should_panic(expected = "tick_spacing must be positive")]
    fn word_position_panics_loudly_on_zero_spacing() {
        v3_word_position(0, 0);
    }

    #[test]
    fn presets_derive_observation_slots_without_pinning_them() {
        assert_eq!(V3StorageLayout::uniswap(10).observations_slot, None);
        assert_eq!(
            V3StorageLayout::uniswap(10).observations_slot(),
            Some(U256::from(8))
        );
        assert_eq!(
            V3StorageLayout::pancake(60).observations_slot(),
            Some(U256::from(9))
        );
        assert_eq!(
            V3StorageLayout::slipstream(200).observations_slot(),
            Some(U256::from(20))
        );
        assert_eq!(
            V3StorageLayout::aero_cl(50).observations_slot(),
            Some(U256::from(17))
        );
        assert_eq!(
            V3StorageLayout::new(
                U256::from(1),
                U256::from(2),
                U256::from(3),
                U256::from(4),
                10
            )
            .observations_slot(),
            None
        );
        assert_eq!(
            V3StorageLayout::new(
                U256::from(1),
                U256::from(2),
                U256::from(3),
                U256::from(4),
                10
            )
            .with_observations_slot(U256::from(77))
            .observations_slot(),
            Some(U256::from(77))
        );
    }

    #[test]
    fn legacy_slipstream_layout_equality_is_unchanged() {
        // Existing registrations build the legacy layout from the four core
        // slots; that must keep classifying as the reviewed legacy family and
        // keep comparing equal to the preset.
        let explicit = V3StorageLayout::new(
            U256::from(6),
            U256::from(16),
            U256::from(17),
            U256::from(18),
            200,
        );
        assert_eq!(explicit, V3StorageLayout::slipstream(200));
        assert_eq!(
            explicit.slipstream_family(),
            Some(SlipstreamLayoutFamily::Legacy)
        );
        assert_eq!(
            V3StorageLayout::slipstream(200)
                .with_observations_slot(U256::from(20))
                .slipstream_family(),
            Some(SlipstreamLayoutFamily::Legacy)
        );
    }

    #[test]
    fn aero_cl_layout_classifies_and_rejects_inconsistent_rings() {
        let explicit = V3StorageLayout::new(
            U256::from(6),
            U256::from(13),
            U256::from(14),
            U256::from(15),
            50,
        );
        assert_eq!(explicit, V3StorageLayout::aero_cl(50));
        assert_eq!(
            explicit.slipstream_family(),
            Some(SlipstreamLayoutFamily::AeroCl)
        );
        assert_eq!(
            V3StorageLayout::aero_cl(100)
                .with_observations_slot(U256::from(17))
                .slipstream_family(),
            Some(SlipstreamLayoutFamily::AeroCl)
        );
        // A pinned ring that contradicts the family is not a reviewed layout.
        assert_eq!(
            V3StorageLayout::aero_cl(100)
                .with_observations_slot(U256::from(20))
                .slipstream_family(),
            None
        );
        assert_eq!(
            V3StorageLayout::slipstream(200)
                .with_observations_slot(U256::from(17))
                .slipstream_family(),
            None
        );
        assert_eq!(V3StorageLayout::uniswap(10).slipstream_family(), None);
        assert_ne!(
            V3StorageLayout::aero_cl(50),
            V3StorageLayout::slipstream(50)
        );
    }

    #[test]
    fn aero_volatile_solidly_layout_matches_the_pinned_slots() {
        let layout = SolidlyStorageLayout::aero_volatile();
        assert_eq!(layout.reserve0_slot, U256::from(196_619));
        assert_eq!(layout.reserve1_slot, U256::from(196_620));
        assert_eq!(layout.token0_slot, U256::from(7));
        assert_eq!(layout.token1_slot, U256::from(8));
    }
}
