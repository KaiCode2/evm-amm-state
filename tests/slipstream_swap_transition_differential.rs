//! Local-EVM differential corpus for the two deployed BIFI
//! Slipstream pool runtimes. The real proxy + implementation execute each
//! generated swap; only the factory fee getters are replaced by a deterministic
//! local runtime so direction, exactness, price-limit, and fee inference cases
//! can be varied without provider or transaction-origin policy dependencies.

use std::{collections::BTreeMap, str::FromStr, sync::Arc};

use alloy_eips::BlockId;
use alloy_primitives::{Address, B256, Bytes, Log, U256, address, b256, hex, keccak256};
use alloy_provider::{RootProvider, network::AnyNetwork};
use alloy_rpc_client::RpcClient;
use alloy_transport::mock::Asserter;
use anyhow::{Context, Result, anyhow};
use evm_amm_state::adapters::storage::{
    slipstream_tick_info_storage_keys_with_base, v3_tick_bitmap_storage_key_with_base,
    v3_word_position,
};
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

fn liquidity_calldata(
    pool: Address,
    is_mint: bool,
    tick_lower: i32,
    tick_upper: i32,
    amount: u128,
) -> Bytes {
    let mut data = Vec::with_capacity(4 + 32 * 4);
    data.extend_from_slice(
        &keccak256(if is_mint {
            "executeMint(address,int24,int24,uint128)"
        } else {
            "executeBurn(address,int24,int24,uint128)"
        })[..4],
    );
    data.extend_from_slice(&address_word(pool).to_be_bytes::<32>());
    data.extend_from_slice(&signed_word(i128::from(tick_lower)).to_be_bytes::<32>());
    data.extend_from_slice(&signed_word(i128::from(tick_upper)).to_be_bytes::<32>());
    data.extend_from_slice(&U256::from(amount).to_be_bytes::<32>());
    Bytes::from(data)
}

fn signed_i24(raw: U256) -> i32 {
    let raw = (raw & U256::from(0x00ff_ffff_u32)).to::<u32>();
    if raw & 0x0080_0000 == 0 {
        raw as i32
    } else {
        (raw | 0xff00_0000) as i32
    }
}

fn ensure_slot(slots: &mut Vec<(U256, U256)>, slot: U256) {
    if slots.iter().all(|(candidate, _)| *candidate != slot) {
        slots.push((slot, U256::ZERO));
    }
}

fn set_slot(slots: &mut Vec<(U256, U256)>, slot: U256, value: U256) {
    ensure_slot(slots, slot);
    slots
        .iter_mut()
        .find(|(candidate, _)| *candidate == slot)
        .expect("ensured slot")
        .1 = value;
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
        V3SwapTransitionCapability::Exact,
    );
    let decoded = ConcentratedLiquidityAdapter::default().decode_event_with_context(
        &registration,
        &log,
        &parent,
        &context,
    );
    assert_eq!(decoded.error, None, "{} must replay exactly", case.name);
    let event = decoded.event.expect("reference event");
    assert_eq!(event.quality, UpdateQuality::Exact);
    let mut derived = parent.0.clone();
    for update in event.updates {
        match update {
            StateUpdate::Slot {
                address,
                slot,
                value,
            } => {
                assert_eq!(address, spec.pool);
                derived.insert((address, slot), value);
            }
            other => return Err(anyhow!("unexpected exact update: {other:?}")),
        }
    }
    assert_eq!(
        ConcentratedLiquidityAdapter::swap_transition_capability_with_context(
            &registration,
            &base_context,
        ),
        V3SwapTransitionCapability::Exact,
        "reviewed runtime must expose quote-exact replay without accounting evidence",
    );
    let quote_decoded = ConcentratedLiquidityAdapter::default().decode_event_with_context(
        &registration,
        &log,
        &parent,
        &base_context,
    );
    assert_eq!(
        quote_decoded.error, None,
        "{} quote-exact replay",
        case.name,
    );
    let quote_event = quote_decoded.event.expect("quote-exact event");
    assert_eq!(quote_event.quality, UpdateQuality::Exact);
    let mut quote_derived = parent.0.clone();
    for update in quote_event.updates {
        match update {
            StateUpdate::Slot {
                address,
                slot,
                value,
            } => {
                assert_eq!(address, spec.pool);
                quote_derived.insert((address, slot), value);
            }
            other => return Err(anyhow!("unexpected quote-exact update: {other:?}")),
        }
    }
    assert!(
        access
            .slots
            .iter()
            .any(|(address, _)| *address == spec.pool),
        "reference execution must independently enumerate pool storage",
    );
    for (address, slot) in access
        .slots
        .iter()
        .filter(|(address, _)| *address == spec.pool)
    {
        let expected = reference
            .cached_storage_value(*address, *slot)
            .ok_or_else(|| anyhow!("reference omitted accessed pool slot {slot}"))?;
        let actual = derived
            .get(&(*address, *slot))
            .copied()
            .ok_or_else(|| anyhow!("transition omitted accessed pool slot {slot}"))?;
        assert_eq!(
            actual, expected,
            "{} {:?} diverged at pool slot {slot}",
            case.name, spec.family,
        );
    }

    let mut offline = cache(spec.chain_id).await;
    offline.set_block(BlockId::from((BLOCK_HASH, Some(true))));
    offline.set_timestamp(Some(1_800_000_000 + sequence));
    offline.set_block_context(Some(1_000 + sequence), Some(0));
    offline
        .db_mut()
        .cache
        .block_hashes
        .insert(U256::from(999 + sequence), PARENT_HASH);
    offline
        .db_mut()
        .insert_account_info(CALLER, AccountInfo::default());
    offline
        .db_mut()
        .insert_account_info(Address::ZERO, AccountInfo::default());
    let derived_pool_slots = quote_derived
        .iter()
        .filter_map(|((address, slot), value)| (*address == spec.pool).then_some((*slot, *value)))
        .collect::<Vec<_>>();
    install(
        &mut offline,
        spec.pool,
        runtime(spec.proxy_runtime),
        &derived_pool_slots,
    );
    install(
        &mut offline,
        spec.implementation,
        runtime(spec.implementation_runtime),
        &[],
    );
    install(
        &mut offline,
        spec.factory,
        runtime(include_str!(
            "fixtures/slipstream_reference_factory_runtime.hex"
        )),
        &[(
            U256::ZERO,
            U256::from(case.fee) | (U256::from(100_000) << 24_usize),
        )],
    );
    let token_balance_slot = mapping_slot(spec.pool, U256::ZERO);
    for token in [token0, token1] {
        let balance = reference
            .cached_storage_value(token, token_balance_slot)
            .context("reference post-Swap pool token balance")?;
        install(
            &mut offline,
            token,
            runtime(include_str!("fixtures/reference_swap_token_runtime.hex")),
            &[(token_balance_slot, balance)],
        );
    }
    install(
        &mut offline,
        HARNESS,
        runtime(include_str!(
            "fixtures/v3_reference_swap_harness_runtime.hex"
        )),
        &[
            (U256::ZERO, address_word(token0)),
            (U256::from(1), address_word(token1)),
        ],
    );
    for follow_up_direction in [true, false] {
        let follow_up_limit = if follow_up_direction {
            MIN_SQRT_LIMIT
        } else {
            MAX_SQRT_LIMIT
        };
        let calldata = execute_calldata(spec.pool, follow_up_direction, 1, follow_up_limit);
        let expected = match reference.call_raw(CALLER, HARNESS, calldata.clone(), false)? {
            ExecutionResult::Success { output, .. } => output.into_data(),
            other => return Err(anyhow!("reference follow-up quote failed: {other:?}")),
        };
        let actual = match offline.call_raw(CALLER, HARNESS, calldata, false)? {
            ExecutionResult::Success { output, .. } => output.into_data(),
            other => {
                return Err(anyhow!(
                    "provider-disconnected follow-up quote failed: {other:?}"
                ));
            }
        };
        assert_eq!(
            actual, expected,
            "{} {:?} provider-disconnected follow-up direction {follow_up_direction}",
            case.name, spec.family,
        );
    }
    Ok(())
}

async fn run_liquidity_round_trip(spec: FamilySpec, sequence: u64) -> Result<()> {
    let fixture: Value = serde_json::from_str(spec.fixture)?;
    let spacing = fixture["pool"]["tick_spacing"].as_i64().unwrap() as i32;
    let layout = V3StorageLayout::slipstream(spacing);
    let mut slots: Vec<(U256, U256)> = fixture["parent_storage"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(slot, value)| (word(slot), word(value.as_str().unwrap())))
        .collect();
    let slot0 = slots
        .iter()
        .find(|(slot, _)| *slot == layout.slot0_slot)
        .unwrap()
        .1;
    let current_tick = signed_i24(slot0 >> 160_usize);
    let tick_lower = current_tick.div_euclid(spacing) * spacing;
    let tick_upper = tick_lower + spacing;
    let lower_keys =
        slipstream_tick_info_storage_keys_with_base(tick_lower, layout.ticks_base_slot);
    let upper_keys =
        slipstream_tick_info_storage_keys_with_base(tick_upper, layout.ticks_base_slot);
    let lower_bitmap = v3_tick_bitmap_storage_key_with_base(
        v3_word_position(tick_lower, spacing),
        layout.tick_bitmap_base_slot,
    );
    let upper_bitmap = v3_tick_bitmap_storage_key_with_base(
        v3_word_position(tick_upper, spacing),
        layout.tick_bitmap_base_slot,
    );
    for slot in lower_keys
        .iter()
        .chain(upper_keys.iter())
        .chain([&lower_bitmap, &upper_bitmap])
    {
        ensure_slot(&mut slots, *slot);
    }
    // Build a coherent, uninitialized in-range pair around the fixture's
    // current tick. The historical fixtures intentionally contain only the
    // ticks needed by their swap trace, so a set bitmap bit may otherwise lack
    // its live Tick.Info words in this generated liquidity corpus.
    for slot in lower_keys.iter().chain(upper_keys.iter()) {
        set_slot(&mut slots, *slot, U256::ZERO);
    }
    for (tick, bitmap_slot) in [(tick_lower, lower_bitmap), (tick_upper, upper_bitmap)] {
        let bit = tick.div_euclid(spacing).rem_euclid(256) as usize;
        let current = slots
            .iter()
            .find(|(candidate, _)| *candidate == bitmap_slot)
            .expect("ensured bitmap")
            .1;
        set_slot(&mut slots, bitmap_slot, current & !(U256::from(1) << bit));
    }
    let parent = ParentState(
        slots
            .iter()
            .map(|(slot, value)| ((spec.pool, *slot), *value))
            .collect(),
    );

    let timestamp = 1_810_000_000 + sequence;
    let mut reference = cache(spec.chain_id).await;
    reference.set_block(BlockId::from((BLOCK_HASH, Some(true))));
    reference.set_timestamp(Some(timestamp));
    reference.set_block_context(Some(2_000 + sequence), Some(0));
    reference
        .db_mut()
        .cache
        .block_hashes
        .insert(U256::from(1_999 + sequence), PARENT_HASH);
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
        &[(U256::ZERO, U256::from(10_000_u64))],
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
    install(&mut reference, token0, token_code.clone(), &[]);
    install(&mut reference, token1, token_code, &[]);
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

    let registration = PoolRegistration::new(PoolKey::Slipstream(spec.pool))
        .with_state_address(spec.pool)
        .with_metadata(ProtocolMetadata::Slipstream(
            V3Metadata::default()
                .with_tick_spacing(spacing)
                .with_storage_layout(layout),
        ));
    let amount = 7_u128;
    let mint_calldata = liquidity_calldata(spec.pool, true, tick_lower, tick_upper, amount);
    let (_, mint_access) =
        reference.call_raw_with_access_list(CALLER, HARNESS, mint_calldata.clone())?;
    let mint_result = reference.call_raw(CALLER, HARNESS, mint_calldata, true)?;
    let mint_logs = match mint_result {
        ExecutionResult::Success { logs, .. } => logs,
        other => return Err(anyhow!("reference Mint failed: {other:?}")),
    };
    let mint_topic = keccak256("Mint(address,address,int24,int24,uint128,uint256,uint256)");
    let mint_log: Log = mint_logs
        .into_iter()
        .find(|log| log.address == spec.pool && log.topics().first() == Some(&mint_topic))
        .ok_or_else(|| anyhow!("reference execution emitted no Mint"))?;
    let mint_context = AdapterEventContext::for_block(2_000 + sequence, BLOCK_HASH, timestamp)
        .with_chain_id(spec.chain_id)
        .with_parent_hash(PARENT_HASH)
        .with_transaction_hash(B256::from(
            U256::from(20_000 + sequence).to_be_bytes::<32>(),
        ))
        .with_event_order(1, 2);
    let mint_decoded = ConcentratedLiquidityAdapter::default().decode_event_with_context(
        &registration,
        &mint_log,
        &parent,
        &mint_context,
    );
    assert_eq!(mint_decoded.error, None, "{:?} Mint", spec.family);
    let mint_event = mint_decoded.event.expect("reference Mint event");
    assert_eq!(mint_event.quality, UpdateQuality::Exact);
    let mut mint_derived = parent.0.clone();
    let mut mint_update_slots = Vec::new();
    for update in mint_event.updates {
        match update {
            StateUpdate::Slot {
                address,
                slot,
                value,
            } => {
                assert_eq!(address, spec.pool);
                mint_update_slots.push(slot);
                mint_derived.insert((address, slot), value);
            }
            other => return Err(anyhow!("unexpected exact Mint update: {other:?}")),
        }
    }
    assert!(
        mint_access
            .slots
            .iter()
            .any(|(address, _)| *address == spec.pool),
        "deployed Mint must independently access pool storage",
    );
    for slot in mint_update_slots.iter().chain(
        lower_keys
            .iter()
            .chain(upper_keys.iter())
            .chain([&lower_bitmap, &upper_bitmap]),
    ) {
        let expected = reference
            .cached_storage_value(spec.pool, *slot)
            .ok_or_else(|| anyhow!("reference Mint omitted search slot {slot}"))?;
        let actual = mint_derived
            .get(&(spec.pool, *slot))
            .copied()
            .ok_or_else(|| anyhow!("derived Mint omitted search slot {slot}"))?;
        assert_eq!(
            actual, expected,
            "{:?} Mint diverged at search slot {slot}",
            spec.family,
        );
    }

    // Materialize only the event-derived pool state plus the prewarmed local
    // execution dependencies into a fresh cache backed by a panic-on-access
    // mock provider. Both swap directions must execute and match the deployed
    // reference state without any reconstruction or lazy provider read.
    let mut offline = cache(spec.chain_id).await;
    offline.set_block(BlockId::from((BLOCK_HASH, Some(true))));
    offline.set_timestamp(Some(timestamp));
    offline.set_block_context(Some(2_000 + sequence), Some(0));
    offline
        .db_mut()
        .cache
        .block_hashes
        .insert(U256::from(1_999 + sequence), PARENT_HASH);
    offline
        .db_mut()
        .insert_account_info(CALLER, AccountInfo::default());
    offline
        .db_mut()
        .insert_account_info(Address::ZERO, AccountInfo::default());
    let derived_pool_slots = mint_derived
        .iter()
        .filter_map(|((address, slot), value)| (*address == spec.pool).then_some((*slot, *value)))
        .collect::<Vec<_>>();
    install(
        &mut offline,
        spec.pool,
        runtime(spec.proxy_runtime),
        &derived_pool_slots,
    );
    install(
        &mut offline,
        spec.implementation,
        runtime(spec.implementation_runtime),
        &[],
    );
    install(
        &mut offline,
        spec.factory,
        runtime(include_str!(
            "fixtures/slipstream_reference_factory_runtime.hex"
        )),
        &[(U256::ZERO, U256::from(10_000_u64))],
    );
    let token_balance_slot = mapping_slot(spec.pool, U256::ZERO);
    let token0_balance = reference
        .cached_storage_value(token0, token_balance_slot)
        .context("reference Mint token0 pool balance")?;
    let token1_balance = reference
        .cached_storage_value(token1, token_balance_slot)
        .context("reference Mint token1 pool balance")?;
    install(
        &mut offline,
        token0,
        runtime(include_str!("fixtures/reference_swap_token_runtime.hex")),
        &[(token_balance_slot, token0_balance)],
    );
    install(
        &mut offline,
        token1,
        runtime(include_str!("fixtures/reference_swap_token_runtime.hex")),
        &[(token_balance_slot, token1_balance)],
    );
    install(
        &mut offline,
        HARNESS,
        runtime(include_str!(
            "fixtures/v3_reference_swap_harness_runtime.hex"
        )),
        &[
            (U256::ZERO, address_word(token0)),
            (U256::from(1), address_word(token1)),
        ],
    );
    for zero_for_one in [true, false] {
        let limit = if zero_for_one {
            MIN_SQRT_LIMIT
        } else {
            MAX_SQRT_LIMIT
        };
        let calldata = execute_calldata(spec.pool, zero_for_one, 1, limit);
        let expected = match reference.call_raw(CALLER, HARNESS, calldata.clone(), false)? {
            ExecutionResult::Success { output, .. } => output.into_data(),
            other => return Err(anyhow!("reference post-Mint quote failed: {other:?}")),
        };
        let actual = match offline.call_raw(CALLER, HARNESS, calldata, false)? {
            ExecutionResult::Success { output, .. } => output.into_data(),
            other => {
                return Err(anyhow!(
                    "provider-disconnected post-Mint quote failed: {other:?}"
                ));
            }
        };
        assert_eq!(
            actual, expected,
            "{:?} provider-disconnected post-Mint quote direction {zero_for_one}",
            spec.family,
        );
    }

    let burn_parent = ParentState(mint_derived);
    let burn_calldata = liquidity_calldata(spec.pool, false, tick_lower, tick_upper, amount);
    let (_, burn_access) =
        reference.call_raw_with_access_list(CALLER, HARNESS, burn_calldata.clone())?;
    let burn_result = reference.call_raw(CALLER, HARNESS, burn_calldata, true)?;
    let burn_logs = match burn_result {
        ExecutionResult::Success { logs, .. } => logs,
        other => return Err(anyhow!("reference Burn failed: {other:?}")),
    };
    let burn_topic = keccak256("Burn(address,int24,int24,uint128,uint256,uint256)");
    let burn_log: Log = burn_logs
        .into_iter()
        .find(|log| log.address == spec.pool && log.topics().first() == Some(&burn_topic))
        .ok_or_else(|| anyhow!("reference execution emitted no Burn"))?;
    let burn_context = AdapterEventContext::for_block(2_000 + sequence, BLOCK_HASH, timestamp)
        .with_chain_id(spec.chain_id)
        .with_parent_hash(PARENT_HASH)
        .with_transaction_hash(B256::from(
            U256::from(30_000 + sequence).to_be_bytes::<32>(),
        ))
        .with_event_order(2, 3);
    let burn_decoded = ConcentratedLiquidityAdapter::default().decode_event_with_context(
        &registration,
        &burn_log,
        &burn_parent,
        &burn_context,
    );
    assert_eq!(burn_decoded.error, None, "{:?} Burn", spec.family);
    let burn_event = burn_decoded.event.expect("reference Burn event");
    assert_eq!(burn_event.quality, UpdateQuality::Exact);
    let mut burn_derived = burn_parent.0.clone();
    let mut burn_update_slots = Vec::new();
    for update in burn_event.updates {
        match update {
            StateUpdate::Slot {
                address,
                slot,
                value,
            } => {
                assert_eq!(address, spec.pool);
                burn_update_slots.push(slot);
                burn_derived.insert((address, slot), value);
            }
            other => return Err(anyhow!("unexpected exact Burn update: {other:?}")),
        }
    }
    assert!(
        burn_access
            .slots
            .iter()
            .any(|(address, _)| *address == spec.pool),
        "deployed Burn must independently access pool storage",
    );
    for slot in burn_update_slots.iter().chain(
        lower_keys
            .iter()
            .chain(upper_keys.iter())
            .chain([&lower_bitmap, &upper_bitmap]),
    ) {
        let expected = reference
            .cached_storage_value(spec.pool, *slot)
            .ok_or_else(|| anyhow!("reference Burn omitted search slot {slot}"))?;
        let actual = burn_derived
            .get(&(spec.pool, *slot))
            .copied()
            .ok_or_else(|| anyhow!("derived Burn omitted search slot {slot}"))?;
        assert_eq!(
            actual, expected,
            "{:?} Burn diverged at search slot {slot}",
            spec.family,
        );
    }
    for slot in &burn_update_slots {
        offline
            .db_mut()
            .insert_account_storage(
                spec.pool,
                *slot,
                burn_derived
                    .get(&(spec.pool, *slot))
                    .copied()
                    .expect("Burn update value"),
            )
            .context("apply event-derived Burn slot to disconnected cache")?;
    }
    for zero_for_one in [true, false] {
        let limit = if zero_for_one {
            MIN_SQRT_LIMIT
        } else {
            MAX_SQRT_LIMIT
        };
        let calldata = execute_calldata(spec.pool, zero_for_one, 1, limit);
        let expected = match reference.call_raw(CALLER, HARNESS, calldata.clone(), false)? {
            ExecutionResult::Success { output, .. } => output.into_data(),
            other => return Err(anyhow!("reference post-Burn quote failed: {other:?}")),
        };
        let actual = match offline.call_raw(CALLER, HARNESS, calldata, false)? {
            ExecutionResult::Success { output, .. } => output.into_data(),
            other => {
                return Err(anyhow!(
                    "provider-disconnected post-Burn quote failed: {other:?}"
                ));
            }
        };
        assert_eq!(
            actual, expected,
            "{:?} provider-disconnected post-Burn quote direction {zero_for_one}",
            spec.family,
        );
    }
    Ok(())
}

#[tokio::test]
async fn deployed_base_and_optimism_runtimes_match_exact_transition_matrix() -> Result<()> {
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

#[tokio::test]
async fn deployed_base_and_optimism_runtimes_match_liquidity_round_trip() -> Result<()> {
    for (index, spec) in specs().into_iter().enumerate() {
        run_liquidity_round_trip(spec, index as u64 + 1).await?;
    }
    Ok(())
}
