use super::*;
use std::collections::BTreeMap;
struct View(BTreeMap<(Address, U256), U256>);
impl StateView for View {
    fn storage(&self, address: Address, slot: U256) -> Option<U256> {
        self.0.get(&(address, slot)).copied()
    }
}
fn apply<E: SolEvent>(
    address: Address,
    event: E,
    view: &View,
) -> Result<Option<Vec<StateUpdate>>, AdapterEventError> {
    event_updates(
        &Log {
            address,
            data: event.encode_log_data(),
        },
        view,
    )
}
fn result(updates: Vec<StateUpdate>) -> U256 {
    let [StateUpdate::Slot { value, .. }] = updates.as_slice() else {
        panic!("one absolute write")
    };
    *value
}
#[test]
fn fee_configuration_preserves_padding_and_rejects_unknown_modules() {
    let padding = U256::ONE << 250_usize;
    let view = View(
        [
            (7_u64, padding),
            (11, padding),
            (13, padding),
            (5, U256::from(30)),
        ]
        .into_iter()
        .map(|(s, v)| ((FACTORY, U256::from(s)), v))
        .collect(),
    );
    let event = abi::SetMevTaxModule {
        value: Address::ZERO,
        gasLimit: 15_000,
    };
    assert_eq!(
        result(apply(FACTORY, event, &view).unwrap().unwrap()),
        padding | (U256::from(15_000) << 160_usize)
    );
    assert!(
        apply(
            FACTORY,
            abi::SetFeeModule {
                value: Address::repeat_byte(7),
                gasLimit: 60_000
            },
            &view
        )
        .is_err()
    );
    assert!(
        apply(
            FACTORY,
            abi::SetPoolTape {
                value: TAPE,
                gasLimit: 0
            },
            &view
        )
        .is_err()
    );
    assert!(
        apply(
            FACTORY,
            abi::SetDefaultFee {
                value: U256::from(301)
            },
            &view
        )
        .is_err()
    );
    assert_eq!(
        result(
            apply(
                FACTORY,
                abi::SetDefaultFee {
                    value: U256::from(300)
                },
                &view
            )
            .unwrap()
            .unwrap()
        ),
        U256::from(300)
    );
    assert!(
        apply(
            FACTORY,
            abi::SetFeeManager {
                value: Address::repeat_byte(8)
            },
            &view
        )
        .is_err()
    );
}
#[test]
fn custom_fee_indicator_and_packed_mev_controls_are_exact() {
    let padding = U256::ONE << 250_usize;
    let key = solidity_address_mapping_key(POOL, 0);
    let view = View(
        [
            ((FEE, key), padding | U256::from(30)),
            ((MEV, U256::ONE), padding | U256::from(42)),
            (
                (MEV, U256::from(2)),
                padding | (U256::from(9) << 96_usize) | U256::from(7),
            ),
        ]
        .into(),
    );
    assert_eq!(
        result(
            apply(
                FEE,
                abi::SetCustomFee {
                    pool: POOL,
                    fee: U256::from(420)
                },
                &view
            )
            .unwrap()
            .unwrap()
        ),
        padding | U256::from(420)
    );
    assert!(
        apply(
            FEE,
            abi::SetCustomFee {
                pool: POOL,
                fee: U256::from(421)
            },
            &view
        )
        .is_err()
    );
    assert!(
        apply(
            FEE,
            abi::SetCustomFee {
                pool: Address::repeat_byte(1),
                fee: U256::from(420)
            },
            &view
        )
        .unwrap()
        .is_none()
    );
    assert_eq!(
        result(
            apply(MEV, abi::MultiplierSet { value: 65 }, &view)
                .unwrap()
                .unwrap()
        ),
        padding | U256::from(42) | (U256::from(65) << 160_usize)
    );
    assert_eq!(
        result(
            apply(
                MEV,
                abi::MinThresholdSet {
                    value: 11_u64.try_into().unwrap()
                },
                &view
            )
            .unwrap()
            .unwrap()
        ),
        padding | (U256::from(9) << 96_usize) | U256::from(11)
    );
}
