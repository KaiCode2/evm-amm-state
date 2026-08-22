//! Acceptance tests for the fail-closed rejection of a **cross-venue**
//! registration: one whose [`PoolKey`] and [`ProtocolMetadata`] name different
//! protocols.
//!
//! The hazard these pin, as reported from the field: a `PoolRegistration` names
//! its venue twice. The `PoolKey` variant is what `PoolRegistration::protocol`
//! reports and therefore what every adapter dispatches on; the
//! `ProtocolMetadata` variant is what the storage layout is resolved from
//! (`storage::layout_for` matches on the metadata, not the key). Nothing used to
//! force the two to agree. A `PoolKey::UniswapV3` carrying
//! `ProtocolMetadata::Slipstream` was therefore cold-started against Slipstream's
//! slots (`slot0` at 6, liquidity at 16) while `simulate_swap` encoded canonical
//! Uniswap `quoteExactInputSingle` calldata — a `uint24 fee` struct — for a
//! quoter whose ABI expects an `int24 tickSpacing`. That reverts, or worse
//! returns a plausible-but-wrong quote, and the failure surfaces a long way from
//! the registration that caused it.
//!
//! These tests live in an external crate so they exercise exactly the surface a
//! consumer sees. They deliberately assert BOTH directions:
//!
//! - the contradictory combinations are refused, without mutating the registry;
//! - the combinations that merely *look* similar — an explicit non-default
//!   storage layout, `Unknown` metadata, the `Custom` extension hatch — keep
//!   working. A check that rejected those would be worse than no check.

use std::sync::Arc;

use alloy_primitives::{Address, B256, U256};

use evm_amm_state::adapters::storage::V3StorageLayout;
use evm_amm_state::adapters::{
    AdapterRegistry, CustomPoolKey, PoolKey, PoolRegistration, ProtocolId, ProtocolMetadata,
    RegistryError, V3Metadata,
};

fn addr(byte: u8) -> Address {
    Address::repeat_byte(byte)
}

/// The exact reported incident, at the registry boundary.
#[test]
fn uniswap_v3_key_carrying_slipstream_metadata_is_refused() {
    let key = PoolKey::UniswapV3(addr(0x11));
    // A plausible-looking Slipstream payload: tick spacing set, fee left unset
    // (Slipstream is tickSpacing-keyed and has no fee tier).
    let registration = PoolRegistration::new(key.clone()).with_metadata(
        ProtocolMetadata::Slipstream(V3Metadata::default().with_tick_spacing(100)),
    );

    let mut registry = AdapterRegistry::new();
    let error = registry
        .register_pool(registration)
        .expect_err("a Uniswap V3 key must not carry Slipstream metadata");

    let RegistryError::ProtocolMismatch(mismatch) = error else {
        panic!("expected RegistryError::ProtocolMismatch, got {error:?}");
    };
    assert_eq!(mismatch.key, key);
    assert_eq!(mismatch.key_protocol, ProtocolId::UniswapV3);
    assert_eq!(mismatch.metadata_protocol, ProtocolId::Slipstream);

    // Fails CLOSED: nothing was admitted, so nothing downstream can quote it.
    assert!(registry.pool(&key).is_none());
    assert!(registry.is_empty());
}

/// Every cross-venue pair a caller can build out of the V3 family, in both
/// directions. All three variants wrap `V3Metadata`, which is why the payload
/// type cannot be the signal — only the variant can.
#[test]
fn every_v3_family_cross_pair_is_refused() {
    let pool = addr(0x21);
    let families: [(PoolKey, ProtocolMetadata, ProtocolId); 3] = [
        (
            PoolKey::UniswapV3(pool),
            ProtocolMetadata::UniswapV3(V3Metadata::default()),
            ProtocolId::UniswapV3,
        ),
        (
            PoolKey::PancakeV3(pool),
            ProtocolMetadata::PancakeV3(V3Metadata::default()),
            ProtocolId::PancakeV3,
        ),
        (
            PoolKey::Slipstream(pool),
            ProtocolMetadata::Slipstream(V3Metadata::default()),
            ProtocolId::Slipstream,
        ),
    ];

    for (key, _, key_protocol) in &families {
        for (_, metadata, metadata_protocol) in &families {
            let registration = PoolRegistration::new(key.clone()).with_metadata(metadata.clone());
            let outcome = AdapterRegistry::new().register_pool(registration);
            if key_protocol == metadata_protocol {
                assert!(
                    outcome.is_ok(),
                    "{key_protocol:?} must accept its own metadata",
                );
            } else {
                assert!(
                    matches!(outcome, Err(RegistryError::ProtocolMismatch(_))),
                    "{key_protocol:?} key must refuse {metadata_protocol:?} metadata",
                );
            }
        }
    }
}

/// Cross-family pairs outside the V3 family, plus a `PoolKey::Custom` whose
/// third-party protocol name is not the venue a built-in metadata variant
/// describes.
#[test]
fn cross_family_pairs_are_refused() {
    let cases = [
        (
            PoolKey::UniswapV2(addr(0x31)),
            ProtocolMetadata::UniswapV3(V3Metadata::default()),
        ),
        (
            PoolKey::Curve(addr(0x32)),
            ProtocolMetadata::UniswapV3(V3Metadata::default()),
        ),
        (
            PoolKey::BalancerV2(B256::repeat_byte(0x33)),
            ProtocolMetadata::UniswapV3(V3Metadata::default()),
        ),
        (
            PoolKey::Custom(CustomPoolKey::Address {
                protocol: "acme-v1",
                address: addr(0x34),
            }),
            ProtocolMetadata::UniswapV3(V3Metadata::default()),
        ),
    ];

    for (key, metadata) in cases {
        let registration = PoolRegistration::new(key.clone()).with_metadata(metadata);
        assert!(
            matches!(
                AdapterRegistry::new().register_pool(registration),
                Err(RegistryError::ProtocolMismatch(_)),
            ),
            "{key:?} must refuse another venue's metadata",
        );
    }
}

/// The checked builder reports the mistake at the point it is made, rather than
/// deferring to registration.
#[test]
fn try_with_metadata_reports_the_mismatch_at_the_call_site() {
    let key = PoolKey::Slipstream(addr(0x41));
    let error = PoolRegistration::new(key)
        .try_with_metadata(ProtocolMetadata::UniswapV3(
            V3Metadata::default().with_fee(500),
        ))
        .expect_err("a Slipstream key must not carry Uniswap V3 metadata");

    assert_eq!(error.key_protocol, ProtocolId::Slipstream);
    assert_eq!(error.metadata_protocol, ProtocolId::UniswapV3);
    // The message names both venues: the original failure's whole problem was
    // surfacing far from its cause.
    let rendered = error.to_string();
    assert!(rendered.contains("Slipstream"), "{rendered}");
    assert!(rendered.contains("UniswapV3"), "{rendered}");
}

// --- what must KEEP working ---

/// The deliberate case the check must not catch: a caller supplying an explicit
/// `storage_layout` that differs from the family default, because their fork
/// lays its slots out differently. That names one venue and overrides its slots.
/// It stays permitted even when the slots supplied are literally another
/// family's preset — the metadata variant, not the slot values, is what names a
/// venue.
#[test]
fn explicit_non_default_storage_layout_still_registers() {
    let mut registry = AdapterRegistry::new();

    registry
        .register_pool(
            PoolRegistration::new(PoolKey::UniswapV3(addr(0x51))).with_metadata(
                ProtocolMetadata::UniswapV3(
                    V3Metadata::default()
                        .with_tick_spacing(100)
                        .with_storage_layout(V3StorageLayout::slipstream(100)),
                ),
            ),
        )
        .expect("an explicit fork layout under the matching family variant");

    registry
        .register_pool(
            PoolRegistration::new(PoolKey::UniswapV3(addr(0x52))).with_metadata(
                ProtocolMetadata::UniswapV3(
                    V3Metadata::default()
                        .with_tick_spacing(7)
                        .with_storage_layout(V3StorageLayout::new(
                            U256::from(41),
                            U256::from(42),
                            U256::from(43),
                            U256::from(44),
                            7,
                        )),
                ),
            ),
        )
        .expect("hand-built slots matching no preset");

    assert_eq!(registry.len(), 2);
}

/// `Unknown` is the `PoolRegistration::new` default and the state every pool is
/// in before cold-start fills its metadata. If the check rejected it, no
/// registration could ever be made in the normal order.
#[test]
fn unknown_metadata_still_registers_for_every_key() {
    let keys = [
        PoolKey::UniswapV2(addr(0x61)),
        PoolKey::UniswapV3(addr(0x62)),
        PoolKey::PancakeV3(addr(0x63)),
        PoolKey::Slipstream(addr(0x64)),
        PoolKey::SolidlyV2(addr(0x65)),
        PoolKey::BalancerV2(B256::repeat_byte(0x66)),
        PoolKey::Curve(addr(0x67)),
        PoolKey::Custom(CustomPoolKey::Address {
            protocol: "acme-v1",
            address: addr(0x68),
        }),
    ];
    let count = keys.len();

    let mut registry = AdapterRegistry::new();
    for key in keys {
        registry
            .register_pool(PoolRegistration::new(key.clone()))
            .unwrap_or_else(|error| panic!("{key:?} with Unknown metadata: {error}"));
    }
    assert_eq!(registry.len(), count);
}

/// The third-party hatch: `Custom` metadata is an opaque `dyn Any` the crate
/// cannot read, so it names no venue and cannot contradict any key. This is the
/// pairing `tests/custom_adapter.rs` registers, plus the built-in-key variant.
#[test]
fn custom_metadata_still_registers() {
    let mut registry = AdapterRegistry::new();

    registry
        .register_pool(
            PoolRegistration::new(PoolKey::Custom(CustomPoolKey::Address {
                protocol: "acme-v1",
                address: addr(0x71),
            }))
            .with_metadata(ProtocolMetadata::Custom(Arc::new(0u8))),
        )
        .expect("a custom key with its own opaque metadata");

    registry
        .register_pool(
            PoolRegistration::new(PoolKey::UniswapV3(addr(0x72)))
                .with_metadata(ProtocolMetadata::Custom(Arc::new(0u8))),
        )
        .expect("an opaque payload on a built-in key names no second venue");

    assert_eq!(registry.len(), 2);
}

/// The V3-family adapter is the place the wrong calldata was actually encoded,
/// and it can be driven without a registry. The guard there fails closed before
/// the cache is touched at all.
#[cfg(feature = "uniswap-v3")]
mod adapter_backstop {
    use super::{V3Metadata, addr};

    use alloy_primitives::{Address, Bytes, U256};
    use evm_amm_state::adapters::{
        AdapterCache, AmmAdapter, CacheError, CallOutcome, ConcentratedLiquidityAdapter, PoolKey,
        PoolRegistration, ProtocolMetadata, SimConfig, SlotChange, StateDiff, StateUpdate,
        StateView,
    };

    /// A cache that refuses to be used at all. Every method raises the SAME
    /// panic, so a test can assert "the adapter got as far as touching state"
    /// without depending on which access happens first.
    struct UntouchableCache;

    /// The single signal that the adapter proceeded past its registration guard.
    const TOUCHED: &str = "cache was touched";

    impl StateView for UntouchableCache {
        fn storage(&self, _address: Address, _slot: U256) -> Option<U256> {
            panic!("{TOUCHED}");
        }
    }

    impl AdapterCache for UntouchableCache {
        fn cached_storage(&self, _address: Address, _slot: U256) -> Option<U256> {
            panic!("{TOUCHED}");
        }
        fn apply_updates(&mut self, _updates: &[StateUpdate]) -> StateDiff {
            panic!("{TOUCHED}");
        }
        fn verify_slots(
            &mut self,
            _slots: &[(Address, U256)],
        ) -> Result<Vec<SlotChange>, CacheError> {
            panic!("{TOUCHED}");
        }
        fn purge_storage(&mut self, _address: Address) -> StateDiff {
            panic!("{TOUCHED}");
        }
        fn purge_slots(&mut self, _address: Address, _slots: &[U256]) -> StateDiff {
            panic!("{TOUCHED}");
        }
        fn read_storage_slot(
            &mut self,
            _address: Address,
            _slot: U256,
        ) -> Result<U256, CacheError> {
            panic!("{TOUCHED}");
        }
        fn call_raw(
            &mut self,
            _from: Address,
            _to: Address,
            _calldata: Bytes,
            _commit: bool,
        ) -> Result<CallOutcome, CacheError> {
            panic!("{TOUCHED}");
        }
    }

    /// Holding the adapter directly bypasses the registry, so the adapter itself
    /// has to fail closed too. Without the guard, this registration encodes a
    /// `uint24 fee` Uniswap quote (the key says UniswapV3) using metadata that
    /// describes a Slipstream pool.
    #[test]
    fn simulate_swap_refuses_a_cross_venue_registration() {
        let pool = PoolRegistration::new(PoolKey::UniswapV3(addr(0x81))).with_metadata(
            ProtocolMetadata::Slipstream(
                V3Metadata::default()
                    .with_tick_spacing(100)
                    // A fee is present, so the Uniswap encoding would have
                    // succeeded and produced a wrong quote rather than erroring.
                    .with_fee(500),
            ),
        );

        let error = ConcentratedLiquidityAdapter::default()
            .simulate_swap(
                &pool,
                &mut UntouchableCache,
                addr(0x01),
                addr(0x02),
                U256::from(1_000_u64),
                &SimConfig::default(),
            )
            .expect_err("the adapter must refuse a cross-venue registration");

        let rendered = error.to_string();
        assert!(rendered.contains("UniswapV3"), "{rendered}");
        assert!(rendered.contains("Slipstream"), "{rendered}");
    }

    /// The matching registration must still get through the guard and on to the
    /// quote path — proving the guard rejects only the contradiction, not V3
    /// quoting in general. Reaching the cache is the success signal here.
    #[test]
    #[should_panic(expected = "cache was touched")]
    fn simulate_swap_still_proceeds_for_a_matching_registration() {
        let pool = PoolRegistration::new(PoolKey::UniswapV3(addr(0x82))).with_metadata(
            ProtocolMetadata::UniswapV3(V3Metadata::default().with_tick_spacing(60).with_fee(500)),
        );

        let _ = ConcentratedLiquidityAdapter::default().simulate_swap(
            &pool,
            &mut UntouchableCache,
            addr(0x01),
            addr(0x02),
            U256::from(1_000_u64),
            &SimConfig::default(),
        );
    }
}
