//! Focused execution-state regressions; production-shaped fixtures follow separately.
use super::*;
use std::collections::BTreeMap;

struct View(BTreeMap<U256, U256>);
impl StateView for View {
    fn storage(&self, address: Address, slot: U256) -> Option<U256> {
        (address == POOL)
            .then(|| self.0.get(&slot).copied())
            .flatten()
    }
    fn verified_code_hash(&self, chain_id: u64, address: Address) -> Option<B256> {
        let reviewed =
            reviewed_aero_volatile_runtime(AeroVolatileRuntimeFamily::AeroOptimismMooBifi);
        if chain_id != 10 {
            return None;
        }
        if address == POOL {
            Some(reviewed.proxy_runtime_code_hash)
        } else if address == reviewed.implementation {
            Some(reviewed.implementation_runtime_code_hash)
        } else {
            None
        }
    }
}

fn baseline(time: u64) -> View {
    View(
        [
            (2, 1_000_000),
            (RESERVE0, 100),
            (RESERVE1, 200),
            (LAST_TIME, time),
            (CUMULATIVE0, 1_000),
            (CUMULATIVE1, 2_000),
            (INDEX0, 30),
            (INDEX1, 40),
            (ORACLE_META, (2 << 32) | (1 << 16)),
            (11, time),
            (14, 1),
        ]
        .into_iter()
        .map(|(k, v)| (U256::from(k), U256::from(v)))
        .collect(),
    )
}

fn value(updates: &[StateUpdate], slot: u64) -> Option<U256> {
    updates.iter().find_map(|u| match u {
        StateUpdate::Slot {
            slot: key, value, ..
        } if *key == U256::from(slot) => Some(*value),
        _ => None,
    })
}

#[test]
fn sync_updates_cumulatives_and_grows_exactly_one_observation_after_strict_period() {
    let view = baseline(100);
    let boundary = oracle::sync(&view, 160, U256::from(90), U256::from(230)).unwrap();
    assert_eq!(value(&boundary, CUMULATIVE0), Some(U256::from(7_000)));
    assert_eq!(value(&boundary, ORACLE_META), None);
    let next = oracle::sync(&view, 161, U256::from(90), U256::from(230)).unwrap();
    assert_eq!(value(&next, CUMULATIVE1), Some(U256::from(14_200)));
    assert_eq!(value(&next, 14), Some(U256::from(161)));
    assert_eq!(value(&next, 15), Some(U256::from(7_100)));
    assert_eq!(
        value(&next, ORACLE_META),
        Some(U256::from((2_u64 << 32) | (2 << 16) | 1))
    );
}

#[test]
fn timestamp_wrap_and_missing_cells_never_default_to_zero() {
    let time = u64::from(u32::MAX) - 20;
    let view = baseline(time);
    let updates = oracle::sync(&view, time + 61, U256::ONE, U256::ONE).unwrap();
    assert_eq!(value(&updates, 14), Some(U256::from(40)));
    let mut absent = baseline(100);
    absent.0.remove(&U256::from(14));
    assert!(oracle::sync(&absent, 161, U256::ONE, U256::ONE).is_ok());
    absent.0.remove(&U256::from(11));
    assert!(oracle::sync(&absent, 161, U256::ONE, U256::ONE).is_err());
    assert!(oracle::sync(&view, time - 1, U256::ONE, U256::ONE).is_err());
}

#[test]
fn fee_index_and_supply_use_checked_integer_arithmetic() {
    let view = baseline(100);
    assert_eq!(
        value(
            &fee_indexes(&view, U256::from(3), U256::ZERO).unwrap(),
            INDEX0
        ),
        Some(U256::from(3_000_000_000_030_u64))
    );
    assert!(fee_indexes(&view, U256::MAX, U256::ZERO).is_err());
    assert!(fee_indexes(&view, U256::ZERO, U256::ZERO).is_err());
    assert!(total_supply(&view, Address::ZERO, Address::ZERO, U256::ONE).is_err());
    assert!(total_supply(&view, Address::repeat_byte(1), Address::ZERO, U256::MAX).is_err());
}

mod canonical;
mod tape_ambiguity;

#[test]
fn reserve_oracle_growth_wrap_and_invalid_geometry_are_explicit() {
    let mut view = baseline(100);
    view.0.insert(U256::from(17), U256::ZERO);
    let growth = oracle::grow(&view, 2, 3).unwrap();
    assert_eq!(value(&growth, 17), Some(U256::ONE));
    assert_eq!(value(&growth, 18), Some(U256::ONE));
    assert!(oracle::grow(&view, 1, 3).is_err());
    assert!(oracle::grow(&view, 2, 2).is_err());
    assert!(oracle::grow(&view, 2, 4).is_ok());
    view.0.insert(U256::from(17), U256::ONE << 200_usize);
    assert!(oracle::grow(&view, 2, 3).is_err());
    view.0.insert(
        U256::from(ORACLE_META),
        U256::from((65_535_u64 << 32) | (65_535 << 16) | 65_534),
    );
    view.0.insert(U256::from(11 + 3 * 65_534), U256::from(100));
    let wrapped = oracle::sync(&view, 161, U256::ONE, U256::ONE).unwrap();
    assert_eq!(value(&wrapped, 11), Some(U256::from(161)));
    assert_eq!(
        value(&wrapped, ORACLE_META).unwrap() & U256::from(u16::MAX),
        U256::ZERO
    );
    view.0.insert(U256::from(ORACLE_META), U256::ZERO);
    assert!(oracle::sync(&view, 161, U256::ONE, U256::ONE).is_err());
}

#[test]
fn reserve_zero_and_overflow_match_solidity_checked_arithmetic() {
    let mut view = baseline(100);
    view.0.insert(U256::from(RESERVE0), U256::ZERO);
    let zero = oracle::sync(&view, 161, U256::ONE, U256::ONE).unwrap();
    assert_eq!(value(&zero, CUMULATIVE1), Some(U256::from(2_000)));
    view.0.insert(U256::from(RESERVE0), U256::MAX);
    assert!(oracle::sync(&view, 161, U256::ONE, U256::ONE).is_err());
    view.0.insert(U256::from(2), U256::ZERO);
    assert!(fee_indexes(&view, U256::ONE, U256::ZERO).is_err());
}

#[test]
fn fresh_allocation_requires_runtime_and_rejects_reused_or_contradictory_cells() {
    struct Unattested<'a>(&'a View);
    impl StateView for Unattested<'_> {
        fn storage(&self, address: Address, slot: U256) -> Option<U256> {
            self.0.storage(address, slot)
        }
    }
    let mut view = baseline(100);
    assert!(oracle::grow(&Unattested(&view), 2, 3).is_err());
    assert!(oracle::grow(&view, 2, 3).is_ok());
    assert!(oracle::grow(&view, 1, 3).is_err());
    assert!(oracle::grow(&view, 3, 4).is_err());
    view.0.insert(U256::from(15), U256::from(7));
    assert!(oracle::sync(&view, 161, U256::ONE, U256::ONE).is_err());
    view.0.insert(
        U256::from(ORACLE_META),
        U256::from((2_u64 << 32) | (2 << 16) | 1),
    );
    view.0.insert(U256::from(14), U256::from(100));
    view.0.remove(&U256::from(11));
    assert!(oracle::sync(&view, 161, U256::ONE, U256::ONE).is_err());
}
