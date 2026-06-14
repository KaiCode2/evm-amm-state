use super::*;
use crate::tuning::{ProtocolAddresses, SyncSpeedMode, sync_speed_mode};

/// Adaptive scan parameters based on pool liquidity depth.
///
/// High-liquidity pools need fewer words scanned because bounded swaps cross
/// fewer ticks. Tick spacing affects how much price range each word covers:
/// - spacing=1: 1 word = 256 ticks (~2.56% price range)
/// - spacing=10: 1 word = 2560 ticks (~25.6% price range)
/// - spacing=60: 1 word = 15360 ticks (~153% price range)
pub(crate) struct AdaptiveScanParams {
    /// Maximum words to scan in each direction from current tick.
    pub(crate) max_scan_words: i32,
    /// Consecutive empty bitmap words before stopping the scan.
    pub(crate) empty_word_threshold: usize,
}

/// Compute scan parameters based on pool liquidity and tick spacing.
///
/// High-liquidity pools need fewer words because swaps cross fewer ticks.
/// The thresholds are conservative (3-10x more coverage than estimated
/// ticks-crossed for a $100K swap at each liquidity level).
pub(crate) fn compute_adaptive_scan_params(
    liquidity: u128,
    tick_spacing: i32,
) -> AdaptiveScanParams {
    let narrow_spacing = tick_spacing < 10;

    // Speed multiplier: reduces scan range for slower modes to save RPC calls.
    let multiplier = match sync_speed_mode() {
        SyncSpeedMode::Fast => 1.0,
        SyncSpeedMode::Normal => 0.75,
        SyncSpeedMode::Slow => 0.5,
        SyncSpeedMode::XSlow => 0.15,
    };

    let scale = |words: i32| -> i32 { ((words as f64) * multiplier).ceil().max(3.0) as i32 };

    if liquidity >= 1_000_000_000_000_000_000 {
        // Deep liquidity (>=1e18): very few ticks crossed per swap
        AdaptiveScanParams {
            max_scan_words: scale(if narrow_spacing { 15 } else { 8 }),
            empty_word_threshold: 3,
        }
    } else if liquidity >= 1_000_000_000_000_000 {
        // Moderate liquidity (>=1e15)
        AdaptiveScanParams {
            max_scan_words: scale(if narrow_spacing { 30 } else { 15 }),
            empty_word_threshold: 4,
        }
    } else if liquidity >= 1_000_000_000_000 {
        // Thin liquidity (>=1e12)
        AdaptiveScanParams {
            max_scan_words: scale(if narrow_spacing { 80 } else { 40 }),
            empty_word_threshold: 5,
        }
    } else {
        // Very thin liquidity: use full scan limits
        AdaptiveScanParams {
            max_scan_words: scale(if narrow_spacing {
                max_scan_radius()
            } else {
                200
            }),
            empty_word_threshold: 5,
        }
    }
}

/// Whether a V3-style pool is Uniswap V3, PancakeSwap V3, or Slipstream CL.
/// Determines which storage slot constants to use.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum V3Flavor {
    UniswapV3,
    PancakeSwapV3,
    /// Aerodrome/Velodrome Slipstream CL pools — shifted storage layout
    /// (slot0=6, liquidity=17, ticks=18, tickBitmap=19).
    Slipstream,
}

impl V3Flavor {
    pub fn slot0_slot(&self) -> U256 {
        match self {
            V3Flavor::UniswapV3 | V3Flavor::PancakeSwapV3 => V3_SLOT0_SLOT,
            V3Flavor::Slipstream => SLIPSTREAM_SLOT0_SLOT,
        }
    }

    pub fn liquidity_slot(&self) -> U256 {
        match self {
            V3Flavor::UniswapV3 => V3_LIQUIDITY_SLOT,
            V3Flavor::PancakeSwapV3 => PANCAKE_V3_LIQUIDITY_SLOT,
            V3Flavor::Slipstream => SLIPSTREAM_LIQUIDITY_SLOT,
        }
    }

    pub fn tick_bitmap_base_slot(&self) -> U256 {
        match self {
            V3Flavor::UniswapV3 => V3_TICK_BITMAP_BASE_SLOT,
            V3Flavor::PancakeSwapV3 => PANCAKE_V3_TICK_BITMAP_BASE_SLOT,
            V3Flavor::Slipstream => SLIPSTREAM_TICK_BITMAP_BASE_SLOT,
        }
    }

    pub fn ticks_base_slot(&self) -> U256 {
        match self {
            V3Flavor::UniswapV3 => V3_TICKS_BASE_SLOT,
            V3Flavor::PancakeSwapV3 => PANCAKE_V3_TICKS_BASE_SLOT,
            V3Flavor::Slipstream => SLIPSTREAM_TICKS_BASE_SLOT,
        }
    }

    pub(crate) fn tick_bitmap_key(&self, word: i16) -> U256 {
        match self {
            V3Flavor::UniswapV3 => v3_tick_bitmap_storage_key(word),
            V3Flavor::PancakeSwapV3 => {
                v3_tick_bitmap_storage_key_with_base(word, PANCAKE_V3_TICK_BITMAP_BASE_SLOT)
            }
            V3Flavor::Slipstream => {
                v3_tick_bitmap_storage_key_with_base(word, SLIPSTREAM_TICK_BITMAP_BASE_SLOT)
            }
        }
    }

    pub(crate) fn tick_info_keys(&self, tick: i32) -> [U256; 4] {
        match self {
            V3Flavor::UniswapV3 => v3_tick_info_storage_keys(tick),
            V3Flavor::PancakeSwapV3 => {
                v3_tick_info_storage_keys_with_base(tick, PANCAKE_V3_TICKS_BASE_SLOT)
            }
            V3Flavor::Slipstream => {
                v3_tick_info_storage_keys_with_base(tick, SLIPSTREAM_TICKS_BASE_SLOT)
            }
        }
    }
}

/// Build a factory-address → V3Flavor lookup map from [`ProtocolAddresses`].
/// Used during auto-loading to distinguish UniswapV3 vs PancakeSwapV3 vs Slipstream
/// pools that share the same Swap event signature.
pub fn build_v3_factory_map(
    addrs: &ProtocolAddresses,
) -> std::collections::HashMap<Address, V3Flavor> {
    let mut map = std::collections::HashMap::new();
    if let Some(addr) = addrs.uniswap_v3_factory {
        map.insert(addr, V3Flavor::UniswapV3);
    }
    if let Some(addr) = addrs.pancake_v3_factory {
        map.insert(addr, V3Flavor::PancakeSwapV3);
    }
    if let Some(addr) = addrs.slipstream_factory {
        map.insert(addr, V3Flavor::Slipstream);
    }
    map
}

/// Check if a V3 pool needs a full tick resync based on how far the tick has moved.
///
/// Returns true if the tick has moved to a different bitmap word since last sync,
/// which means we might need to load new tick data.
pub fn needs_tick_resync(pool: &UniswapV3Pool, new_tick: i32) -> bool {
    if pool.tick_spacing == 0 {
        return false;
    }

    let old_word = tick_to_word(pool.tick, pool.tick_spacing);
    let new_word = tick_to_word(new_tick, pool.tick_spacing);

    // If we've moved to a different word, check if we have that word's bitmap
    if old_word != new_word {
        // Check if the new word is already in our bitmap
        !pool.tick_bitmap.contains_key(&(new_word as i16))
    } else {
        false
    }
}

pub(crate) fn div_floor_i32(a: i32, b: i32) -> i32 {
    let mut q = a / b;
    if (a ^ b) < 0 && a % b != 0 {
        q -= 1;
    }
    q
}

pub(crate) fn tick_to_word(tick: i32, tick_spacing: i32) -> i32 {
    let compressed = div_floor_i32(tick, tick_spacing);
    compressed.div_euclid(256)
}

/// Extract initialized tick indices from a single bitmap word.
///
/// This is a per-word version of `extract_initialized_ticks_from_bitmap`,
/// used during incremental resync to compare individual words.
pub(crate) fn extract_ticks_from_bitmap_word(
    word_pos: i32,
    bitmap: U256,
    tick_spacing: i32,
) -> Vec<i32> {
    let mut ticks = Vec::new();
    if bitmap == U256::ZERO {
        return ticks;
    }
    let limbs = bitmap.as_limbs();
    for (limb_idx, &limb) in limbs.iter().enumerate() {
        if limb == 0 {
            continue;
        }
        let base_bit = (limb_idx * 64) as i32;
        let mut remaining = limb;
        while remaining != 0 {
            let bit_in_limb = remaining.trailing_zeros() as i32;
            remaining &= remaining - 1;
            let bit = base_bit + bit_in_limb;
            let tick_index = (word_pos * 256 + bit) * tick_spacing;
            ticks.push(tick_index);
        }
    }
    ticks
}

/// Extract initialized tick indices from the tick bitmap.
/// Uses optimized bit iteration - O(set bits) instead of O(256) per word.
pub(crate) fn extract_initialized_ticks_from_bitmap(pool: &UniswapV3Pool) -> Vec<i32> {
    let mut initialized_ticks = Vec::new();
    for (word_pos, bitmap) in pool
        .tick_bitmap
        .iter()
        .filter(|(_, bitmap)| **bitmap != U256::ZERO)
    {
        let word_pos = *word_pos as i32;
        // Iterate over the 4 u64 limbs (64 bits each = 256 total)
        let limbs = bitmap.as_limbs();
        for (limb_idx, &limb) in limbs.iter().enumerate() {
            if limb == 0 {
                continue;
            }
            let base_bit = (limb_idx * 64) as i32;
            let mut remaining = limb;
            while remaining != 0 {
                // Find position of lowest set bit
                let bit_in_limb = remaining.trailing_zeros() as i32;
                // Clear the lowest set bit
                remaining &= remaining - 1;
                let bit = base_bit + bit_in_limb;
                let tick_index = (word_pos * 256 + bit) * pool.tick_spacing;
                initialized_ticks.push(tick_index);
            }
        }
    }
    initialized_ticks
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_token() -> Token {
        Token::new_with_decimals(Address::ZERO, 18)
    }

    #[test]
    fn test_tick_to_word() {
        // tick_spacing = 1
        assert_eq!(tick_to_word(0, 1), 0);
        assert_eq!(tick_to_word(255, 1), 0);
        assert_eq!(tick_to_word(256, 1), 1);
        assert_eq!(tick_to_word(-1, 1), -1);
        assert_eq!(tick_to_word(-256, 1), -1);
        assert_eq!(tick_to_word(-257, 1), -2);

        // tick_spacing = 60
        assert_eq!(tick_to_word(0, 60), 0);
        assert_eq!(tick_to_word(60 * 255, 60), 0);
        assert_eq!(tick_to_word(60 * 256, 60), 1);
        assert_eq!(tick_to_word(-60, 60), -1);
    }

    #[test]
    fn test_tick_to_word_boundaries() {
        // Test at MIN_TICK and MAX_TICK
        let min_word_ts1 = tick_to_word(MIN_TICK, 1);
        let max_word_ts1 = tick_to_word(MAX_TICK, 1);

        // With tick_spacing=1, the range should be about -3466 to 3465 words
        assert!(min_word_ts1 < 0);
        assert!(max_word_ts1 > 0);

        // The total word range for tick_spacing=1 should be ~6931
        let word_range = max_word_ts1 - min_word_ts1 + 1;
        assert!(word_range > 6000 && word_range < 7000);
    }

    #[test]
    fn test_needs_tick_resync_same_word() {
        let mut pool = UniswapV3Pool {
            address: Address::ZERO,
            token_a: dummy_token(),
            token_b: dummy_token(),
            fee: 3000,
            tick_spacing: 60,
            sqrt_price: U256::ZERO,
            tick: 1000,
            liquidity: 0,
            tick_bitmap: HashMap::new(),
            ticks: HashMap::new(),
        };

        // Add the current word to the bitmap
        let current_word = tick_to_word(1000, 60);
        pool.tick_bitmap.insert(current_word as i16, U256::ZERO);

        // Small tick movement within the same word - no resync needed
        assert!(!needs_tick_resync(&pool, 1001));
        assert!(!needs_tick_resync(&pool, 999));
    }

    #[test]
    fn test_needs_tick_resync_different_word_not_cached() {
        let mut pool = UniswapV3Pool {
            address: Address::ZERO,
            token_a: dummy_token(),
            token_b: dummy_token(),
            fee: 3000,
            tick_spacing: 60,
            sqrt_price: U256::ZERO,
            tick: 0,
            liquidity: 0,
            tick_bitmap: HashMap::new(),
            ticks: HashMap::new(),
        };

        // Add only word 0 to the bitmap
        pool.tick_bitmap.insert(0, U256::ZERO);

        // Large tick movement to a different word not in cache - needs resync
        let new_tick = 60 * 256 + 1; // This is in word 1
        assert!(needs_tick_resync(&pool, new_tick));
    }

    #[test]
    fn test_needs_tick_resync_different_word_cached() {
        let mut pool = UniswapV3Pool {
            address: Address::ZERO,
            token_a: dummy_token(),
            token_b: dummy_token(),
            fee: 3000,
            tick_spacing: 60,
            sqrt_price: U256::ZERO,
            tick: 0,
            liquidity: 0,
            tick_bitmap: HashMap::new(),
            ticks: HashMap::new(),
        };

        // Add both word 0 and word 1 to the bitmap
        pool.tick_bitmap.insert(0, U256::ZERO);
        pool.tick_bitmap.insert(1, U256::ZERO);

        // Large tick movement to a different word but it's cached - no resync needed
        let new_tick = 60 * 256 + 1; // This is in word 1
        assert!(!needs_tick_resync(&pool, new_tick));
    }

    #[test]
    fn test_div_floor_i32() {
        assert_eq!(div_floor_i32(7, 3), 2);
        assert_eq!(div_floor_i32(-7, 3), -3);
        assert_eq!(div_floor_i32(7, -3), -3);
        assert_eq!(div_floor_i32(-7, -3), 2);
        assert_eq!(div_floor_i32(6, 3), 2);
        assert_eq!(div_floor_i32(-6, 3), -2);
    }

    #[test]
    fn test_extract_ticks_from_bitmap_word_empty() {
        let ticks = extract_ticks_from_bitmap_word(0, U256::ZERO, 60);
        assert!(ticks.is_empty());
    }

    #[test]
    fn test_extract_ticks_from_bitmap_word_single_bit() {
        // Bit 0 set in word 0, tick_spacing=60 -> tick = 0*256*60 + 0*60 = 0
        let bitmap = U256::from(1u64);
        let ticks = extract_ticks_from_bitmap_word(0, bitmap, 60);
        assert_eq!(ticks, vec![0]);
    }

    #[test]
    fn test_extract_ticks_from_bitmap_word_multiple_bits() {
        // Bits 0 and 3 set in word 0, tick_spacing=1
        let bitmap = U256::from(0b1001u64); // bits 0 and 3
        let ticks = extract_ticks_from_bitmap_word(0, bitmap, 1);
        assert_eq!(ticks, vec![0, 3]);
    }

    #[test]
    fn test_extract_ticks_from_bitmap_word_negative_word() {
        // Bit 0 in word -1, tick_spacing=1 -> tick = -1*256 + 0 = -256
        let bitmap = U256::from(1u64);
        let ticks = extract_ticks_from_bitmap_word(-1, bitmap, 1);
        assert_eq!(ticks, vec![-256]);
    }

    #[test]
    fn test_extract_ticks_from_bitmap_word_with_spacing() {
        // Bit 5 in word 2, tick_spacing=10 -> tick = (2*256 + 5) * 10 = 5170
        let bitmap = U256::from(1u64 << 5);
        let ticks = extract_ticks_from_bitmap_word(2, bitmap, 10);
        assert_eq!(ticks, vec![5170]);
    }

    #[test]
    fn test_extract_ticks_from_bitmap_word_matches_pool_extractor() {
        // Verify our per-word function gives same results as the pool-level extractor
        let tick_spacing = 60;

        let mut tick_bitmap: HashMap<i16, U256> = HashMap::new();
        let bitmap_word0 = U256::from(0xFF00u64);
        let bitmap_word1 = U256::from(0x01u64);
        tick_bitmap.insert(0, bitmap_word0);
        tick_bitmap.insert(1, bitmap_word1);

        let pool = UniswapV3Pool {
            address: Address::ZERO,
            token_a: dummy_token(),
            token_b: dummy_token(),
            fee: 3000,
            tick_spacing,
            sqrt_price: U256::ZERO,
            tick: 0,
            liquidity: 0,
            tick_bitmap: tick_bitmap.clone(),
            ticks: HashMap::new(),
        };

        // Get ticks from the pool-level extractor
        let mut pool_ticks = extract_initialized_ticks_from_bitmap(&pool);
        pool_ticks.sort();

        // Get ticks from our per-word function
        let mut word_ticks: Vec<i32> = Vec::new();
        word_ticks.extend(extract_ticks_from_bitmap_word(
            0,
            bitmap_word0,
            tick_spacing,
        ));
        word_ticks.extend(extract_ticks_from_bitmap_word(
            1,
            bitmap_word1,
            tick_spacing,
        ));
        word_ticks.sort();

        assert_eq!(
            pool_ticks, word_ticks,
            "per-word extractor should match pool extractor"
        );
    }

    // --- AdaptiveScanParams tests ---

    /// Set speed to Fast so adaptive scan tests use base (unmultiplied) values.
    fn set_fast_mode() {
        crate::tuning::set_sync_speed_mode(SyncSpeedMode::Fast);
    }

    #[test]
    fn test_adaptive_scan_deep_liquidity_narrow_spacing() {
        set_fast_mode();
        let params = compute_adaptive_scan_params(1_000_000_000_000_000_000, 1);
        assert_eq!(params.max_scan_words, 15);
        assert_eq!(params.empty_word_threshold, 3);
    }

    #[test]
    fn test_adaptive_scan_deep_liquidity_wide_spacing() {
        set_fast_mode();
        let params = compute_adaptive_scan_params(1_000_000_000_000_000_000, 60);
        assert_eq!(params.max_scan_words, 8);
        assert_eq!(params.empty_word_threshold, 3);
    }

    #[test]
    fn test_adaptive_scan_moderate_liquidity_narrow_spacing() {
        set_fast_mode();
        let params = compute_adaptive_scan_params(1_000_000_000_000_000, 1);
        assert_eq!(params.max_scan_words, 30);
        assert_eq!(params.empty_word_threshold, 4);
    }

    #[test]
    fn test_adaptive_scan_moderate_liquidity_wide_spacing() {
        set_fast_mode();
        let params = compute_adaptive_scan_params(1_000_000_000_000_000, 10);
        assert_eq!(params.max_scan_words, 15);
        assert_eq!(params.empty_word_threshold, 4);
    }

    #[test]
    fn test_adaptive_scan_thin_liquidity() {
        set_fast_mode();
        let params = compute_adaptive_scan_params(1_000_000_000_000, 1);
        assert_eq!(params.max_scan_words, 80);
        assert_eq!(params.empty_word_threshold, 5);

        let params_wide = compute_adaptive_scan_params(1_000_000_000_000, 60);
        assert_eq!(params_wide.max_scan_words, 40);
        assert_eq!(params_wide.empty_word_threshold, 5);
    }

    #[test]
    fn test_adaptive_scan_very_thin_liquidity() {
        set_fast_mode();
        let params = compute_adaptive_scan_params(999_999_999_999, 1);
        assert_eq!(params.max_scan_words, max_scan_radius());
        assert_eq!(params.empty_word_threshold, 5);

        let params_wide = compute_adaptive_scan_params(0, 60);
        assert_eq!(params_wide.max_scan_words, 200);
        assert_eq!(params_wide.empty_word_threshold, 5);
    }

    #[test]
    fn test_adaptive_scan_boundary_values() {
        set_fast_mode();
        // Exactly at tier boundaries
        let deep = compute_adaptive_scan_params(1_000_000_000_000_000_000, 1);
        let just_below_deep = compute_adaptive_scan_params(999_999_999_999_999_999, 1);
        assert_eq!(deep.max_scan_words, 15);
        assert_eq!(just_below_deep.max_scan_words, 30);

        let moderate = compute_adaptive_scan_params(1_000_000_000_000_000, 1);
        let just_below_moderate = compute_adaptive_scan_params(999_999_999_999_999, 1);
        assert_eq!(moderate.max_scan_words, 30);
        assert_eq!(just_below_moderate.max_scan_words, 80);

        let thin = compute_adaptive_scan_params(1_000_000_000_000, 1);
        let just_below_thin = compute_adaptive_scan_params(999_999_999_999, 1);
        assert_eq!(thin.max_scan_words, 80);
        assert_eq!(just_below_thin.max_scan_words, max_scan_radius());
    }

    #[test]
    fn test_adaptive_scan_speed_mode_scaling() {
        // Verify that speed mode affects scan params.
        // NOTE: this test mutates global state. set_fast_mode() at end to
        // avoid poisoning other parallel tests that also call set_fast_mode().
        set_fast_mode();
        let fast = compute_adaptive_scan_params(1_000_000_000_000_000, 1);

        crate::tuning::set_sync_speed_mode(SyncSpeedMode::XSlow);
        let xslow = compute_adaptive_scan_params(1_000_000_000_000_000, 1);

        // Restore to Fast (all other tests in this module assume Fast).
        set_fast_mode();

        assert!(
            fast.max_scan_words > xslow.max_scan_words,
            "fast ({}) should scan more words than xslow ({})",
            fast.max_scan_words,
            xslow.max_scan_words
        );
    }

    #[test]
    fn test_adaptive_scan_spacing_threshold() {
        set_fast_mode();
        // tick_spacing=9 is narrow, tick_spacing=10 is wide
        let narrow = compute_adaptive_scan_params(1_000_000_000_000_000_000, 9);
        let wide = compute_adaptive_scan_params(1_000_000_000_000_000_000, 10);
        assert_eq!(narrow.max_scan_words, 15); // narrow path
        assert_eq!(wide.max_scan_words, 8); // wide path
    }

    #[test]
    fn test_adaptive_scan_very_high_liquidity() {
        set_fast_mode();
        // u128::MAX should still hit deep tier
        let params = compute_adaptive_scan_params(u128::MAX, 1);
        assert_eq!(params.max_scan_words, 15);
        assert_eq!(params.empty_word_threshold, 3);
    }
}
