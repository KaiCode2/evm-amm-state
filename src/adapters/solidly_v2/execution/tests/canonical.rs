//! Exact canonical logs and EIP-1898 before/after words, including three swaps
//! at one block timestamp. The fixture contains no invented storage values.
use super::*;
use crate::adapters::PoolKey;

fn registration() -> PoolRegistration {
    PoolRegistration::new(PoolKey::SolidlyV2(POOL))
        .with_state_address(POOL)
        .with_metadata(ProtocolMetadata::SolidlyV2(
            SolidlyV2Metadata::default().with_aero_volatile(),
        ))
}

#[test]
fn canonical_pool_logs_reproduce_all_declared_execution_cells_without_rpc() {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("canonical_pool.json")).unwrap();
    assert_eq!(fixture["chain_id"], 10);
    let reviewed = reviewed_aero_volatile_runtime(AeroVolatileRuntimeFamily::AeroOptimismMooBifi);
    for (address, hash) in [
        (POOL, reviewed.proxy_runtime_code_hash),
        (
            reviewed.implementation,
            reviewed.implementation_runtime_code_hash,
        ),
    ] {
        assert_eq!(
            fixture["runtime_identities"][address.to_string().to_lowercase()]
                .as_str()
                .unwrap(),
            hash.to_string()
        );
    }
    let registration = registration();
    let mut syncs = 0;
    let mut fees = 0;
    for frame in fixture["frames"].as_array().unwrap() {
        let words = frame["before"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(slot, value)| {
                (
                    slot.parse().unwrap(),
                    value.as_str().unwrap().parse().unwrap(),
                )
            })
            .collect();
        let mut view = View(words);
        for encoded in frame["logs"].as_array().unwrap() {
            let log: alloy_rpc_types_eth::Log = serde_json::from_value(encoded.clone()).unwrap();
            assert_eq!(
                log.block_hash.unwrap().to_string(),
                frame["block_hash"].as_str().unwrap()
            );
            assert!(!log.removed);
            let context = AdapterEventContext::for_block(
                frame["block_number"].as_u64().unwrap(),
                log.block_hash.unwrap(),
                frame["timestamp"].as_u64().unwrap(),
            )
            .with_chain_id(10)
            .with_parent_hash(frame["parent_hash"].as_str().unwrap().parse().unwrap())
            .with_transaction_hash(log.transaction_hash.unwrap())
            .with_event_order(log.transaction_index.unwrap(), log.log_index.unwrap());
            if log.inner.topics()[0] == Sync::SIGNATURE_HASH {
                syncs += 1;
            }
            if log.inner.topics()[0] == abi::Fees::SIGNATURE_HASH {
                fees += 1;
            }
            if let Some(updates) = SolidlyV2Adapter::aero_volatile_execution_event_updates(
                &registration,
                &log.inner,
                &view,
                &context,
            )
            .unwrap()
            {
                for update in updates {
                    let StateUpdate::Slot {
                        address,
                        slot,
                        value,
                    } = update
                    else {
                        panic!("absolute atomic writes only")
                    };
                    assert_eq!(address, POOL);
                    view.0.insert(slot, value);
                }
            }
        }
        for (slot, expected) in frame["after"].as_object().unwrap() {
            assert_eq!(
                view.0.get(&slot.parse().unwrap()),
                Some(&expected.as_str().unwrap().parse::<U256>().unwrap()),
                "block {} slot {slot}",
                frame["block_number"]
            );
        }
    }
    assert!(syncs >= 5);
    assert_eq!(syncs, fees);
}

#[test]
fn canonical_first_observation_use_matches_even_when_untouched_prewarm_cells_are_absent() {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("canonical_pool.json")).unwrap();
    let mut tested = false;
    for frame in fixture["frames"].as_array().unwrap() {
        let before: U256 = frame["before"]["0x30008"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        let after: U256 = frame["after"]["0x30008"].as_str().unwrap().parse().unwrap();
        let cardinality = ((before >> 16_usize) & U256::from(u16::MAX)).to::<u16>();
        let new_cardinality = ((after >> 16_usize) & U256::from(u16::MAX)).to::<u16>();
        if new_cardinality <= cardinality {
            continue;
        }
        assert_eq!(new_cardinality, cardinality + 1);
        let mut view = View(
            frame["before"]
                .as_object()
                .unwrap()
                .iter()
                .map(|(s, v)| (s.parse().unwrap(), v.as_str().unwrap().parse().unwrap()))
                .collect(),
        );
        for slot in 11 + 3 * u64::from(cardinality)..14 + 3 * u64::from(cardinality) {
            assert_eq!(view.0.remove(&U256::from(slot)), Some(U256::ONE));
        }
        for encoded in frame["logs"].as_array().unwrap() {
            let log: alloy_rpc_types_eth::Log = serde_json::from_value(encoded.clone()).unwrap();
            if log.inner.topics()[0] != Sync::SIGNATURE_HASH {
                continue;
            }
            let event = Sync::decode_log_data_validate(&log.inner.data).unwrap();
            for update in oracle::sync(
                &view,
                frame["timestamp"].as_u64().unwrap(),
                event.reserve0,
                event.reserve1,
            )
            .unwrap()
            {
                let StateUpdate::Slot { slot, value, .. } = update else {
                    panic!("absolute write")
                };
                view.0.insert(slot, value);
            }
        }
        for slot in 11 + 3 * u64::from(cardinality)..14 + 3 * u64::from(cardinality) {
            assert_eq!(
                view.0.get(&U256::from(slot)).unwrap(),
                &frame["after"][format!("{slot:#x}")]
                    .as_str()
                    .unwrap()
                    .parse::<U256>()
                    .unwrap()
            );
        }
        tested = true;
    }
    assert!(
        tested,
        "canonical fixture must cross the real allocation frontier"
    );
}

#[test]
fn execution_surface_rejects_missing_or_wrong_lineage_and_registration() {
    let log = Log {
        address: POOL,
        data: Sync {
            reserve0: U256::ONE,
            reserve1: U256::ONE,
        }
        .encode_log_data(),
    };
    let good = AdapterEventContext::for_block(5, B256::repeat_byte(1), 110)
        .with_chain_id(10)
        .with_parent_hash(B256::repeat_byte(2))
        .with_transaction_hash(B256::repeat_byte(3))
        .with_event_order(0, 0);
    assert!(
        SolidlyV2Adapter::aero_volatile_execution_event_updates(
            &registration(),
            &log,
            &baseline(100),
            &good
        )
        .is_ok()
    );
    for bad in [
        AdapterEventContext::default(),
        good.with_chain_id(1),
        good.with_parent_hash(B256::ZERO),
        good.with_transaction_hash(B256::ZERO),
    ] {
        assert!(
            SolidlyV2Adapter::aero_volatile_execution_event_updates(
                &registration(),
                &log,
                &baseline(100),
                &bad
            )
            .is_err()
        );
    }
}
