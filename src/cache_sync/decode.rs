use std::time::{SystemTime, UNIX_EPOCH};

use alloy_primitives::U256;
use amms::amms::uniswap_v3::Info;

/// Pack V2 reserves into slot 8 format for EVM cache injection.
///
/// V2 slot 8 packing: `reserve0(uint112) | reserve1(uint112) << 112 | blockTimestampLast(uint32) << 224`
pub fn encode_v2_reserves_raw(reserve0: u128, reserve1: u128) -> U256 {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as u32;
    U256::from(reserve0) | (U256::from(reserve1) << 112) | (U256::from(timestamp) << 224)
}

/// Patch sqrtPriceX96 + tick into an existing V3 slot0 value, preserving observation fields.
///
/// V3 slot 0 packing: `sqrtPriceX96(uint160) | tick(int24) << 160 | observationIndex(uint16) << 184 | ...`
/// This patches bits 0-183 while preserving bits 184-255 (observation metadata, feeProtocol, unlocked).
pub fn encode_v3_slot0_patch(existing: U256, sqrt_price: U256, tick: i32) -> U256 {
    // Mask to preserve bits 184-255 (observation fields)
    let lower_mask: U256 = (U256::from(1u64) << 184) - U256::from(1u64);
    let upper_preserved = existing & !lower_mask;
    // Encode tick as 24-bit unsigned (two's complement for negative values)
    let tick_bits = (tick as u32) & 0xFFFFFF;
    upper_preserved | sqrt_price | (U256::from(tick_bits) << 160)
}

/// Decode a V2 packed reserves slot (slot 8) into reserve0 and reserve1.
///
/// V2 slot 8 packing: `reserve0(uint112) | reserve1(uint112) << 112 | blockTimestampLast(uint32) << 224`
pub fn decode_v2_reserves_raw(raw: U256) -> (u128, u128) {
    // reserve0 is the lower 112 bits, reserve1 is bits 112..223
    let limbs = raw.as_limbs(); // [bits 0-63, 64-127, 128-191, 192-255]
    let reserve0: u128 = (limbs[0] as u128 | ((limbs[1] as u128) << 64)) & ((1u128 << 112) - 1);
    let shifted: U256 = raw >> 112;
    let shifted_limbs = shifted.as_limbs();
    let reserve1: u128 =
        (shifted_limbs[0] as u128 | ((shifted_limbs[1] as u128) << 64)) & ((1u128 << 112) - 1);
    (reserve0, reserve1)
}

/// Decode a V3 slot0 packed value into sqrtPriceX96 and tick.
///
/// V3 slot 0 packing: `sqrtPriceX96(uint160) | tick(int24) << 160 | ...`
pub fn decode_v3_slot0_raw(raw: U256) -> (U256, i32) {
    let mask_160: U256 = (U256::from(1u64) << 160) - U256::from(1u64);
    let sqrt_price = raw & mask_160;
    // tick is int24 at bits 160..183
    let shifted: U256 = raw >> 160;
    let tick_raw = shifted.as_limbs()[0] as u32 & 0xFFFFFF;
    // Sign-extend from 24-bit
    let tick = if tick_raw & 0x800000 != 0 {
        (tick_raw | 0xFF000000) as i32
    } else {
        tick_raw as i32
    };
    (sqrt_price, tick)
}

/// Decode raw tick info storage slots into an `Info` struct.
///
/// slot0_val: liquidityGross(u128, lower 128 bits) | liquidityNet(i128, upper 128 bits)
/// slot3_val: initialized flag at bit 248
pub fn decode_v3_tick_info_raw(slot0_val: U256, slot3_val: U256) -> Info {
    let limbs = slot0_val.as_limbs();
    let liquidity_gross: u128 = limbs[0] as u128 | ((limbs[1] as u128) << 64);
    let liquidity_net: i128 = (limbs[2] as u128 | ((limbs[3] as u128) << 64)) as i128;
    let initialized = (slot3_val >> 248) & U256::from(1u64) != U256::ZERO;
    Info {
        liquidity_gross,
        liquidity_net,
        initialized,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_v2_reserves_round_trip() {
        let r0: u128 = 1_000_000_000_000; // 1 trillion
        let r1: u128 = 500_000_000;
        let packed = encode_v2_reserves_raw(r0, r1);
        let (decoded_r0, decoded_r1) = decode_v2_reserves_raw(packed);
        assert_eq!(decoded_r0, r0);
        assert_eq!(decoded_r1, r1);
    }

    #[test]
    fn test_v2_reserves_max_values() {
        // uint112 max = 2^112 - 1
        let max_112: u128 = (1u128 << 112) - 1;
        let packed = encode_v2_reserves_raw(max_112, max_112);
        let (decoded_r0, decoded_r1) = decode_v2_reserves_raw(packed);
        assert_eq!(decoded_r0, max_112);
        assert_eq!(decoded_r1, max_112);
    }

    #[test]
    fn test_v3_slot0_round_trip_positive_tick() {
        // Simulate a real slot0 value with observation fields set
        let observation_fields = U256::from(0xABCDu64) << 184; // some data in upper bits
        let original_price = U256::from(79228162514264337593543950336u128); // ~1.0 price
        let _original_tick: i32 = 100;
        let original_slot0 = observation_fields | original_price | (U256::from(100u32) << 160);

        let new_price = U256::from(79228162514264337593543950000u128);
        let new_tick: i32 = 200;
        let patched = encode_v3_slot0_patch(original_slot0, new_price, new_tick);

        let (decoded_price, decoded_tick) = decode_v3_slot0_raw(patched);
        assert_eq!(decoded_price, new_price);
        assert_eq!(decoded_tick, new_tick);

        // Verify observation fields preserved
        let lower_mask: U256 = (U256::from(1u64) << 184) - U256::from(1u64);
        assert_eq!(patched & !lower_mask, observation_fields);
    }

    #[test]
    fn test_v3_slot0_round_trip_negative_tick() {
        let observation_fields = U256::from(0x1234u64) << 184;
        let original_slot0 = observation_fields;

        let price = U256::from(12345678u64);
        let tick: i32 = -100;
        let patched = encode_v3_slot0_patch(original_slot0, price, tick);

        let (decoded_price, decoded_tick) = decode_v3_slot0_raw(patched);
        assert_eq!(decoded_price, price);
        assert_eq!(decoded_tick, tick);
    }

    #[test]
    fn test_v3_slot0_round_trip_extreme_negative_tick() {
        let observation_fields = U256::ZERO;
        let price = U256::from(4295128739u64); // MIN_SQRT_RATIO
        let tick: i32 = -887272; // MIN_TICK

        let patched = encode_v3_slot0_patch(observation_fields, price, tick);
        let (decoded_price, decoded_tick) = decode_v3_slot0_raw(patched);
        assert_eq!(decoded_price, price);
        assert_eq!(decoded_tick, tick);
    }
}
