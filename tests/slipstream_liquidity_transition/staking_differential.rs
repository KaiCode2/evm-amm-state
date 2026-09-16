//! Staking quote-state parity against the reviewed deployed pool bytecode.
use super::staking::*;
use super::*;
use evm_fork_cache::cache::EvmCache;
use revm::{
    context::result::ExecutionResult,
    state::{AccountInfo, Bytecode},
};

fn install(cache: &mut EvmCache, address: Address, hex: &str) {
    let code = Bytecode::new_raw(alloy_primitives::hex::decode(hex.trim()).unwrap().into());
    cache.db_mut().insert_account_info(
        address,
        AccountInfo {
            code_hash: code.hash_slow(),
            code: Some(code),
            ..Default::default()
        },
    );
    cache
        .db_mut()
        .replace_account_storage(address, Default::default())
        .unwrap();
}

fn position_slot(owner: Address) -> U256 {
    let mut packed = owner.as_slice().to_vec();
    packed.extend_from_slice(&topic_i24(-200).as_slice()[29..]);
    packed.extend_from_slice(&topic_i24(200).as_slice()[29..]);
    let mut mapping = keccak256(packed).to_vec();
    mapping.extend_from_slice(&U256::from(19).to_be_bytes::<32>());
    U256::from_be_slice(keccak256(mapping).as_slice())
}

#[tokio::test(flavor = "multi_thread")]
async fn continuous_staking_quote_state_matches_deployed_pool_execution() {
    let (fixture, mut derived, mut engine, _) = setup().await;
    let (_, mut reference, _, reference_provider) = setup().await;
    install(
        &mut reference,
        OP_POOL,
        include_str!("../fixtures/optimism_slipstream_proxy_runtime.hex"),
    );
    install(
        &mut reference,
        address!("c28ad28853a547556780bebf7847628501a3bcbb"),
        include_str!("../fixtures/optimism_slipstream_implementation_runtime.hex"),
    );
    reference
        .db_mut()
        .insert_account_info(GAUGE, AccountInfo::default());
    reference
        .db_mut()
        .insert_account_info(Address::ZERO, AccountInfo::default());
    for ((address, slot), value) in &fixture.state.0 {
        reference
            .db_mut()
            .insert_account_storage(*address, *slot, *value)
            .unwrap();
    }
    reference
        .db_mut()
        .insert_account_storage(OP_POOL, position_slot(NFT), U256::from(1_000))
        .unwrap();
    for (offset, (deposit, amount)) in [(true, 400_u128), (false, 400), (true, 250), (false, 250)]
        .into_iter()
        .enumerate()
    {
        let block = 100 + offset as u64;
        reference.set_timestamp(Some(1_000 + block));
        let delta = if deposit {
            U256::from(amount)
        } else {
            U256::ZERO.wrapping_sub(U256::from(amount))
        };
        let mut call = keccak256("stake(int128,int24,int24,bool)")[..4].to_vec();
        for word in [
            delta,
            U256::from_be_slice(topic_i24(-200).as_slice()),
            U256::from(200),
            U256::from(1),
        ] {
            call.extend_from_slice(&word.to_be_bytes::<32>());
        }
        let outcome = reference
            .call_raw(GAUGE, OP_POOL, call.into(), true)
            .unwrap();
        assert!(
            matches!(outcome, ExecutionResult::Success { .. }),
            "{outcome:?}"
        );
        let report = engine
            .ingest_batch(&mut derived, batch(staking_logs(deposit, amount), block))
            .unwrap();
        assert!(report.degraded_pools.is_empty());
        for slot in [U256::from(15), fixture.lower_keys[1], fixture.upper_keys[1]] {
            assert_eq!(
                derived.cached_storage_value(OP_POOL, slot).unwrap() & WORD_128_MASK,
                reference.cached_storage_value(OP_POOL, slot).unwrap() & WORD_128_MASK,
                "block {block}, slot {slot}"
            );
        }
    }
    assert_eq!(
        reference_provider.read_q().len(),
        1,
        "bytecode replay touched the provider"
    );
}
