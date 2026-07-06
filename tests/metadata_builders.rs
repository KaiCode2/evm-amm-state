//! MANAGER-AUTHORED acceptance tests for the config-struct forward-compat
//! hardening: the protocol metadata structs are `#[non_exhaustive]` (so future
//! fields never break downstream struct literals) and therefore must be
//! constructable from *outside* the crate via `Default` + `with_*` builders.
//!
//! These tests live in a separate crate (integration test), so they exercise the
//! exact external surface a consumer sees: a `#[non_exhaustive]` struct cannot be
//! built with a struct literal here — only through the builder. The implementation
//! agent must make these pass WITHOUT modifying them.

use alloy_primitives::{Address, U256};

use evm_amm_state::adapters::storage::V3StorageLayout;
use evm_amm_state::adapters::{
    BalancerV2Metadata, CurveMetadata, CurveVariant, EventSource, PoolKey, PoolRegistration,
    SimConfig, SolidlyV2Metadata, UniswapV2Metadata, V3Metadata,
};

fn addr(b: u8) -> Address {
    Address::repeat_byte(b)
}

#[test]
fn uniswap_v2_metadata_builder_sets_fields() {
    let m = UniswapV2Metadata::default()
        .with_token0(addr(1))
        .with_token1(addr(2))
        .with_fee_bps(30);
    assert_eq!(m.token0, Some(addr(1)));
    assert_eq!(m.token1, Some(addr(2)));
    assert_eq!(m.fee_bps, Some(30));
}

#[test]
fn v3_metadata_builder_sets_fields_including_warm_radius() {
    let layout = V3StorageLayout::uniswap(60);
    let m = V3Metadata::default()
        .with_token0(addr(1))
        .with_token1(addr(2))
        .with_fee(500)
        .with_tick_spacing(10)
        .with_storage_layout(layout)
        .with_warm_word_radius(4);
    assert_eq!(m.token0, Some(addr(1)));
    assert_eq!(m.token1, Some(addr(2)));
    assert_eq!(m.fee, Some(500));
    assert_eq!(m.tick_spacing, Some(10));
    assert_eq!(m.storage_layout, Some(layout));
    assert_eq!(m.warm_word_radius, Some(4));
}

#[test]
fn v3_metadata_default_leaves_warm_radius_unset() {
    let m = V3Metadata::default();
    assert_eq!(m.warm_word_radius, None);
}

#[test]
fn solidly_v2_metadata_builder_sets_fields() {
    let m = SolidlyV2Metadata::default()
        .with_token0(addr(1))
        .with_token1(addr(2))
        .with_stable(true);
    assert_eq!(m.token0, Some(addr(1)));
    assert_eq!(m.token1, Some(addr(2)));
    assert_eq!(m.stable, Some(true));
}

#[test]
fn balancer_v2_metadata_builder_sets_fields() {
    let m = BalancerV2Metadata::default()
        .with_vault(addr(9))
        .with_pool_address(addr(8))
        .with_tokens([addr(1), addr(2)])
        .with_balance_slots([U256::from(3), U256::from(4)]);
    assert_eq!(m.vault, Some(addr(9)));
    assert_eq!(m.pool_address, Some(addr(8)));
    assert_eq!(m.tokens, vec![addr(1), addr(2)]);
    assert_eq!(m.balance_slots, vec![U256::from(3), U256::from(4)]);
}

#[test]
fn curve_metadata_builder_sets_fields() {
    let m = CurveMetadata::default()
        .with_coins([addr(1), addr(2), addr(3)])
        .with_discovered_slots([U256::from(7)])
        .with_variant(CurveVariant::CryptoSwapNG);
    assert_eq!(m.coins, vec![addr(1), addr(2), addr(3)]);
    assert_eq!(m.discovered_slots, vec![U256::from(7)]);
    assert_eq!(m.variant, CurveVariant::CryptoSwapNG);
}

// The already-builder-shaped config structs must remain externally constructable
// under `#[non_exhaustive]` via their existing constructors (guards the attribute
// from silently breaking consumers who use the intended API).
#[test]
fn config_structs_construct_via_existing_builders() {
    let _reg = PoolRegistration::new(PoolKey::UniswapV2(addr(1)))
        .with_state_address(addr(1))
        .with_metadata(evm_amm_state::adapters::ProtocolMetadata::UniswapV2(
            UniswapV2Metadata::default().with_fee_bps(30),
        ));
    let _cfg = SimConfig::default()
        .with_v3_quoter(addr(2))
        .with_v2_router(addr(3));
    let _src = EventSource::direct(addr(4), Vec::new());
}
