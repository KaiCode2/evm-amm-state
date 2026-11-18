//! Actual reviewed bytecode, synthetic fully declared parent storage. This is
//! evidence of the event-information boundary, not a tape-state approximation.
use super::*;
use alloy_provider::{RootProvider, network::AnyNetwork};
use alloy_rpc_client::RpcClient;
use alloy_transport::mock::Asserter;
use evm_fork_cache::cache::{EvmCache, TxConfig};
use revm::{
    context::result::ExecutionResult,
    state::{AccountInfo, Bytecode},
};
use std::sync::Arc;

const FACTORY: Address = address!("8e6a9fcd1d638cc760ffba397605fe3355de76d2");
const TAPE: Address = address!("34bdd64b19d8631f936c2d07f19aaa6b83515992");

async fn cache(cap: u64) -> EvmCache {
    let provider = RootProvider::<AnyNetwork>::new(RpcClient::mocked(Asserter::new()));
    let mut cache = EvmCache::new(Arc::new(provider)).await;
    cache.set_timestamp(Some(110));
    cache.set_block_context(Some(100), Some(0));
    for address in [Address::ZERO, POOL] {
        cache
            .db_mut()
            .insert_account_info(address, AccountInfo::default());
    }
    for (address, runtime, expected) in [
        (
            FACTORY,
            include_str!("factory_runtime.hex"),
            b256!("bb6f945479608857a220f9643db76e877f1e52844d953d980c18f5ea38d9574b"),
        ),
        (
            TAPE,
            include_str!("tape_runtime.hex"),
            b256!("dfff920fc935eb9b3c1f202ca7332e9131e3f0ea3c82dfa0d791f782ecd0163b"),
        ),
    ] {
        let code = Bytecode::new_raw(Bytes::from(
            alloy_primitives::hex::decode(runtime.trim()).unwrap(),
        ));
        assert_eq!(code.hash_slow(), expected);
        cache.db_mut().insert_account_info(
            address,
            AccountInfo {
                code_hash: expected,
                code: Some(code),
                nonce: 1,
                ..AccountInfo::default()
            },
        );
    }
    let mut words = vec![
        (
            FACTORY,
            U256::from(11),
            U256::from_be_slice(TAPE.as_slice()) | (U256::from(cap) << 160_usize),
        ),
        (FACTORY, solidity_address_mapping_key(POOL, 17), U256::ONE),
        (
            TAPE,
            solidity_address_mapping_key(POOL, 2),
            U256::from_be_slice(FACTORY.as_slice()) | (U256::from(60) << 160_usize),
        ),
    ];
    let accumulator = solidity_address_mapping_key(POOL, 3);
    let buffer = solidity_address_mapping_key(POOL, 4);
    for offset in 0..4 {
        words.push((TAPE, accumulator + U256::from(offset), U256::ONE));
    }
    words.push((
        TAPE,
        accumulator + U256::from(4),
        U256::from(100) | (U256::ONE << 48_usize),
    ));
    words.push((
        TAPE,
        buffer + U256::from(4),
        U256::from(100) | (U256::ONE << 48_usize),
    ));
    words.push((
        TAPE,
        buffer + U256::from(327_675),
        U256::from((1_u64 << 32) | (1 << 16)),
    ));
    for (address, slot, value) in words {
        cache
            .db_mut()
            .insert_account_storage(address, slot, value)
            .unwrap();
    }
    cache
}

fn calldata(direct: bool) -> Bytes {
    let signature = if direct {
        "record(address,(uint128,uint128,uint128,uint128,uint128,uint128,uint128,uint128))"
    } else {
        "recordPoolTape((uint128,uint128,uint128,uint128,uint128,uint128,uint128,uint128))"
    };
    let mut data = keccak256(signature)[..4].to_vec();
    if direct {
        data.extend_from_slice(&U256::from_be_slice(POOL.as_slice()).to_be_bytes::<32>());
    }
    for value in 1..=8 {
        data.extend_from_slice(&U256::from(value).to_be_bytes::<32>());
    }
    data.into()
}

fn success(result: ExecutionResult) -> Option<(u64, Vec<Log>, Bytes)> {
    match result {
        ExecutionResult::Success {
            gas_used,
            logs,
            output,
            ..
        } => Some((gas_used, logs, output.into_data())),
        _ => None,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn reviewed_factory_can_swallow_a_silent_tape_failure_with_identical_logs() {
    let input = calldata(true);
    let intrinsic = 21_000
        + input
            .iter()
            .map(|b| if *b == 0 { 4 } else { 16 })
            .sum::<u64>();
    let mut direct = cache(200_000).await;
    let (direct_gas, logs, _) =
        success(direct.call_raw(FACTORY, TAPE, input, true).unwrap()).unwrap();
    assert!(
        logs.is_empty(),
        "within-cadence record emits no observation"
    );
    let cap = direct_gas - intrinsic + 100;
    let accumulator = solidity_address_mapping_key(POOL, 3);
    let expected = direct.cached_storage_value(TAPE, accumulator).unwrap();
    assert_ne!(expected, U256::ONE);
    let mut ample = cache(cap).await;
    let ample_result = success(
        ample
            .call_raw_with(
                POOL,
                FACTORY,
                calldata(false),
                true,
                &TxConfig {
                    gas_limit: Some(300_000),
                    ..TxConfig::default()
                },
            )
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        ample.cached_storage_value(TAPE, accumulator),
        Some(expected)
    );
    let mut found = None;
    // The factory's gasleft > cap check does not guarantee the nested CALL
    // receives cap after ABI preparation, CALL cost and EIP-150 clipping.
    for gas in (cap + 21_000..cap + 40_000).step_by(100) {
        let mut bounded = cache(cap).await;
        let result = bounded
            .call_raw_with(
                POOL,
                FACTORY,
                calldata(false),
                true,
                &TxConfig {
                    gas_limit: Some(gas),
                    ..TxConfig::default()
                },
            )
            .unwrap();
        if let Some(result) = success(result)
            && bounded.cached_storage_value(TAPE, accumulator) == Some(U256::ONE)
        {
            found = Some((gas, result));
            break;
        }
    }
    let (gas, failed) =
        found.expect("a successful outer factory call with rolled-back silent tape state");
    assert_eq!(ample_result.1, failed.1);
    assert_eq!(ample_result.2, failed.2);
    assert!(failed.1.is_empty());
    assert_ne!(ample_result.0, failed.0);
    eprintln!(
        "reviewed tape ambiguity: cap={cap}, bounded_tx_gas={gas}, success_used={}, swallowed_failure_used={}",
        ample_result.0, failed.0
    );
}
