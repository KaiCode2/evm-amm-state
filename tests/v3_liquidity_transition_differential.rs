//! Provider-free differential corpus for canonical Uniswap V3 liquidity events.
//!
//! Each scenario drives the embedded deployed Uniswap V3 pool runtime in revm
//! through an ordered sequence of `Mint`, `Burn`, and `Swap` calls, feeds every
//! bytecode-emitted log plus the exact same parent storage to the public
//! context-aware adapter path, and compares the complete touched pool surface
//! after each step. No RPC or formula-only reference implementation contributes
//! to the expected poststate.
//!
//! Sequences rather than isolated cases are the point. A `Mint` that only
//! writes `liquidityGross`/`liquidityNet` produces correct *quotes* forever --
//! `SwapMath.computeSwapStep` never reads a tick's outside accumulators -- and
//! diverges the moment a swap actually crosses that tick and commits
//! `Tick.cross`. Only replaying mint, swap, and burn against real bytecode in
//! order exposes that class of error.

use std::{
    collections::{BTreeMap, BTreeSet},
    str::FromStr,
    sync::Arc,
};

use alloy_primitives::{Address, B256, Bytes, Log, U256, address, b256, hex, keccak256};
use alloy_provider::{RootProvider, network::AnyNetwork};
use alloy_rpc_client::RpcClient;
use alloy_transport::mock::Asserter;
use anyhow::{Result, anyhow};
use evm_amm_state::adapters::storage::{
    V3StorageLayout, v3_tick_bitmap_storage_key_with_base, v3_tick_info_storage_keys_with_base,
};
use evm_amm_state::adapters::{
    AdapterEventContext, AdapterEventKind, AmmAdapter, ConcentratedLiquidityAdapter, PoolKey,
    PoolRegistration, ProtocolMetadata, RepairAction, StateUpdate, StateView, UpdateQuality,
    V3ImmutablePatchValues, V3LiquidityTransitionCapability, V3Metadata, uniswap_v3_code_seed,
    uniswap_v3_max_liquidity_per_tick,
};
use evm_fork_cache::cache::EvmCache;
use revm::{
    context::result::ExecutionResult,
    state::{AccountInfo, Bytecode},
};

const CALLER: Address = address!("00000000000000000000000000000000000000c1");
const POOL: Address = address!("00000000000000000000000000000000000000c2");
const FACTORY: Address = address!("00000000000000000000000000000000000000c3");
const TOKEN0: Address = address!("00000000000000000000000000000000000000c4");
const TOKEN1: Address = address!("00000000000000000000000000000000000000c5");
const HARNESS: Address = address!("00000000000000000000000000000000000000c6");
const BLOCK_HASH: B256 = b256!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
const PARENT_HASH: B256 = b256!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
const Q96: U256 = U256::from_limbs([0, 1 << 32, 0, 0]);
const MIN_SQRT_LIMIT: U256 = U256::from_limbs([4_295_128_740, 0, 0, 0]);
const MAX_SQRT_LIMIT: &str = "1461446703485210103287273052203988822378723970341";
const POOL_TOKEN_BALANCE: U256 = U256::from_limbs([0, 0, 1 << 8, 0]);

#[derive(Clone, Default)]
struct ParentState(BTreeMap<(Address, U256), U256>);

impl ParentState {
    /// Apply an update batch, returning every slot the batch dropped.
    ///
    /// A purge is not a value: it marks a cell unknown, so the parent forgets it
    /// rather than keeping a stale one.
    fn apply(&mut self, updates: &[StateUpdate]) -> BTreeSet<U256> {
        let mut purged = BTreeSet::new();
        for update in updates {
            match update {
                StateUpdate::Slot {
                    address,
                    slot,
                    value,
                } => {
                    self.0.insert((*address, *slot), *value);
                }
                StateUpdate::Purge {
                    address,
                    scope: evm_amm_state::adapters::PurgeScope::Slots(slots),
                } => {
                    for slot in slots {
                        self.0.remove(&(*address, *slot));
                        if *address == POOL {
                            purged.insert(*slot);
                        }
                    }
                }
                other => panic!("exact transition emitted non-atomic update {other:?}"),
            }
        }
        purged
    }

    fn pool_slots(&self) -> Vec<(U256, U256)> {
        self.0
            .iter()
            .filter(|((address, _), _)| *address == POOL)
            .map(|((_, slot), value)| (*slot, *value))
            .collect()
    }
}

impl StateView for ParentState {
    fn storage(&self, address: Address, slot: U256) -> Option<U256> {
        self.0.get(&(address, slot)).copied()
    }
}

#[derive(Clone, Copy, Debug)]
struct TickSpec {
    tick: i32,
    liquidity_gross: u128,
    liquidity_net: i128,
}

#[derive(Clone, Copy, Debug)]
enum Action {
    Mint {
        tick_lower: i32,
        tick_upper: i32,
        amount: u128,
    },
    Burn {
        tick_lower: i32,
        tick_upper: i32,
        amount: u128,
    },
    Swap {
        zero_for_one: bool,
        amount: i128,
    },
}

#[derive(Clone, Copy, Debug)]
struct OracleCase {
    index: u16,
    cardinality: u16,
    cardinality_next: u16,
    parent_timestamp: u32,
}

const DEFAULT_ORACLE: OracleCase = OracleCase {
    index: 0,
    cardinality: 1,
    cardinality_next: 1,
    parent_timestamp: 100,
};

#[derive(Clone)]
struct Scenario {
    name: &'static str,
    fee: u32,
    tick_spacing: i32,
    liquidity: U256,
    fee_protocol: u8,
    seeded_ticks: Vec<TickSpec>,
    /// Bitmap words present in the parent. Cold start reconstructs every word in
    /// its radius, including empty ones, so an unset bit is proven -- not
    /// unknown -- and the transition may skip reading that tick entirely.
    bitmap_words: Vec<i16>,
    oracle: OracleCase,
    /// One block timestamp per action; repeating a value exercises the
    /// same-timestamp oracle path.
    timestamps: Vec<u64>,
    actions: Vec<Action>,
    /// Parent slots to override after the coherent base parent is built.
    overrides: Vec<(U256, U256)>,
}

async fn cache() -> EvmCache {
    let asserter = Asserter::new();
    let client = RpcClient::mocked(asserter);
    let provider = RootProvider::<AnyNetwork>::new(client);
    EvmCache::new(Arc::new(provider)).await
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
        .expect("mark reference account storage local");
    for (slot, value) in slots {
        cache
            .db_mut()
            .insert_account_storage(address, *slot, *value)
            .expect("seed reference account slot");
    }
}

fn runtime(hex_runtime: &str) -> Bytes {
    Bytes::from(hex::decode(hex_runtime.trim()).expect("checked-in runtime hex"))
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

/// The four storage words of `positions[keccak256(owner, tickLower, tickUpper)]`.
///
/// Position accounting is deliberately outside the adapter's declared surface:
/// `swap` never reads `positions`, so reconstructing it would buy nothing for
/// quoting or simulation and would require parent cells cold start never warms.
/// The differential excludes exactly these words -- named, not by tolerating
/// mismatches -- so every other cell stays under the completeness oracle.
fn position_slots(owner: Address, tick_lower: i32, tick_upper: i32) -> [U256; 4] {
    let mut packed = Vec::with_capacity(26);
    packed.extend_from_slice(owner.as_slice());
    packed.extend_from_slice(&tick_lower.to_be_bytes()[1..]);
    packed.extend_from_slice(&tick_upper.to_be_bytes()[1..]);
    let key = keccak256(packed);
    let mut encoded = [0_u8; 64];
    encoded[..32].copy_from_slice(key.as_slice());
    encoded[63] = 7;
    let base = U256::from_be_slice(keccak256(encoded).as_slice());
    [
        base,
        base + U256::from(1),
        base + U256::from(2),
        base + U256::from(3),
    ]
}

fn signed_word(value: i128) -> U256 {
    if value >= 0 {
        U256::from(value as u128)
    } else {
        (!U256::from(value.unsigned_abs())).wrapping_add(U256::from(1))
    }
}

fn pool_code(scenario: &Scenario) -> Result<Bytes> {
    let mut immutables = V3ImmutablePatchValues::default()
        .with_pool_address(POOL)
        .with_factory(FACTORY)
        .with_token0(TOKEN0)
        .with_token1(TOKEN1)
        .with_fee(scenario.fee)
        .with_tick_spacing(scenario.tick_spacing);
    immutables.max_liquidity_per_tick = uniswap_v3_max_liquidity_per_tick(scenario.tick_spacing);
    Ok(uniswap_v3_code_seed(POOL, &immutables)?.runtime_bytecode)
}

/// Seed the pool account, both tokens, and the calling harness into `cache`.
fn install_world(cache: &mut EvmCache, scenario: &Scenario, slots: &[(U256, U256)]) -> Result<()> {
    cache
        .db_mut()
        .insert_account_info(CALLER, AccountInfo::default());
    cache
        .db_mut()
        .insert_account_info(Address::ZERO, AccountInfo::default());
    install(cache, POOL, pool_code(scenario)?, slots);
    let token_code = runtime(include_str!("fixtures/reference_swap_token_runtime.hex"));
    let token_slots = [(mapping_slot(POOL, U256::ZERO), POOL_TOKEN_BALANCE)];
    install(cache, TOKEN0, token_code.clone(), &token_slots);
    install(cache, TOKEN1, token_code, &token_slots);
    install(
        cache,
        HARNESS,
        runtime(include_str!(
            "fixtures/v3_reference_swap_harness_runtime.hex"
        )),
        &[
            (U256::ZERO, address_word(TOKEN0)),
            (U256::from(1), address_word(TOKEN1)),
        ],
    );
    Ok(())
}

fn slot0_word(scenario: &Scenario) -> U256 {
    Q96 | (U256::from(scenario.oracle.index) << 184_usize)
        | (U256::from(scenario.oracle.cardinality) << 200_usize)
        | (U256::from(scenario.oracle.cardinality_next) << 216_usize)
        | (U256::from(scenario.fee_protocol) << 232_usize)
        | (U256::from(1) << 240_usize)
}

fn observation(timestamp: u32) -> U256 {
    U256::from(timestamp) | (U256::from(1) << 248_usize)
}

fn initialized_tick(tick: TickSpec) -> [U256; 4] {
    [
        U256::from(tick.liquidity_gross) | (signed_word(tick.liquidity_net) << 128_usize),
        U256::from(17),
        U256::from(19),
        U256::from(1) << 248_usize,
    ]
}

/// Build a coherent canonical parent: slot0, both fee accumulators, protocol
/// fees, active liquidity, the observation ring, every declared bitmap word, and
/// the `Tick.Info` words of each seeded initialized tick.
fn parent_slots(scenario: &Scenario, layout: V3StorageLayout) -> (Vec<(U256, U256)>, Vec<U256>) {
    let mut slots = vec![
        (layout.slot0_slot, slot0_word(scenario)),
        (U256::from(1), U256::from(101)),
        (U256::from(2), U256::from(202)),
        (U256::from(3), U256::from(303)),
        (layout.liquidity_slot, scenario.liquidity),
    ];
    let mut declared: Vec<U256> = slots.iter().map(|(slot, _)| *slot).collect();

    for word in &scenario.bitmap_words {
        let slot = v3_tick_bitmap_storage_key_with_base(*word, layout.tick_bitmap_base_slot);
        slots.push((slot, U256::ZERO));
        declared.push(slot);
    }

    for index in 0..scenario.oracle.cardinality_next {
        let value = if index < scenario.oracle.cardinality {
            let timestamp = if index == scenario.oracle.index {
                scenario.oracle.parent_timestamp
            } else {
                scenario
                    .oracle
                    .parent_timestamp
                    .saturating_sub(u32::from(scenario.oracle.cardinality - index) * 10)
            };
            observation(timestamp)
        } else {
            U256::ZERO
        };
        let slot = U256::from(8) + U256::from(index);
        slots.push((slot, value));
        declared.push(slot);
    }

    for tick in &scenario.seeded_ticks {
        let compressed = tick.tick.div_euclid(scenario.tick_spacing);
        let word_position = compressed.div_euclid(256) as i16;
        let bit = compressed.rem_euclid(256) as usize;
        let word_slot =
            v3_tick_bitmap_storage_key_with_base(word_position, layout.tick_bitmap_base_slot);
        let bitmap = slots
            .iter_mut()
            .find(|(slot, _)| *slot == word_slot)
            .expect("seeded tick needs its bitmap word declared");
        bitmap.1 |= U256::from(1) << bit;
        let keys = v3_tick_info_storage_keys_with_base(tick.tick, layout.ticks_base_slot);
        slots.extend(keys.into_iter().zip(initialized_tick(*tick)));
        declared.extend(keys);
    }
    for (slot, value) in &scenario.overrides {
        match slots.iter_mut().find(|(existing, _)| existing == slot) {
            Some(entry) => entry.1 = *value,
            None => {
                slots.push((*slot, *value));
                declared.push(*slot);
            }
        }
    }
    (slots, declared)
}

fn action_calldata(action: Action) -> Bytes {
    let mut data = Vec::with_capacity(4 + 32 * 4);
    match action {
        Action::Mint {
            tick_lower,
            tick_upper,
            amount,
        }
        | Action::Burn {
            tick_lower,
            tick_upper,
            amount,
        } => {
            let signature = if matches!(action, Action::Mint { .. }) {
                "executeMint(address,int24,int24,uint128)"
            } else {
                "executeBurn(address,int24,int24,uint128)"
            };
            data.extend_from_slice(&keccak256(signature)[..4]);
            data.extend_from_slice(&address_word(POOL).to_be_bytes::<32>());
            data.extend_from_slice(&signed_word(i128::from(tick_lower)).to_be_bytes::<32>());
            data.extend_from_slice(&signed_word(i128::from(tick_upper)).to_be_bytes::<32>());
            data.extend_from_slice(&U256::from(amount).to_be_bytes::<32>());
        }
        Action::Swap {
            zero_for_one,
            amount,
        } => {
            data.extend_from_slice(&keccak256("execute(address,bool,int256,uint160)")[..4]);
            data.extend_from_slice(&address_word(POOL).to_be_bytes::<32>());
            data.extend_from_slice(&U256::from(zero_for_one as u8).to_be_bytes::<32>());
            data.extend_from_slice(&signed_word(amount).to_be_bytes::<32>());
            let limit = if zero_for_one {
                MIN_SQRT_LIMIT
            } else {
                U256::from_str(MAX_SQRT_LIMIT).expect("max sqrt limit")
            };
            data.extend_from_slice(&limit.to_be_bytes::<32>());
        }
    }
    Bytes::from(data)
}

fn action_topic(action: Action) -> B256 {
    match action {
        Action::Mint { .. } => {
            keccak256("Mint(address,address,int24,int24,uint128,uint256,uint256)")
        }
        Action::Burn { .. } => keccak256("Burn(address,int24,int24,uint128,uint256,uint256)"),
        Action::Swap { .. } => {
            keccak256("Swap(address,address,int256,int256,uint160,uint128,int24)")
        }
    }
}

fn expected_kind(action: Action) -> AdapterEventKind {
    match action {
        Action::Mint { .. } => AdapterEventKind::LiquidityAdded,
        Action::Burn { .. } => AdapterEventKind::LiquidityRemoved,
        Action::Swap { .. } => AdapterEventKind::Swap,
    }
}

/// Every `Tick.Info` and bitmap slot an action can reach, so the comparison
/// covers cells the reference wrote but never read back.
fn action_slots(action: Action, scenario: &Scenario, layout: V3StorageLayout) -> Vec<U256> {
    let (tick_lower, tick_upper) = match action {
        Action::Mint {
            tick_lower,
            tick_upper,
            ..
        }
        | Action::Burn {
            tick_lower,
            tick_upper,
            ..
        } => (tick_lower, tick_upper),
        Action::Swap { .. } => return Vec::new(),
    };
    let mut slots = Vec::new();
    for tick in [tick_lower, tick_upper] {
        slots.extend(v3_tick_info_storage_keys_with_base(
            tick,
            layout.ticks_base_slot,
        ));
        let word = tick.div_euclid(scenario.tick_spacing).div_euclid(256) as i16;
        slots.push(v3_tick_bitmap_storage_key_with_base(
            word,
            layout.tick_bitmap_base_slot,
        ));
    }
    slots
}

/// Run the same swap against the reference and against a cache holding nothing
/// but the event-derived state, and require identical amounts.
///
/// This is the executable half of the contract: matching storage is necessary,
/// but the question a caller actually asks is whether a swap simulated on
/// event-derived state behaves like one on the real chain.
async fn assert_executable(
    scenario: &Scenario,
    reference: &mut EvmCache,
    derived: &ParentState,
    timestamp: u64,
    block: u64,
    label: &str,
) -> Result<()> {
    for (zero_for_one, amount) in [(true, 5_000_000_000_i128), (false, 5_000_000_000_i128)] {
        let calldata = action_calldata(Action::Swap {
            zero_for_one,
            amount,
        });
        let expected = reference.call_raw(CALLER, HARNESS, calldata.clone(), false)?;
        let mut replica = cache().await;
        replica.set_timestamp(Some(timestamp));
        replica.set_block_context(Some(block), Some(0));
        install_world(&mut replica, scenario, &derived.pool_slots())?;
        let actual = replica.call_raw(CALLER, HARNESS, calldata, false)?;
        match (expected, actual) {
            (
                ExecutionResult::Success {
                    output: expected, ..
                },
                ExecutionResult::Success { output: actual, .. },
            ) => {
                assert_eq!(
                    expected.data(),
                    actual.data(),
                    "{label}: swap simulated on event-derived state returned different amounts \
                     (zero_for_one = {zero_for_one})",
                );
            }
            (expected, actual) => {
                return Err(anyhow!(
                    "{label}: swap outcome differs on event-derived state \
                     (zero_for_one = {zero_for_one}): reference {expected:?}, derived {actual:?}"
                ));
            }
        }
    }
    Ok(())
}

async fn run_scenario(scenario: &Scenario) -> Result<()> {
    assert_eq!(
        scenario.timestamps.len(),
        scenario.actions.len(),
        "scenario {} needs one timestamp per action",
        scenario.name
    );
    let layout = V3StorageLayout::uniswap(scenario.tick_spacing);
    let (pool_slots, declared_slots) = parent_slots(scenario, layout);
    let mut derived = ParentState(
        pool_slots
            .iter()
            .map(|(slot, value)| ((POOL, *slot), *value))
            .collect(),
    );

    let mut reference = cache().await;
    install_world(&mut reference, scenario, &pool_slots)?;

    let registration = PoolRegistration::new(PoolKey::UniswapV3(POOL))
        .with_state_address(POOL)
        .with_metadata(ProtocolMetadata::UniswapV3(
            V3Metadata::default()
                .with_fee(scenario.fee)
                .with_tick_spacing(scenario.tick_spacing)
                .with_storage_layout(layout),
        ));
    assert_eq!(
        ConcentratedLiquidityAdapter::liquidity_transition_capability(&registration),
        V3LiquidityTransitionCapability::Exact,
        "scenario {} must register as exactly replayable",
        scenario.name
    );

    let mut tracked: BTreeSet<U256> = declared_slots.into_iter().collect();
    let mut applied_any_liquidity_event = false;

    for (step, action) in scenario.actions.iter().copied().enumerate() {
        let timestamp = scenario.timestamps[step];
        let block = 100 + step as u64;
        reference.set_timestamp(Some(timestamp));
        reference.set_block_context(Some(block), Some(0));
        tracked.extend(action_slots(action, scenario, layout));

        let before = derived.clone();
        // The position this event belongs to is the one surface the transition
        // does not reconstruct, so it is both excluded from the storage
        // comparison and required to be explicitly dropped.
        let excluded: BTreeSet<U256> = match action {
            Action::Mint {
                tick_lower,
                tick_upper,
                ..
            }
            | Action::Burn {
                tick_lower,
                tick_upper,
                ..
            } => position_slots(HARNESS, tick_lower, tick_upper)
                .into_iter()
                .collect(),
            Action::Swap { .. } => BTreeSet::new(),
        };
        let calldata = action_calldata(action);
        let (_, access) = reference.call_raw_with_access_list(CALLER, HARNESS, calldata.clone())?;
        let result = reference.call_raw(CALLER, HARNESS, calldata, true)?;
        let logs = match result {
            ExecutionResult::Success { logs, .. } => logs,
            other => {
                return Err(anyhow!(
                    "scenario {} step {step} ({action:?}) failed on deployed bytecode: {other:?}",
                    scenario.name
                ));
            }
        };
        let topic = action_topic(action);
        let reference_log: Log = logs
            .into_iter()
            .find(|log| log.address == POOL && log.topics().first() == Some(&topic))
            .ok_or_else(|| {
                anyhow!(
                    "scenario {} step {step} ({action:?}) emitted no matching log",
                    scenario.name
                )
            })?;

        let context = AdapterEventContext::for_block(block, BLOCK_HASH, timestamp)
            .with_chain_id(1)
            .with_parent_hash(PARENT_HASH)
            .with_transaction_hash(B256::repeat_byte(0xd0_u8.wrapping_add(step as u8)))
            .with_event_order(step as u64 + 1, step as u64 + 2);
        let decoded = ConcentratedLiquidityAdapter::default().decode_event_with_context(
            &registration,
            &reference_log,
            &derived,
            &context,
        );
        assert_eq!(
            decoded.error, None,
            "scenario {} step {step} ({action:?})",
            scenario.name
        );
        let event = decoded.event.expect("event must decode");
        assert_eq!(
            event.quality,
            UpdateQuality::Exact,
            "scenario {} step {step} ({action:?}) must be exact",
            scenario.name
        );
        assert_eq!(
            event.kind,
            expected_kind(action),
            "scenario {} step {step}",
            scenario.name
        );
        if matches!(action, Action::Mint { .. } | Action::Burn { .. }) {
            applied_any_liquidity_event = true;
        }
        let purged = derived.apply(&event.updates);
        // The only cells an exact liquidity transition may drop are the ones it
        // deliberately does not reconstruct. Anything else dropped here would be
        // a silent hole in the pricing surface.
        assert_eq!(
            purged, excluded,
            "scenario {} step {step} ({action:?}) purged an unexpected slot set",
            scenario.name
        );

        // The completeness oracle: every pool slot the reference touched, plus
        // every slot this scenario declares, must agree -- and every slot the
        // reference *changed* must appear in the emitted update batch, so a
        // correct-by-accident carry-over cannot pass.
        let touched: BTreeSet<U256> = tracked
            .iter()
            .copied()
            .chain(
                access
                    .slots
                    .into_iter()
                    .filter_map(|(address, slot)| (address == POOL).then_some(slot)),
            )
            .filter(|slot| !excluded.contains(slot))
            .collect();
        for slot in touched {
            let expected = reference
                .cached_storage_value(POOL, slot)
                .unwrap_or_default();
            assert_eq!(
                derived.storage(POOL, slot).unwrap_or_default(),
                expected,
                "scenario {} step {step} ({action:?}): deployed-bytecode mismatch at slot {slot}",
                scenario.name
            );
            if before.storage(POOL, slot).unwrap_or_default() != expected {
                assert!(
                    event.updates.iter().any(|update| matches!(
                        update,
                        StateUpdate::Slot { address, slot: updated, value }
                            if *address == POOL && *updated == slot && *value == expected
                    )),
                    "scenario {} step {step} ({action:?}) omitted reference-changed slot {slot}",
                    scenario.name
                );
            }
        }

        assert_executable(
            scenario,
            &mut reference,
            &derived,
            timestamp,
            block,
            &format!("scenario {} step {step} ({action:?})", scenario.name),
        )
        .await?;
    }

    assert!(
        applied_any_liquidity_event,
        "scenario {} exercised no liquidity event",
        scenario.name
    );
    Ok(())
}

fn base_scenario(name: &'static str) -> Scenario {
    Scenario {
        name,
        fee: 3_000,
        tick_spacing: 60,
        liquidity: U256::from(1_000_000_000_000_000_000_u128),
        fee_protocol: 0,
        seeded_ticks: vec![
            TickSpec {
                tick: -600,
                liquidity_gross: 200_000_000_000_000_000,
                liquidity_net: 200_000_000_000_000_000,
            },
            TickSpec {
                tick: 600,
                liquidity_gross: 200_000_000_000_000_000,
                liquidity_net: -200_000_000_000_000_000,
            },
        ],
        bitmap_words: vec![-1, 0],
        oracle: DEFAULT_ORACLE,
        timestamps: Vec::new(),
        actions: Vec::new(),
        overrides: Vec::new(),
    }
}

/// A mint straddling the current tick initializes `tickLower` with the two
/// global fee accumulators and the current oracle reading, and `tickUpper` with
/// nothing but its flag. Getting that asymmetry backwards is invisible to every
/// quote and corrupts the pool the first time a swap crosses either boundary.
#[tokio::test(flavor = "multi_thread")]
async fn in_range_mint_initializes_both_boundaries_and_survives_crossing() -> Result<()> {
    let mut scenario = base_scenario("in-range mint then crossings");
    scenario.timestamps = vec![110, 120, 130, 140];
    scenario.actions = vec![
        Action::Mint {
            tick_lower: -120,
            tick_upper: 120,
            amount: 50_000_000_000_000_000,
        },
        Action::Swap {
            zero_for_one: true,
            amount: 200_000_000_000_000,
        },
        Action::Swap {
            zero_for_one: false,
            amount: 400_000_000_000_000,
        },
        Action::Swap {
            zero_for_one: true,
            amount: 200_000_000_000_000,
        },
    ];
    run_scenario(&scenario).await
}

/// Fee growth must already be nonzero when a tick is initialized, otherwise the
/// "all growth happened below" convention is indistinguishable from writing
/// zero. Swapping first makes both accumulators nonzero before the mint.
#[tokio::test(flavor = "multi_thread")]
async fn mint_after_a_swap_seeds_outside_accumulators_from_live_fee_growth() -> Result<()> {
    let mut scenario = base_scenario("mint after fee growth");
    scenario.timestamps = vec![110, 120, 130, 140, 150];
    scenario.actions = vec![
        Action::Swap {
            zero_for_one: true,
            amount: 900_000_000_000_000,
        },
        Action::Swap {
            zero_for_one: false,
            amount: 1_500_000_000_000_000,
        },
        Action::Mint {
            tick_lower: -180,
            tick_upper: 180,
            amount: 70_000_000_000_000_000,
        },
        Action::Swap {
            zero_for_one: true,
            amount: 800_000_000_000_000,
        },
        Action::Swap {
            zero_for_one: false,
            amount: 800_000_000_000_000,
        },
    ];
    run_scenario(&scenario).await
}

/// Burning a tick's entire gross runs `Tick.clear`, which deletes all four
/// words. Zeroing only word 0 leaves stale accumulators behind that a later
/// re-initialization of the same tick would inherit.
#[tokio::test(flavor = "multi_thread")]
async fn burn_to_zero_clears_every_tick_word_and_allows_reinitialization() -> Result<()> {
    let mut scenario = base_scenario("burn to zero then reinitialize");
    scenario.timestamps = vec![110, 120, 130, 140, 150, 160];
    scenario.actions = vec![
        Action::Mint {
            tick_lower: -120,
            tick_upper: 120,
            amount: 50_000_000_000_000_000,
        },
        Action::Swap {
            zero_for_one: true,
            amount: 700_000_000_000_000,
        },
        Action::Burn {
            tick_lower: -120,
            tick_upper: 120,
            amount: 50_000_000_000_000_000,
        },
        Action::Mint {
            tick_lower: -120,
            tick_upper: 120,
            amount: 30_000_000_000_000_000,
        },
        Action::Swap {
            zero_for_one: false,
            amount: 900_000_000_000_000,
        },
        Action::Burn {
            tick_lower: -120,
            tick_upper: 120,
            amount: 30_000_000_000_000_000,
        },
    ];
    run_scenario(&scenario).await
}

/// Out-of-range positions must not touch active liquidity or the oracle, and a
/// mint onto an already-initialized tick must accumulate gross without
/// re-running the initialization convention.
#[tokio::test(flavor = "multi_thread")]
async fn out_of_range_mint_and_burn_leave_active_liquidity_untouched() -> Result<()> {
    let mut scenario = base_scenario("out-of-range positions");
    scenario.timestamps = vec![110, 120, 130, 140, 150];
    scenario.actions = vec![
        // Upper side, reusing the seeded tick 600 as its upper boundary.
        Action::Mint {
            tick_lower: 300,
            tick_upper: 600,
            amount: 40_000_000_000_000_000,
        },
        // Lower side, reusing the seeded tick -600.
        Action::Mint {
            tick_lower: -600,
            tick_upper: -300,
            amount: 40_000_000_000_000_000,
        },
        Action::Swap {
            zero_for_one: true,
            amount: 3_000_000_000_000_000,
        },
        Action::Burn {
            tick_lower: 300,
            tick_upper: 600,
            amount: 40_000_000_000_000_000,
        },
        Action::Burn {
            tick_lower: -600,
            tick_upper: -300,
            amount: 20_000_000_000_000_000,
        },
    ];
    run_scenario(&scenario).await
}

/// Both boundaries in one bitmap word must compose: `flipTick` XORs, so two
/// full-word writes computed from the same parent would clobber each other.
#[tokio::test(flavor = "multi_thread")]
async fn boundaries_sharing_a_bitmap_word_compose() -> Result<()> {
    let mut scenario = base_scenario("shared bitmap word");
    scenario.timestamps = vec![110, 120, 130];
    scenario.actions = vec![
        Action::Mint {
            tick_lower: 60,
            tick_upper: 120,
            amount: 25_000_000_000_000_000,
        },
        Action::Mint {
            tick_lower: 60,
            tick_upper: 180,
            amount: 25_000_000_000_000_000,
        },
        Action::Burn {
            tick_lower: 60,
            tick_upper: 120,
            amount: 25_000_000_000_000_000,
        },
    ];
    run_scenario(&scenario).await
}

/// A second action in the same block must reuse the current observation rather
/// than advancing the ring, and a full ring must wrap while growing to
/// `observationCardinalityNext`.
#[tokio::test(flavor = "multi_thread")]
async fn oracle_same_timestamp_growth_and_wrap() -> Result<()> {
    let mut scenario = base_scenario("oracle growth and wrap");
    scenario.oracle = OracleCase {
        index: 1,
        cardinality: 2,
        cardinality_next: 3,
        parent_timestamp: 100,
    };
    scenario.timestamps = vec![110, 110, 120, 130];
    scenario.actions = vec![
        Action::Mint {
            tick_lower: -120,
            tick_upper: 120,
            amount: 20_000_000_000_000_000,
        },
        // Same timestamp: `_modifyPosition` must not write a new observation.
        Action::Mint {
            tick_lower: -180,
            tick_upper: 180,
            amount: 20_000_000_000_000_000,
        },
        // Grows to cardinalityNext, then wraps back to index 0.
        Action::Mint {
            tick_lower: -240,
            tick_upper: 240,
            amount: 20_000_000_000_000_000,
        },
        Action::Mint {
            tick_lower: -300,
            tick_upper: 300,
            amount: 20_000_000_000_000_000,
        },
    ];
    run_scenario(&scenario).await
}

/// Tick spacing changes both the bitmap geometry and `maxLiquidityPerTick`,
/// which canonical Uniswap holds as a runtime immutable rather than in storage.
#[tokio::test(flavor = "multi_thread")]
async fn every_canonical_fee_tier_replays() -> Result<()> {
    for (fee, tick_spacing) in [(100_u32, 1_i32), (500, 10), (3_000, 60), (10_000, 200)] {
        let mut scenario = base_scenario("fee tier");
        scenario.fee = fee;
        scenario.tick_spacing = tick_spacing;
        scenario.seeded_ticks = vec![
            TickSpec {
                tick: -10 * tick_spacing,
                liquidity_gross: 200_000_000_000_000_000,
                liquidity_net: 200_000_000_000_000_000,
            },
            TickSpec {
                tick: 10 * tick_spacing,
                liquidity_gross: 200_000_000_000_000_000,
                liquidity_net: -200_000_000_000_000_000,
            },
        ];
        scenario.timestamps = vec![110, 120, 130];
        scenario.actions = vec![
            Action::Mint {
                tick_lower: -2 * tick_spacing,
                tick_upper: 2 * tick_spacing,
                amount: 50_000_000_000_000_000,
            },
            Action::Swap {
                zero_for_one: true,
                amount: 500_000_000_000_000,
            },
            Action::Burn {
                tick_lower: -2 * tick_spacing,
                tick_upper: 2 * tick_spacing,
                amount: 50_000_000_000_000_000,
            },
        ];
        run_scenario(&scenario).await?;
    }
    Ok(())
}

/// A nonzero `feeProtocol` splits swap fees, so the accumulators a later mint
/// copies into its new tick differ from the no-protocol-fee case.
#[tokio::test(flavor = "multi_thread")]
async fn protocol_fees_do_not_disturb_tick_initialization() -> Result<()> {
    let mut scenario = base_scenario("protocol fees");
    scenario.fee_protocol = 0x44;
    scenario.timestamps = vec![110, 120, 130];
    scenario.actions = vec![
        Action::Swap {
            zero_for_one: true,
            amount: 1_000_000_000_000_000,
        },
        Action::Mint {
            tick_lower: -120,
            tick_upper: 120,
            amount: 60_000_000_000_000_000,
        },
        Action::Swap {
            zero_for_one: false,
            amount: 1_000_000_000_000_000,
        },
    ];
    run_scenario(&scenario).await
}

/// A zero-amount `Burn` is a position fee poke: `_modifyPosition` guards every
/// tick, bitmap, oracle, and liquidity write on a nonzero delta, so the pool's
/// pricing surface must not move at all.
#[tokio::test(flavor = "multi_thread")]
async fn zero_amount_burn_is_an_exact_no_op() -> Result<()> {
    let mut scenario = base_scenario("zero-amount burn");
    scenario.timestamps = vec![110, 120];
    scenario.actions = vec![
        Action::Mint {
            tick_lower: -120,
            tick_upper: 120,
            amount: 40_000_000_000_000_000,
        },
        Action::Burn {
            tick_lower: -120,
            tick_upper: 120,
            amount: 0,
        },
    ];
    run_scenario(&scenario).await
}

/// A boundary tick outside the warmed bitmap radius used to purge the pool's
/// entire storage, forcing a full cold start and leaving it unquotable in the
/// meantime. It must now cost only the cells that boundary actually occupies,
/// while price, active liquidity, the oracle, and the resolvable boundary are
/// still established exactly.
#[tokio::test(flavor = "multi_thread")]
async fn a_cold_boundary_tick_costs_its_own_slots_not_the_pool() -> Result<()> {
    // Tick 20040 compresses to 334, which lives in bitmap word 1 — a word this
    // parent never warmed, so the boundary is unknown rather than empty.
    const COLD_TICK: i32 = 20_040;
    const WARM_TICK: i32 = -60;
    const AMOUNT: u128 = 45_000_000_000_000_000;

    let mut scenario = base_scenario("cold upper boundary");
    scenario.timestamps = vec![110];
    scenario.actions = vec![Action::Mint {
        tick_lower: WARM_TICK,
        tick_upper: COLD_TICK,
        amount: AMOUNT,
    }];
    let layout = V3StorageLayout::uniswap(scenario.tick_spacing);
    let (pool_slots, _) = parent_slots(&scenario, layout);
    let mut derived = ParentState(
        pool_slots
            .iter()
            .map(|(slot, value)| ((POOL, *slot), *value))
            .collect(),
    );
    let mut reference = cache().await;
    reference.set_timestamp(Some(110));
    reference.set_block_context(Some(100), Some(0));
    install_world(&mut reference, &scenario, &pool_slots)?;

    let cold_bitmap = v3_tick_bitmap_storage_key_with_base(1, layout.tick_bitmap_base_slot);
    let cold_tick_keys = v3_tick_info_storage_keys_with_base(COLD_TICK, layout.ticks_base_slot);
    assert!(
        derived.storage(POOL, cold_bitmap).is_none(),
        "the fixture must leave the cold boundary's bitmap word unknown"
    );

    let action = scenario.actions[0];
    let calldata = action_calldata(action);
    let result = reference.call_raw(CALLER, HARNESS, calldata, true)?;
    let logs = match result {
        ExecutionResult::Success { logs, .. } => logs,
        other => return Err(anyhow!("reference mint failed: {other:?}")),
    };
    let topic = action_topic(action);
    let reference_log: Log = logs
        .into_iter()
        .find(|log| log.address == POOL && log.topics().first() == Some(&topic))
        .ok_or_else(|| anyhow!("reference emitted no Mint log"))?;

    let registration = PoolRegistration::new(PoolKey::UniswapV3(POOL))
        .with_state_address(POOL)
        .with_metadata(ProtocolMetadata::UniswapV3(
            V3Metadata::default()
                .with_fee(scenario.fee)
                .with_tick_spacing(scenario.tick_spacing)
                .with_storage_layout(layout),
        ));
    let context = AdapterEventContext::for_block(100, BLOCK_HASH, 110)
        .with_chain_id(1)
        .with_parent_hash(PARENT_HASH)
        .with_transaction_hash(B256::repeat_byte(0xd0))
        .with_event_order(1, 2);
    let decoded = ConcentratedLiquidityAdapter::default().decode_event_with_context(
        &registration,
        &reference_log,
        &derived,
        &context,
    );
    assert_eq!(decoded.error, None);
    let event = decoded
        .event
        .expect("cold-boundary mint still yields an event");

    assert_eq!(
        event.quality,
        UpdateQuality::RequiresRepair,
        "an unresolvable boundary cannot be claimed exact"
    );
    let expected_repair: BTreeSet<(Address, U256)> = cold_tick_keys
        .into_iter()
        .chain([cold_bitmap])
        .map(|slot| (POOL, slot))
        .collect();
    match &event.repair {
        RepairAction::VerifySlots(slots) => {
            assert_eq!(
                slots.iter().copied().collect::<BTreeSet<_>>(),
                expected_repair,
                "repair must name exactly the unresolvable boundary's cells"
            );
        }
        other => return Err(anyhow!("expected a targeted slot resync, got {other:?}")),
    }
    assert!(
        !matches!(event.repair, RepairAction::PurgeStorage(_)),
        "a cold boundary must not discard the whole pool"
    );

    let purged = derived.apply(&event.updates);
    let expected_purge: BTreeSet<U256> = cold_tick_keys
        .into_iter()
        .chain([cold_bitmap])
        .chain(position_slots(HARNESS, WARM_TICK, COLD_TICK))
        .collect();
    assert_eq!(
        purged, expected_purge,
        "only the unresolvable boundary and the event's own position are dropped"
    );

    // Everything the parent *could* answer is still established exactly: the
    // in-range mint moved price-adjacent state and the resolvable boundary.
    let mut exact_slots = vec![
        layout.slot0_slot,
        U256::from(1),
        U256::from(2),
        U256::from(3),
        layout.liquidity_slot,
        U256::from(8),
        v3_tick_bitmap_storage_key_with_base(-1, layout.tick_bitmap_base_slot),
    ];
    exact_slots.extend(v3_tick_info_storage_keys_with_base(
        WARM_TICK,
        layout.ticks_base_slot,
    ));
    for slot in exact_slots {
        assert_eq!(
            derived.storage(POOL, slot).unwrap_or_default(),
            reference
                .cached_storage_value(POOL, slot)
                .unwrap_or_default(),
            "cold boundary must not disturb slot {slot}"
        );
    }
    assert_eq!(
        derived.storage(POOL, layout.liquidity_slot),
        Some(scenario.liquidity + U256::from(AMOUNT)),
        "an in-range mint still applies its active-liquidity delta"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Accounting events: `Flash`, `SetFeeProtocol`, `CollectProtocol`, and
// `IncreaseObservationCardinalityNext`.
//
// None of these move price, liquidity, or a tick, and each was previously an
// unknown mutation that discarded the pool's whole storage. Two are
// `onlyFactoryOwner`, so the accounting harness is installed at the address the
// pool's `factory` immutable points to and reports itself as that factory's
// owner — the canonical privileged path executes rather than being stubbed out.
// ---------------------------------------------------------------------------

fn install_accounting_harness(cache: &mut EvmCache) {
    install(
        cache,
        FACTORY,
        runtime(include_str!(
            "fixtures/v3_reference_accounting_harness_runtime.hex"
        )),
        &[
            (U256::ZERO, address_word(TOKEN0)),
            (U256::from(1), address_word(TOKEN1)),
        ],
    );
}

fn accounting_calldata(signature: &str, words: &[U256]) -> Bytes {
    let mut data = Vec::with_capacity(4 + 32 * (words.len() + 1));
    data.extend_from_slice(&keccak256(signature)[..4]);
    data.extend_from_slice(&address_word(POOL).to_be_bytes::<32>());
    for word in words {
        data.extend_from_slice(&word.to_be_bytes::<32>());
    }
    Bytes::from(data)
}

/// Execute one accounting entrypoint against the deployed runtime, replay its
/// log through the adapter, and require the derived state to match every pool
/// slot the reference touched.
async fn run_accounting_case(
    scenario: &Scenario,
    calldata: Bytes,
    topic: B256,
    extra_tracked: &[U256],
    label: &str,
) -> Result<()> {
    let layout = V3StorageLayout::uniswap(scenario.tick_spacing);
    let (pool_slots, declared_slots) = parent_slots(scenario, layout);
    let mut derived = ParentState(
        pool_slots
            .iter()
            .map(|(slot, value)| ((POOL, *slot), *value))
            .collect(),
    );
    let mut reference = cache().await;
    reference.set_timestamp(Some(150));
    reference.set_block_context(Some(100), Some(0));
    install_world(&mut reference, scenario, &pool_slots)?;
    install_accounting_harness(&mut reference);

    let (_, access) = reference.call_raw_with_access_list(CALLER, FACTORY, calldata.clone())?;
    let result = reference.call_raw(CALLER, FACTORY, calldata, true)?;
    let logs = match result {
        ExecutionResult::Success { logs, .. } => logs,
        other => {
            return Err(anyhow!(
                "{label}: deployed bytecode rejected the call: {other:?}"
            ));
        }
    };
    let reference_log: Log = logs
        .into_iter()
        .find(|log| log.address == POOL && log.topics().first() == Some(&topic))
        .ok_or_else(|| anyhow!("{label}: pool emitted no matching log"))?;

    let registration = PoolRegistration::new(PoolKey::UniswapV3(POOL))
        .with_state_address(POOL)
        .with_metadata(ProtocolMetadata::UniswapV3(
            V3Metadata::default()
                .with_fee(scenario.fee)
                .with_tick_spacing(scenario.tick_spacing)
                .with_storage_layout(layout),
        ));
    let context = AdapterEventContext::for_block(100, BLOCK_HASH, 150)
        .with_chain_id(1)
        .with_parent_hash(PARENT_HASH)
        .with_transaction_hash(B256::repeat_byte(0xe0))
        .with_event_order(1, 2);
    let decoded = ConcentratedLiquidityAdapter::default().decode_event_with_context(
        &registration,
        &reference_log,
        &derived,
        &context,
    );
    assert_eq!(decoded.error, None, "{label}");
    let event = decoded.event.expect("accounting event must decode");
    assert_eq!(
        event.quality,
        UpdateQuality::Exact,
        "{label} must be an exact transition"
    );
    assert_eq!(
        event.repair,
        RepairAction::None,
        "{label} must need no repair"
    );

    let before = derived.clone();
    let purged = derived.apply(&event.updates);
    assert!(
        purged.is_empty(),
        "{label} must not drop any pool cell, dropped {purged:?}"
    );

    let touched: BTreeSet<U256> = declared_slots
        .into_iter()
        .chain(extra_tracked.iter().copied())
        .chain(
            access
                .slots
                .into_iter()
                .filter_map(|(address, slot)| (address == POOL).then_some(slot)),
        )
        .collect();
    let mut changed = 0_usize;
    for slot in touched {
        let expected = reference
            .cached_storage_value(POOL, slot)
            .unwrap_or_default();
        assert_eq!(
            derived.storage(POOL, slot).unwrap_or_default(),
            expected,
            "{label}: deployed-bytecode mismatch at slot {slot}"
        );
        if before.storage(POOL, slot).unwrap_or_default() != expected {
            changed += 1;
            assert!(
                event.updates.iter().any(|update| matches!(
                    update,
                    StateUpdate::Slot { address, slot: updated, value }
                        if *address == POOL && *updated == slot && *value == expected
                )),
                "{label} omitted reference-changed slot {slot}"
            );
        }
    }
    assert!(changed > 0, "{label} changed nothing, so it proves nothing");
    assert_executable(scenario, &mut reference, &derived, 150, 100, label).await
}

/// A flash loan credits its fee to LP growth and, when a protocol share is
/// configured, to the protocol accumulator. Both halves follow from the amount
/// actually repaid, `slot0.feeProtocol`, and active liquidity.
#[tokio::test(flavor = "multi_thread")]
async fn flash_credits_fee_growth_and_protocol_fees() -> Result<()> {
    for fee_protocol in [0_u8, 0x44, 0x6a] {
        let mut scenario = base_scenario("flash");
        scenario.fee_protocol = fee_protocol;
        let borrow = U256::from(4_000_000_000_000_u128);
        // Repay the principal plus well over the 0.3% quoted fee.
        let repay = borrow + U256::from(90_000_000_000_u128);
        let calldata = accounting_calldata(
            "executeFlash(address,uint256,uint256,uint256,uint256)",
            &[borrow, borrow, repay, repay],
        );
        run_accounting_case(
            &scenario,
            calldata,
            keccak256("Flash(address,address,uint256,uint256,uint256,uint256)"),
            &[],
            &format!("flash with feeProtocol {fee_protocol:#04x}"),
        )
        .await?;
    }
    Ok(())
}

/// `setFeeProtocol` rewrites one byte of `slot0` and must leave price, tick, and
/// every oracle field packed in that same word untouched.
#[tokio::test(flavor = "multi_thread")]
async fn set_fee_protocol_rewrites_only_its_own_byte() -> Result<()> {
    for (from, to) in [(0_u8, (4_u8, 10_u8)), (0x44, (10, 4)), (0x6a, (0, 0))] {
        let mut scenario = base_scenario("set fee protocol");
        scenario.fee_protocol = from;
        let calldata = accounting_calldata(
            "executeSetFeeProtocol(address,uint8,uint8)",
            &[U256::from(to.0), U256::from(to.1)],
        );
        run_accounting_case(
            &scenario,
            calldata,
            keccak256("SetFeeProtocol(uint8,uint8,uint8,uint8)"),
            &[],
            &format!("setFeeProtocol {from:#04x} -> ({}, {})", to.0, to.1),
        )
        .await?;
    }
    Ok(())
}

/// `collectProtocol` debits the accrued protocol balance by the amount the event
/// reports — including canonical Uniswap's deliberate one-wei remainder when a
/// caller asks for the whole balance, which keeps the slot warm.
#[tokio::test(flavor = "multi_thread")]
async fn collect_protocol_debits_the_accrued_balance() -> Result<()> {
    let accrued0 = U256::from(90_000_u128);
    let accrued1 = U256::from(70_000_u128);
    for (request0, request1) in [(U256::from(1_000), U256::from(2_000)), (accrued0, accrued1)] {
        let mut scenario = base_scenario("collect protocol");
        scenario.fee_protocol = 0x44;
        scenario.overrides = vec![(U256::from(3), accrued0 | (accrued1 << 128_usize))];
        let calldata = accounting_calldata(
            "executeCollectProtocol(address,uint128,uint128)",
            &[request0, request1],
        );
        run_accounting_case(
            &scenario,
            calldata,
            keccak256("CollectProtocol(address,address,uint128,uint128)"),
            &[],
            &format!("collectProtocol requesting ({request0}, {request1})"),
        )
        .await?;
    }
    Ok(())
}

/// Growing the oracle reservation stamps `blockTimestamp = 1` across every newly
/// reserved slot. Those slots are provably untouched in the parent — `write`
/// never reaches an index at or above `observationCardinalityNext` — so the
/// transition writes them without reading any of them.
#[tokio::test(flavor = "multi_thread")]
async fn increase_observation_cardinality_next_reserves_the_ring() -> Result<()> {
    for target in [2_u16, 5, 64] {
        let scenario = base_scenario("grow oracle");
        let calldata = accounting_calldata(
            "executeIncreaseObservationCardinalityNext(address,uint16)",
            &[U256::from(target)],
        );
        let reserved: Vec<U256> = (0..target).map(|i| U256::from(8) + U256::from(i)).collect();
        run_accounting_case(
            &scenario,
            calldata,
            keccak256("IncreaseObservationCardinalityNext(uint16,uint16)"),
            &reserved,
            &format!("increaseObservationCardinalityNext to {target}"),
        )
        .await?;
    }
    Ok(())
}
