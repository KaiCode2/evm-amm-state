//! Non-authoritative local-EVM research corpus for the two deployed BIFI
//! Slipstream pool runtimes. The real proxy + implementation execute each
//! generated swap; only the factory fee getters are replaced by a deterministic
//! local runtime so direction, exactness, price-limit, and fee inference cases
//! can be varied without provider or transaction-origin policy dependencies.
//! The public adapter must still report `Unsupported` and purge: this matrix is
//! intentionally narrower than the family-parity gate required for `Exact`.

use std::{collections::BTreeMap, str::FromStr, sync::Arc};

use alloy_eips::BlockId;
use alloy_primitives::{Address, B256, Bytes, Log, U256, address, b256, hex, keccak256};
use alloy_provider::{RootProvider, network::AnyNetwork};
use alloy_rpc_client::RpcClient;
use alloy_transport::mock::Asserter;
use anyhow::{Context, Result, anyhow};
use evm_amm_state::adapters::{
    AdapterEventContext, AmmAdapter, ConcentratedLiquidityAdapter, PoolKey, PoolRegistration,
    ProtocolMetadata, SlipstreamRuntimeFamily, SlipstreamSnapshotIdentity,
    SlipstreamSwapFeeEvidence, StateUpdate, StateView, UpdateQuality, V3Metadata, V3StorageLayout,
    V3SwapTransitionCapability,
};
use evm_fork_cache::cache::EvmCache;
use revm::{
    context::result::ExecutionResult,
    state::{AccountInfo, Bytecode},
};
use serde_json::Value;

const CALLER: Address = address!("00000000000000000000000000000000000000c1");
const HARNESS: Address = address!("00000000000000000000000000000000000000c6");
const BLOCK_HASH: B256 = b256!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
const PARENT_HASH: B256 = b256!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
const MIN_SQRT_LIMIT: U256 = U256::from_limbs([4_295_128_740, 0, 0, 0]);
const MAX_SQRT_LIMIT: U256 = U256::from_limbs([
    6_743_328_256_752_651_557,
    17_280_870_778_742_802_505,
    4_294_805_859,
    0,
]);
const SQRT_MASK: U256 = U256::from_limbs([u64::MAX, u64::MAX, u32::MAX as u64, 0]);

#[derive(Clone)]
struct ParentState(BTreeMap<(Address, U256), U256>);

impl StateView for ParentState {
    fn storage(&self, address: Address, slot: U256) -> Option<U256> {
        self.0.get(&(address, slot)).copied()
    }
}

#[derive(Clone, Copy)]
struct FamilySpec {
    family: SlipstreamRuntimeFamily,
    chain_id: u64,
    pool: Address,
    factory: Address,
    implementation: Address,
    proxy_hash: B256,
    implementation_hash: B256,
    fixture: &'static str,
    proxy_runtime: &'static str,
    implementation_runtime: &'static str,
}

fn specs() -> [FamilySpec; 2] {
    [
        FamilySpec {
            family: SlipstreamRuntimeFamily::AerodromeBaseBifi,
            chain_id: 8_453,
            pool: address!("b378137c90444bbcecd44a1f766851fbf53d2a9e"),
            factory: address!("5e7bb104d84c7cb9b682aac2f3d509f5f406809a"),
            implementation: address!("ec8e5342b19977b4ef8892e02d8daecfa1315831"),
            proxy_hash: b256!("acd6710f7037ad095b1e4d5f8ee5b2681069cb4dd316e77e4e0cb8f85716a2a1"),
            implementation_hash: b256!(
                "772fb5c610b40a122036f544e5b9b5bce6becb19db9524331289d1aaed2d5888"
            ),
            fixture: include_str!("fixtures/base_slipstream_initialized_crossing.json"),
            proxy_runtime: include_str!("fixtures/base_slipstream_proxy_runtime.hex"),
            implementation_runtime: include_str!(
                "fixtures/base_slipstream_implementation_runtime.hex"
            ),
        },
        FamilySpec {
            family: SlipstreamRuntimeFamily::VelodromeOptimismBifi,
            chain_id: 10,
            pool: address!("173cdc71e29d5cffa6d090ad99f555a24b8831f9"),
            factory: address!("cc0bddb707055e04e497ab22a59c2af4391cd12f"),
            implementation: address!("c28ad28853a547556780bebf7847628501a3bcbb"),
            proxy_hash: b256!("063ca35333cb7f2463f087d40ff9485475550abf4858a2f63c387d4d102b0f4f"),
            implementation_hash: b256!(
                "36c3da904ca0b58544254cd0d978fe4801c32dc1f9e3b3e644487ef541299794"
            ),
            fixture: include_str!("fixtures/optimism_slipstream_initialized_crossing.json"),
            proxy_runtime: include_str!("fixtures/optimism_slipstream_proxy_runtime.hex"),
            implementation_runtime: include_str!(
                "fixtures/optimism_slipstream_implementation_runtime.hex"
            ),
        },
    ]
}

#[derive(Clone, Copy)]
struct Case {
    name: &'static str,
    zero_for_one: bool,
    exact_input: bool,
    fee: u32,
    partial_limit: bool,
}

async fn cache(chain_id: u64) -> EvmCache {
    let asserter = Asserter::new();
    let client = RpcClient::mocked(asserter);
    let provider = RootProvider::<AnyNetwork>::new(client);
    let mut cache = EvmCache::new(Arc::new(provider)).await;
    cache.set_chain_id(chain_id);
    cache
}

fn runtime(value: &str) -> Bytes {
    Bytes::from(hex::decode(value.trim()).expect("checked-in runtime hex"))
}

fn install(cache: &mut EvmCache, address: Address, code: Bytes, slots: &[(U256, U256)]) {
    let code = Bytecode::new_raw(code);
    let code_hash = code.hash_slow();
    cache.db_mut().insert_account_info(
        address,
        AccountInfo {
            balance: U256::ZERO,
            nonce: 1,
            code: Some(code),
            code_hash,
            account_id: None,
        },
    );
    cache
        .db_mut()
        .replace_account_storage(address, Default::default())
        .unwrap();
    for (slot, value) in slots {
        cache
            .db_mut()
            .insert_account_storage(address, *slot, *value)
            .unwrap();
    }
}

fn word(value: &str) -> U256 {
    U256::from_str(value).expect("fixture word")
}

fn address_word(address: Address) -> U256 {
    U256::from_be_slice(address.as_slice())
}

fn mapping_slot(address: Address, slot: U256) -> U256 {
    let mut encoded = [0_u8; 64];
    encoded[12..32].copy_from_slice(address.as_slice());
    encoded[32..].copy_from_slice(&slot.to_be_bytes::<32>());
    U256::from_be_slice(keccak256(encoded).as_slice())
}

fn signed_word(value: i128) -> U256 {
    if value >= 0 {
        U256::from(value as u128)
    } else {
        U256::ZERO.wrapping_sub(U256::from(value.unsigned_abs()))
    }
}

fn execute_calldata(pool: Address, zero_for_one: bool, amount: i128, limit: U256) -> Bytes {
    let mut data = Vec::with_capacity(4 + 32 * 4);
    data.extend_from_slice(&keccak256("execute(address,bool,int256,uint160)")[..4]);
    data.extend_from_slice(&address_word(pool).to_be_bytes::<32>());
    data.extend_from_slice(&U256::from(zero_for_one as u8).to_be_bytes::<32>());
    data.extend_from_slice(&signed_word(amount).to_be_bytes::<32>());
    data.extend_from_slice(&limit.to_be_bytes::<32>());
    Bytes::from(data)
}

async fn run_case(spec: FamilySpec, case: Case, sequence: u64) -> Result<()> {
    let fixture: Value = serde_json::from_str(spec.fixture)?;
    let spacing = fixture["pool"]["tick_spacing"].as_i64().unwrap() as i32;
    let layout = V3StorageLayout::slipstream(spacing);
    let mut slots: Vec<(U256, U256)> = fixture["parent_storage"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(slot, value)| (word(slot), word(value.as_str().unwrap())))
        .collect();
    let liquidity_word = slots
        .iter()
        .find(|(slot, _)| *slot == U256::from(16))
        .unwrap()
        .1;
    let liquidity = liquidity_word & ((U256::from(1) << 128_usize) - U256::from(1));
    let staked = slots
        .iter_mut()
        .find(|(slot, _)| *slot == U256::from(15))
        .unwrap();
    staked.1 = (staked.1 & !((U256::from(1) << 128_usize) - U256::from(1))) | liquidity;
    let slot0 = slots
        .iter()
        .find(|(slot, _)| *slot == U256::from(6))
        .unwrap()
        .1;
    let start_sqrt = slot0 & SQRT_MASK;
    let limit = if case.partial_limit {
        let delta = start_sqrt / U256::from(10_000);
        if case.zero_for_one {
            start_sqrt - delta
        } else {
            start_sqrt + delta
        }
    } else if case.zero_for_one {
        MIN_SQRT_LIMIT
    } else {
        MAX_SQRT_LIMIT
    };
    let base_amount = match (spec.chain_id, case.zero_for_one, case.exact_input) {
        (10, true, true) => 1_000_000_i128,
        (10, true, false) => 1_000_000_000_000_000_000_i128,
        (10, false, true) => 10_000_000_000_000_000_i128,
        (10, false, false) => 1_000_000_i128,
        (_, _, _) => 10_000_000_000_000_000_i128,
    };
    let magnitude = if case.partial_limit {
        i128::MAX / 4
    } else {
        base_amount
    };
    let amount = if case.exact_input {
        magnitude
    } else {
        -magnitude
    };

    let parent = ParentState(
        slots
            .iter()
            .map(|(slot, value)| ((spec.pool, *slot), *value))
            .collect(),
    );
    let mut reference = cache(spec.chain_id).await;
    reference.set_block(BlockId::from((BLOCK_HASH, Some(true))));
    reference.set_timestamp(Some(1_800_000_000 + sequence));
    reference.set_block_context(Some(1_000 + sequence), Some(0));
    reference
        .db_mut()
        .cache
        .block_hashes
        .insert(U256::from(999 + sequence), PARENT_HASH);
    reference
        .db_mut()
        .insert_account_info(CALLER, AccountInfo::default());
    reference
        .db_mut()
        .insert_account_info(Address::ZERO, AccountInfo::default());
    install(
        &mut reference,
        spec.pool,
        runtime(spec.proxy_runtime),
        &slots,
    );
    install(
        &mut reference,
        spec.implementation,
        runtime(spec.implementation_runtime),
        &[],
    );
    install(
        &mut reference,
        spec.factory,
        runtime(include_str!(
            "fixtures/slipstream_reference_factory_runtime.hex"
        )),
        &[(
            U256::ZERO,
            U256::from(case.fee) | (U256::from(100_000) << 24_usize),
        )],
    );
    let token0 = Address::from_slice(
        &slots
            .iter()
            .find(|(slot, _)| *slot == U256::from(1))
            .unwrap()
            .1
            .to_be_bytes::<32>()[12..],
    );
    let token1 = Address::from_slice(
        &slots
            .iter()
            .find(|(slot, _)| *slot == U256::from(2))
            .unwrap()
            .1
            .to_be_bytes::<32>()[12..],
    );
    let token_code = runtime(include_str!("fixtures/reference_swap_token_runtime.hex"));
    let token_slots = [(
        mapping_slot(spec.pool, U256::ZERO),
        U256::from(1) << 200_usize,
    )];
    install(&mut reference, token0, token_code.clone(), &token_slots);
    install(&mut reference, token1, token_code, &token_slots);
    install(
        &mut reference,
        HARNESS,
        runtime(include_str!(
            "fixtures/v3_reference_swap_harness_runtime.hex"
        )),
        &[
            (U256::ZERO, address_word(token0)),
            (U256::from(1), address_word(token1)),
        ],
    );

    let transaction_hash = B256::from(U256::from(10_000 + sequence).to_be_bytes::<32>());
    let base_context =
        AdapterEventContext::for_block(1_000 + sequence, BLOCK_HASH, 1_800_000_000 + sequence)
            .with_chain_id(spec.chain_id)
            .with_parent_hash(PARENT_HASH)
            .with_transaction_hash(transaction_hash)
            .with_event_order(1, 2);
    let identity = SlipstreamSnapshotIdentity::new(
        spec.chain_id,
        1_000 + sequence,
        BLOCK_HASH,
        PARENT_HASH,
        1_800_000_000 + sequence,
        transaction_hash,
        1,
        2,
    )?;
    let candidate = ConcentratedLiquidityAdapter::evaluate_slipstream_all_staked_candidate(
        spec.family,
        reference.snapshot(),
        identity,
        &base_context,
    )?;

    let calldata = execute_calldata(spec.pool, case.zero_for_one, amount, limit);
    let (_, access) = reference.call_raw_with_access_list(CALLER, HARNESS, calldata.clone())?;
    let result = reference.call_raw(CALLER, HARNESS, calldata, true)?;
    let logs = match result {
        ExecutionResult::Success { logs, .. } => logs,
        other => {
            return Err(anyhow!(
                "{} reference execution failed: {other:?}",
                case.name
            ));
        }
    };
    let topic = keccak256("Swap(address,address,int256,int256,uint160,uint128,int24)");
    let log: Log = logs
        .into_iter()
        .find(|log| log.address == spec.pool && log.topics().first() == Some(&topic))
        .ok_or_else(|| anyhow!("{} emitted no Swap", case.name))?;
    let registration = PoolRegistration::new(PoolKey::Slipstream(spec.pool))
        .with_state_address(spec.pool)
        .with_metadata(ProtocolMetadata::Slipstream(
            V3Metadata::default()
                .with_tick_spacing(spacing)
                .with_storage_layout(layout),
        ));
    let evidence = SlipstreamSwapFeeEvidence::new(
        spec.family,
        spec.chain_id,
        spec.pool,
        spec.factory,
        spec.proxy_hash,
        spec.implementation,
        spec.implementation_hash,
        case.fee,
        candidate.effective_fee(),
        candidate.proof(),
        1_000 + sequence,
        BLOCK_HASH,
        PARENT_HASH,
        1_800_000_000 + sequence,
        transaction_hash,
        1,
        2,
    )?;
    let context = base_context.with_slipstream_fee_evidence(evidence);
    let inferred = ConcentratedLiquidityAdapter::infer_slipstream_swap_fee(
        &registration,
        &log,
        &parent,
        &context,
    )
    .with_context(|| format!("{} {:?} infer fee", case.name, spec.family))?;
    assert_eq!(inferred, case.fee, "{} inferred fee", case.name);
    assert_eq!(
        ConcentratedLiquidityAdapter::swap_transition_capability_with_context(
            &registration,
            &context,
        ),
        V3SwapTransitionCapability::Unsupported,
    );
    let decoded = ConcentratedLiquidityAdapter::default().decode_event_with_context(
        &registration,
        &log,
        &parent,
        &context,
    );
    assert!(decoded.error.is_some(), "{} must fail closed", case.name);
    let event = decoded.event.expect("reference event");
    assert_eq!(event.quality, UpdateQuality::RequiresRepair);
    assert!(matches!(
        event.updates.as_slice(),
        [StateUpdate::Purge { .. }]
    ));
    assert!(
        access
            .slots
            .iter()
            .any(|(address, _)| *address == spec.pool),
        "reference execution must independently enumerate pool storage",
    );
    Ok(())
}

#[tokio::test]
async fn deployed_base_and_optimism_runtimes_match_fee_inference_and_fail_closed_matrix()
-> Result<()> {
    let cases = [
        Case {
            name: "zero-fee exact-input zero-for-one",
            zero_for_one: true,
            exact_input: true,
            fee: 0,
            partial_limit: false,
        },
        Case {
            name: "discount exact-output zero-for-one",
            zero_for_one: true,
            exact_input: false,
            fee: 1_000,
            partial_limit: false,
        },
        Case {
            name: "standard exact-input one-for-zero",
            zero_for_one: false,
            exact_input: true,
            fee: 10_000,
            partial_limit: false,
        },
        Case {
            name: "high-fee exact-output one-for-zero",
            zero_for_one: false,
            exact_input: false,
            fee: 30_000,
            partial_limit: false,
        },
        Case {
            name: "price-limit partial exact-input",
            zero_for_one: true,
            exact_input: true,
            fee: 10_000,
            partial_limit: true,
        },
    ];
    let mut sequence = 1;
    for spec in specs() {
        for case in cases {
            run_case(spec, case, sequence).await?;
            sequence += 1;
        }
    }
    Ok(())
}
