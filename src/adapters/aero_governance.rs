//! Event-driven Aero CL fee/governance configuration.
//!
//! The reviewed Aero CL quote path reads configuration cells on the shared
//! `DynamicSwapFeeHook`, the chain's `PriorityFeeMevTaxModule`, the pool's
//! factory, the wired unstaked-fee module and the discount registry. Their
//! setters emit events (topic hashes confirmed against every log those
//! contracts emitted on Ethereum since 26 238 245 and on Base since
//! 52 881 649), so a change is replayed at its block instead of waiting for a
//! repair:
//!
//! - exact cell writes where the deployed packing is confirmed by live reads
//!   (hook `dynamicFeeConfig[pool]`: `baseFee` bits 0..24, `feeCap` bits
//!   24..48, `scalingFactor` bits 48..112; hook defaults and
//!   `secondsAgo | recordCallGasLimit`; MEV `priorityFeeMultiplier` at bits
//!   160..224 of slot 1 and `minThreshold | baseFeeFactor` in slot 2; factory
//!   `swapFeeManager | defaultUnstakedFee` and `tickSpacingToFee`);
//! - fail-closed repairs where the change replaces a reviewed runtime (a new
//!   hook, MEV module, discount registry, pool tape or unstaked-fee module:
//!   the pool is purged and its next cold start re-checks identities), or
//!   where a setter's packing is unconfirmed (initial-fee setters, dynamic
//!   fee resets, discount-registry writes, unstaked-fee module writes: the
//!   affected cells are purged and verified).
//!
//! Hook events carry the pool as `topic1`; events for other pools never route
//! to a reviewed registration.

use super::storage::layout_for;
use super::uniswap_v3::{
    AERO_CL_DISCOUNT_REGISTRY_DISCOUNTS_SLOT, AERO_CL_FACTORY_SWAP_FEE_MANAGER_SLOT,
    AERO_CL_FACTORY_TICK_SPACING_TO_FEE_SLOT, AERO_CL_HOOK_DEFAULT_FEE_CAP_SLOT,
    AERO_CL_HOOK_DEFAULT_SCALING_FACTOR_SLOT, AERO_CL_HOOK_DYNAMIC_FEE_CONFIG_SLOT,
    AERO_CL_HOOK_MEV_TAX_MODULE_SLOT, AERO_CL_HOOK_SECONDS_AGO_SLOT,
    AERO_CL_MEV_MODULE_MULTIPLIER_SLOT, AERO_CL_MEV_MODULE_THRESHOLD_SLOT,
    AERO_CL_UNSTAKED_FEE_MODULE_CUSTOM_FEE_SLOT, AeroClGovernanceRuntime,
    aero_cl_governance_runtime_for_pool, aero_cl_governance_runtimes, solidity_address_mapping_key,
    solidity_i24_mapping_key,
};
use super::v3_transition::validate_reviewed_slipstream_event;
use super::{
    AdapterEvent, AdapterEventContext, AdapterEventError, AdapterEventKind, AdapterEventResult,
    AdapterRegistry, EventSource, PoolKey, PoolRegistration, ProtocolId, PurgeScope, RepairAction,
    StateUpdate, StateView, UpdateQuality, V3TransitionError,
};
use alloy_primitives::{Address, B256, Log, U256};
use alloy_sol_types::SolEvent;

mod abi {
    alloy_sol_types::sol! {
        // DynamicSwapFeeHook (deployed signatures).
        event CustomFeeSet(address indexed pool, uint24 fee);
        event FeeCapSet(address indexed pool, uint256 feeCap);
        event ScalingFactorSet(address indexed pool, uint256 scalingFactor);
        event DefaultScalingFactorSet(uint256 defaultScalingFactor);
        event DefaultFeeCapSet(uint256 defaultFeeCap);
        event SecondsAgoSet(uint32 secondsAgo);
        event RecordCallGasLimitSet(uint32 gasLimit);
        event MevTaxModuleSet(address indexed mevTaxModule);
        event InitialFeeSet(address indexed pool, uint24 fee);
        event InitialFeeDisabled(address indexed pool);
        event DynamicFeeReset(address indexed pool);
        // PriorityFeeMevTaxModule.
        event MultiplierSet(uint64 priorityFeeMultiplier);
        event MinThresholdSet(uint96 minThreshold);
        event BaseFeeFactorSet(uint96 baseFeeFactor);
        // CLFactory.
        event TickSpacingEnabled(int24 indexed tickSpacing, uint24 indexed fee);
        event DefaultUnstakedFeeChanged(uint24 indexed oldUnstakedFee, uint24 indexed newUnstakedFee);
        event SwapFeeManagerChanged(address indexed oldFeeManager, address indexed newFeeManager);
        event UnstakedFeeModuleChanged(address indexed oldFeeModule, address indexed newFeeModule);
        event DefaultSwapHookChanged(address indexed swapHook);
        event PoolSwapHookChanged(address indexed pool, address indexed swapHook);
        event DiscountRegistryChanged(address indexed discountRegistry);
        event ClPoolTapeChanged(address indexed clPoolTape);
        // DiscountRegistry.
        event DiscountSet(address indexed account, uint24 discount);
        event PoolDiscountSet(address indexed pool, address indexed account, uint24 discount);
    }
}

/// The hook's second variant of the initial-fee setter (`uint256` payload);
/// unobserved on chain, handled fail-closed like the `uint24` one.
mod abi_alt {
    alloy_sol_types::sol! {
        event InitialFeeSet(address indexed pool, uint256 fee);
    }
}

/// The third discount-registry event (`0xc5dba97f…`), name unresolved; any
/// registry write invalidates the discount cells.
const DISCOUNT_REGISTRY_UNRESOLVED_TOPIC: B256 =
    alloy_primitives::b256!("c5dba97f0ff39107a9809c03b91dc45fd06f59c08ba8c2b863f9cdd849358419");

const MASK_24: U256 = U256::from_limbs([0xff_ffff, 0, 0, 0]);
const MASK_32: U256 = U256::from_limbs([0xffff_ffff, 0, 0, 0]);
const MASK_64: U256 = U256::from_limbs([u64::MAX, 0, 0, 0]);
const MASK_96: U256 = U256::from_limbs([u64::MAX, u32::MAX as u64, 0, 0]);
const MASK_160: U256 = U256::from_limbs([u64::MAX, u64::MAX, u32::MAX as u64, 0]);

/// Hook events scoped to one pool (`topic1`).
fn hook_pool_topics() -> Vec<B256> {
    vec![
        abi::CustomFeeSet::SIGNATURE_HASH,
        abi::FeeCapSet::SIGNATURE_HASH,
        abi::ScalingFactorSet::SIGNATURE_HASH,
        abi::InitialFeeSet::SIGNATURE_HASH,
        abi_alt::InitialFeeSet::SIGNATURE_HASH,
        abi::InitialFeeDisabled::SIGNATURE_HASH,
        abi::DynamicFeeReset::SIGNATURE_HASH,
    ]
}

/// Hook events that apply to every pool.
fn hook_global_topics() -> Vec<B256> {
    vec![
        abi::DefaultScalingFactorSet::SIGNATURE_HASH,
        abi::DefaultFeeCapSet::SIGNATURE_HASH,
        abi::SecondsAgoSet::SIGNATURE_HASH,
        abi::RecordCallGasLimitSet::SIGNATURE_HASH,
        abi::MevTaxModuleSet::SIGNATURE_HASH,
    ]
}

fn mev_topics() -> Vec<B256> {
    vec![
        abi::MultiplierSet::SIGNATURE_HASH,
        abi::MinThresholdSet::SIGNATURE_HASH,
        abi::BaseFeeFactorSet::SIGNATURE_HASH,
    ]
}

fn factory_global_topics() -> Vec<B256> {
    vec![
        abi::TickSpacingEnabled::SIGNATURE_HASH,
        abi::DefaultUnstakedFeeChanged::SIGNATURE_HASH,
        abi::SwapFeeManagerChanged::SIGNATURE_HASH,
        abi::UnstakedFeeModuleChanged::SIGNATURE_HASH,
        abi::DefaultSwapHookChanged::SIGNATURE_HASH,
        abi::DiscountRegistryChanged::SIGNATURE_HASH,
        abi::ClPoolTapeChanged::SIGNATURE_HASH,
    ]
}

fn registry_topics() -> Vec<B256> {
    vec![
        abi::DiscountSet::SIGNATURE_HASH,
        abi::PoolDiscountSet::SIGNATURE_HASH,
        DISCOUNT_REGISTRY_UNRESOLVED_TOPIC,
    ]
}

fn runtime_for(pool: &PoolRegistration) -> Option<AeroClGovernanceRuntime> {
    if pool.protocol() != ProtocolId::Slipstream {
        return None;
    }
    pool.key
        .address()
        .and_then(aero_cl_governance_runtime_for_pool)
}

/// Event sources a reviewed Aero CL registration subscribes beyond its pool
/// and gauge.
pub(crate) fn sources(pool: &PoolRegistration) -> Vec<EventSource> {
    let Some(runtime) = runtime_for(pool) else {
        return Vec::new();
    };
    let mut sources = vec![
        EventSource::indexed_address(runtime.hook, hook_pool_topics(), 1),
        EventSource::adapter_defined(runtime.hook, hook_global_topics()),
        EventSource::adapter_defined(runtime.factory, factory_global_topics()),
        EventSource::indexed_address(
            runtime.factory,
            vec![abi::PoolSwapHookChanged::SIGNATURE_HASH],
            1,
        ),
        EventSource::adapter_defined(runtime.discount_registry, registry_topics()),
    ];
    if let Some(module) = runtime.mev_tax_module {
        sources.push(EventSource::adapter_defined(module, mev_topics()));
    }
    if runtime.unstaked_fee_module_wired {
        sources.push(EventSource::indexed_address(
            runtime.unstaked_fee_module,
            vec![abi::CustomFeeSet::SIGNATURE_HASH],
            1,
        ));
    }
    sources
}

/// Route a governance log to the reviewed Aero CL registration it configures.
pub(crate) fn route(log: &Log, registry: &AdapterRegistry) -> Option<PoolKey> {
    let topic0 = log.topics().first().copied()?;
    aero_cl_governance_runtimes()
        .into_iter()
        .find_map(|runtime| {
            let key = PoolKey::Slipstream(runtime.pool);
            registry.pool(&key)?;
            let pool_scoped = |topics: &[B256]| {
                topics.contains(&topic0)
                    && log
                        .topics()
                        .get(1)
                        .is_some_and(|topic| *topic == runtime.pool.into_word())
            };
            let routed = if log.address == runtime.hook {
                hook_global_topics().contains(&topic0) || pool_scoped(&hook_pool_topics())
            } else if log.address == runtime.factory {
                factory_global_topics().contains(&topic0)
                    || pool_scoped(&[abi::PoolSwapHookChanged::SIGNATURE_HASH])
            } else if log.address == runtime.discount_registry {
                registry_topics().contains(&topic0)
            } else if Some(log.address) == runtime.mev_tax_module {
                mev_topics().contains(&topic0)
            } else if log.address == runtime.unstaked_fee_module
                && runtime.unstaked_fee_module_wired
            {
                pool_scoped(&[abi::CustomFeeSet::SIGNATURE_HASH])
            } else {
                false
            };
            routed.then_some(key)
        })
}

fn failure(reason: &'static str) -> AdapterEventError {
    AdapterEventError::V3Transition(V3TransitionError::SlipstreamFeeEvidence(reason))
}

/// Replace `width` bits at `offset` of `word` with `value`, failing when the
/// value does not fit.
fn set_bits(word: U256, offset: usize, mask: U256, value: U256) -> Result<U256, AdapterEventError> {
    if value > mask {
        return Err(failure("governance value exceeds its packed field"));
    }
    Ok((word & !(mask << offset)) | (value << offset))
}

enum Transition {
    /// Exact writes.
    Exact(Vec<StateUpdate>),
    /// The written cells cannot be derived; purge and verify them.
    Verify(Vec<(Address, U256)>),
    /// A reviewed runtime binding changed; the pool must be re-reviewed.
    Identity,
}

fn config_key(runtime: AeroClGovernanceRuntime) -> U256 {
    solidity_address_mapping_key(runtime.pool, AERO_CL_HOOK_DYNAMIC_FEE_CONFIG_SLOT)
}

fn derive(
    runtime: AeroClGovernanceRuntime,
    log: &Log,
    state: &dyn StateView,
) -> Result<Transition, AdapterEventError> {
    let topic0 = *log
        .topics()
        .first()
        .ok_or_else(|| failure("governance log without topic"))?;
    let read = |address: Address, slot: U256| {
        state
            .storage(address, slot)
            .ok_or(AdapterEventError::MissingState { address, slot })
    };
    let packed = |address: Address, slot: U256, offset: usize, mask: U256, value: U256| {
        let word = read(address, slot)?;
        Ok::<_, AdapterEventError>(Transition::Exact(vec![StateUpdate::slot(
            address,
            slot,
            set_bits(word, offset, mask, value)?,
        )]))
    };
    let whole = |address: Address, slot: U256, value: U256| {
        Transition::Exact(vec![StateUpdate::slot(address, slot, value)])
    };
    let hook = runtime.hook;
    let factory = runtime.factory;
    if log.address == hook {
        if topic0 == abi::CustomFeeSet::SIGNATURE_HASH {
            let event = decode::<abi::CustomFeeSet>(log)?;
            return packed(
                hook,
                config_key(runtime),
                0,
                MASK_24,
                U256::from(event.fee.to::<u32>()),
            );
        }
        if topic0 == abi::FeeCapSet::SIGNATURE_HASH {
            let event = decode::<abi::FeeCapSet>(log)?;
            return packed(hook, config_key(runtime), 24, MASK_24, event.feeCap);
        }
        if topic0 == abi::ScalingFactorSet::SIGNATURE_HASH {
            let event = decode::<abi::ScalingFactorSet>(log)?;
            return packed(hook, config_key(runtime), 48, MASK_64, event.scalingFactor);
        }
        if topic0 == abi::DefaultScalingFactorSet::SIGNATURE_HASH {
            let event = decode::<abi::DefaultScalingFactorSet>(log)?;
            return Ok(whole(
                hook,
                U256::from(AERO_CL_HOOK_DEFAULT_SCALING_FACTOR_SLOT),
                event.defaultScalingFactor,
            ));
        }
        if topic0 == abi::DefaultFeeCapSet::SIGNATURE_HASH {
            let event = decode::<abi::DefaultFeeCapSet>(log)?;
            return Ok(whole(
                hook,
                U256::from(AERO_CL_HOOK_DEFAULT_FEE_CAP_SLOT),
                event.defaultFeeCap,
            ));
        }
        if topic0 == abi::SecondsAgoSet::SIGNATURE_HASH {
            let event = decode::<abi::SecondsAgoSet>(log)?;
            return packed(
                hook,
                U256::from(AERO_CL_HOOK_SECONDS_AGO_SLOT),
                0,
                MASK_32,
                U256::from(event.secondsAgo),
            );
        }
        if topic0 == abi::RecordCallGasLimitSet::SIGNATURE_HASH {
            let event = decode::<abi::RecordCallGasLimitSet>(log)?;
            return packed(
                hook,
                U256::from(AERO_CL_HOOK_SECONDS_AGO_SLOT),
                32,
                MASK_32,
                U256::from(event.gasLimit),
            );
        }
        if topic0 == abi::MevTaxModuleSet::SIGNATURE_HASH {
            let event = decode::<abi::MevTaxModuleSet>(log)?;
            let current = read(hook, U256::from(AERO_CL_HOOK_MEV_TAX_MODULE_SLOT))?;
            // Re-setting the reviewed module is an exact no-op write; any
            // other module is unreviewed code on the quote path.
            return if Address::from_slice(&current.to_be_bytes::<32>()[12..]) == event.mevTaxModule
            {
                Ok(whole(
                    hook,
                    U256::from(AERO_CL_HOOK_MEV_TAX_MODULE_SLOT),
                    current,
                ))
            } else {
                Ok(Transition::Identity)
            };
        }
        // Initial-fee setters and dynamic-fee resets write fields whose
        // packing is not confirmed by a live read: verify the config word.
        return Ok(Transition::Verify(vec![(hook, config_key(runtime))]));
    }
    if Some(log.address) == runtime.mev_tax_module {
        let module = log.address;
        if topic0 == abi::MultiplierSet::SIGNATURE_HASH {
            let event = decode::<abi::MultiplierSet>(log)?;
            return packed(
                module,
                U256::from(AERO_CL_MEV_MODULE_MULTIPLIER_SLOT),
                160,
                MASK_64,
                U256::from(event.priorityFeeMultiplier),
            );
        }
        if topic0 == abi::MinThresholdSet::SIGNATURE_HASH {
            let event = decode::<abi::MinThresholdSet>(log)?;
            return packed(
                module,
                U256::from(AERO_CL_MEV_MODULE_THRESHOLD_SLOT),
                0,
                MASK_96,
                U256::from(event.minThreshold),
            );
        }
        if topic0 == abi::BaseFeeFactorSet::SIGNATURE_HASH {
            let event = decode::<abi::BaseFeeFactorSet>(log)?;
            return packed(
                module,
                U256::from(AERO_CL_MEV_MODULE_THRESHOLD_SLOT),
                96,
                MASK_96,
                U256::from(event.baseFeeFactor),
            );
        }
        return Ok(Transition::Verify(vec![
            (module, U256::from(AERO_CL_MEV_MODULE_MULTIPLIER_SLOT)),
            (module, U256::from(AERO_CL_MEV_MODULE_THRESHOLD_SLOT)),
        ]));
    }
    if log.address == factory {
        if topic0 == abi::TickSpacingEnabled::SIGNATURE_HASH {
            let event = decode::<abi::TickSpacingEnabled>(log)?;
            if event.tickSpacing.as_i32() != runtime.tick_spacing {
                return Ok(Transition::Exact(Vec::new()));
            }
            return Ok(whole(
                factory,
                solidity_i24_mapping_key(
                    runtime.tick_spacing,
                    AERO_CL_FACTORY_TICK_SPACING_TO_FEE_SLOT,
                ),
                U256::from(event.fee.to::<u32>()),
            ));
        }
        if topic0 == abi::DefaultUnstakedFeeChanged::SIGNATURE_HASH {
            let event = decode::<abi::DefaultUnstakedFeeChanged>(log)?;
            return packed(
                factory,
                U256::from(AERO_CL_FACTORY_SWAP_FEE_MANAGER_SLOT),
                160,
                MASK_24,
                U256::from(event.newUnstakedFee.to::<u32>()),
            );
        }
        if topic0 == abi::SwapFeeManagerChanged::SIGNATURE_HASH {
            let event = decode::<abi::SwapFeeManagerChanged>(log)?;
            return packed(
                factory,
                U256::from(AERO_CL_FACTORY_SWAP_FEE_MANAGER_SLOT),
                0,
                MASK_160,
                U256::from_be_slice(event.newFeeManager.as_slice()),
            );
        }
        // Hook, tape, registry and unstaked-fee module rebinding.
        return Ok(Transition::Identity);
    }
    if log.address == runtime.discount_registry {
        let mut cells = vec![(
            runtime.discount_registry,
            solidity_address_mapping_key(Address::ZERO, AERO_CL_DISCOUNT_REGISTRY_DISCOUNTS_SLOT),
        )];
        if let Some(account) = log
            .topics()
            .get(1)
            .map(|topic| Address::from_slice(&topic[12..]))
        {
            cells.push((
                runtime.discount_registry,
                solidity_address_mapping_key(account, AERO_CL_DISCOUNT_REGISTRY_DISCOUNTS_SLOT),
            ));
        }
        if let Some(account) = log
            .topics()
            .get(2)
            .map(|topic| Address::from_slice(&topic[12..]))
        {
            cells.push((
                runtime.discount_registry,
                solidity_address_mapping_key(account, AERO_CL_DISCOUNT_REGISTRY_DISCOUNTS_SLOT),
            ));
        }
        return Ok(Transition::Verify(cells));
    }
    if log.address == runtime.unstaked_fee_module {
        return Ok(Transition::Verify(vec![(
            runtime.unstaked_fee_module,
            solidity_address_mapping_key(runtime.pool, AERO_CL_UNSTAKED_FEE_MODULE_CUSTOM_FEE_SLOT),
        )]));
    }
    Err(failure("governance log from an unreviewed emitter"))
}

fn decode<E: SolEvent>(log: &Log) -> Result<E, AdapterEventError> {
    let decoded = E::decode_log_validate(log).map_err(|_| failure("malformed governance log"))?;
    if decoded.data.encode_log_data() != log.data {
        return Err(failure("non-canonical governance log payload"));
    }
    Ok(decoded.data)
}

/// Decode a governance log for a reviewed Aero CL registration; `None` when
/// the log is not a governance log of this registration.
pub(crate) fn decode_event(
    pool: &PoolRegistration,
    log: &Log,
    state: &dyn StateView,
    context: &AdapterEventContext,
) -> Option<AdapterEventResult> {
    let runtime = runtime_for(pool)?;
    let emitters = [
        runtime.hook,
        runtime.factory,
        runtime.discount_registry,
        runtime.unstaked_fee_module,
    ];
    if !emitters.contains(&log.address) && Some(log.address) != runtime.mev_tax_module {
        return None;
    }
    let topic0 = log.topics().first().copied()?;
    let event = |quality, updates: Vec<StateUpdate>, repair| AdapterEvent {
        pool: pool.key.clone(),
        emitter: log.address,
        topic0,
        kind: AdapterEventKind::Unknown,
        updates,
        quality,
        repair,
    };
    let verify = |cells: Vec<(Address, U256)>, error: Option<AdapterEventError>| {
        let mut by_address: std::collections::BTreeMap<Address, Vec<U256>> = Default::default();
        for (address, slot) in &cells {
            by_address.entry(*address).or_default().push(*slot);
        }
        let updates = by_address
            .into_iter()
            .map(|(address, slots)| StateUpdate::purge(address, PurgeScope::Slots(slots)))
            .collect();
        let event = event(
            UpdateQuality::RequiresRepair,
            updates,
            RepairAction::VerifySlots(cells),
        );
        match error {
            Some(error) => AdapterEventResult::event_with_error(event, error),
            None => AdapterEventResult::event(event),
        }
    };
    let identity = |error: AdapterEventError| {
        AdapterEventResult::event_with_error(
            event(
                UpdateQuality::RequiresRepair,
                vec![StateUpdate::purge(runtime.pool, PurgeScope::AllStorage)],
                RepairAction::PurgeStorage(runtime.pool),
            ),
            error,
        )
    };
    let layout = match layout_for(pool) {
        Some(layout) => layout,
        None => return Some(identity(failure("missing Slipstream storage layout"))),
    };
    if let Err(error) = validate_reviewed_slipstream_event(runtime.pool, layout, context) {
        return Some(identity(error));
    }
    Some(match derive(runtime, log, state) {
        Ok(Transition::Exact(updates)) => {
            AdapterEventResult::event(event(UpdateQuality::Exact, updates, RepairAction::None))
        }
        Ok(Transition::Verify(cells)) => verify(cells, None),
        Ok(Transition::Identity) => identity(failure(
            "a reviewed runtime binding on the quote path changed; re-review before quoting",
        )),
        Err(AdapterEventError::MissingState { address, slot }) => verify(
            vec![(address, slot)],
            Some(AdapterEventError::MissingState { address, slot }),
        ),
        Err(error) => identity(error),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::storage::V3StorageLayout;
    use crate::adapters::{ProtocolMetadata, V3Metadata};
    use alloy_primitives::{B256, address, b256};
    use std::collections::BTreeMap;

    const BASE_POOL: Address = address!("eebe78d734dc4bac027061cee9bc12c5803fe4f8");
    const HOOK: Address = address!("c0ed888afe87ed1acf01d9f19bb850159e3ce434");
    const MEV: Address = address!("29f12ce5be3d88b56d245b7f4e969c518f799a4a");
    const FACTORY: Address = address!("9cb220581b91b0c83223012fe69688b92ad07a76");

    #[derive(Default)]
    struct State(BTreeMap<(Address, U256), U256>);
    impl StateView for State {
        fn storage(&self, address: Address, slot: U256) -> Option<U256> {
            self.0.get(&(address, slot)).copied()
        }
    }

    fn registration() -> PoolRegistration {
        PoolRegistration::new(PoolKey::Slipstream(BASE_POOL))
            .with_state_address(BASE_POOL)
            .with_metadata(ProtocolMetadata::Slipstream(
                V3Metadata::default()
                    .with_tick_spacing(100)
                    .with_storage_layout(V3StorageLayout::aero_cl(100)),
            ))
    }

    fn context() -> AdapterEventContext {
        AdapterEventContext::for_block(53_205_395, B256::repeat_byte(0xaa), 1_793_200_000)
            .with_chain_id(8_453)
            .with_parent_hash(B256::repeat_byte(0xbb))
            .with_transaction_hash(B256::repeat_byte(0xcc))
            .with_event_order(1, 2)
    }

    fn log<E: SolEvent>(emitter: Address, event: E) -> Log {
        Log {
            address: emitter,
            data: event.encode_log_data(),
        }
    }

    fn apply(state: &mut State, result: AdapterEventResult) -> AdapterEvent {
        let event = result.event.expect("event");
        assert!(result.error.is_none(), "{:?}", result.error);
        assert_eq!(event.quality, UpdateQuality::Exact);
        assert_eq!(event.repair, RepairAction::None);
        for update in &event.updates {
            if let StateUpdate::Slot {
                address,
                slot,
                value,
            } = update
            {
                state.0.insert((*address, *slot), *value);
            }
        }
        event
    }

    /// The three per-pool setters reconstruct the packed config word of a
    /// live Base pool (`0xbb5507da…`, read at head: baseFee 250, feeCap
    /// 50000, scalingFactor 100000000) from zero, so the deployed packing is
    /// pinned by this test.
    #[test]
    fn per_pool_setters_reconstruct_the_live_config_word() {
        let registration = registration();
        let mut state = State::default();
        let key = config_key(aero_cl_governance_runtime_for_pool(BASE_POOL).unwrap());
        state.0.insert((HOOK, key), U256::ZERO);
        for log in [
            log(
                HOOK,
                abi::CustomFeeSet {
                    pool: BASE_POOL,
                    fee: alloy_primitives::aliases::U24::from(250_u32),
                },
            ),
            log(
                HOOK,
                abi::FeeCapSet {
                    pool: BASE_POOL,
                    feeCap: U256::from(50_000),
                },
            ),
            log(
                HOOK,
                abi::ScalingFactorSet {
                    pool: BASE_POOL,
                    scalingFactor: U256::from(100_000_000_u64),
                },
            ),
        ] {
            let result = decode_event(&registration, &log, &state, &context()).expect("routed");
            apply(&mut state, result);
        }
        assert_eq!(
            state.0[&(HOOK, key)],
            U256::from_be_bytes(
                b256!("0000000000000000000000000000000000000000000005f5e10000c3500000fa").0
            )
        );
        // Re-setting a field replaces only that field.
        let result = decode_event(
            &registration,
            &log(
                HOOK,
                abi::FeeCapSet {
                    pool: BASE_POOL,
                    feeCap: U256::from(5_400),
                },
            ),
            &state,
            &context(),
        )
        .unwrap();
        apply(&mut state, result);
        assert_eq!(
            state.0[&(HOOK, key)],
            U256::from_be_bytes(
                b256!("0000000000000000000000000000000000000000000005f5e1000015180000fa").0
            )
        );
    }

    #[test]
    fn setters_for_other_pools_do_not_route_or_apply() {
        let other = address!("bb5507daae822ba89c4da61bbfdf4b49a1666bac");
        let mut registry = AdapterRegistry::new();
        registry
            .register_adapter(std::sync::Arc::new(
                crate::adapters::ConcentratedLiquidityAdapter::default(),
            ))
            .unwrap();
        registry.register_pool(registration()).unwrap();
        let foreign = log(
            HOOK,
            abi::FeeCapSet {
                pool: other,
                feeCap: U256::from(5_400),
            },
        );
        assert_eq!(route(&foreign, &registry), None);
        let ours = log(
            HOOK,
            abi::FeeCapSet {
                pool: BASE_POOL,
                feeCap: U256::from(5_400),
            },
        );
        assert_eq!(
            route(&ours, &registry),
            Some(PoolKey::Slipstream(BASE_POOL))
        );
        let global = log(
            HOOK,
            abi::DefaultFeeCapSet {
                defaultFeeCap: U256::from(40_000),
            },
        );
        assert_eq!(
            route(&global, &registry),
            Some(PoolKey::Slipstream(BASE_POOL))
        );
    }

    #[test]
    fn global_hook_and_mev_setters_write_their_packed_cells() {
        let registration = registration();
        let mut state = State::default();
        state.0.insert(
            (HOOK, U256::from(AERO_CL_HOOK_SECONDS_AGO_SLOT)),
            U256::from(0x3d09000000258_u64),
        );
        state.0.insert((MEV, U256::from(1)), U256::ZERO);
        state.0.insert((MEV, U256::from(2)), U256::ZERO);
        let cases: Vec<(Log, Address, U256, U256)> = vec![
            (
                log(HOOK, abi::SecondsAgoSet { secondsAgo: 900 }),
                HOOK,
                U256::from(AERO_CL_HOOK_SECONDS_AGO_SLOT),
                U256::from(0x3d090_u64) << 32 | U256::from(900),
            ),
            (
                log(HOOK, abi::RecordCallGasLimitSet { gasLimit: 300_000 }),
                HOOK,
                U256::from(AERO_CL_HOOK_SECONDS_AGO_SLOT),
                U256::from(300_000_u64) << 32 | U256::from(900),
            ),
            (
                log(
                    HOOK,
                    abi::DefaultScalingFactorSet {
                        defaultScalingFactor: U256::from(7),
                    },
                ),
                HOOK,
                U256::from(AERO_CL_HOOK_DEFAULT_SCALING_FACTOR_SLOT),
                U256::from(7),
            ),
            (
                log(
                    MEV,
                    abi::MultiplierSet {
                        priorityFeeMultiplier: 5,
                    },
                ),
                MEV,
                U256::from(1),
                U256::from(5) << 160,
            ),
            (
                log(
                    MEV,
                    abi::MinThresholdSet {
                        minThreshold: alloy_primitives::aliases::U96::from(1_000_000_000_u64),
                    },
                ),
                MEV,
                U256::from(2),
                U256::from(1_000_000_000_u64),
            ),
            (
                log(
                    MEV,
                    abi::BaseFeeFactorSet {
                        baseFeeFactor: alloy_primitives::aliases::U96::from(3_u64),
                    },
                ),
                MEV,
                U256::from(2),
                U256::from(3) << 96 | U256::from(1_000_000_000_u64),
            ),
            (
                log(
                    FACTORY,
                    abi::DefaultUnstakedFeeChanged {
                        oldUnstakedFee: alloy_primitives::aliases::U24::from(0_u32),
                        newUnstakedFee: alloy_primitives::aliases::U24::from(50_000_u32),
                    },
                ),
                FACTORY,
                U256::from(AERO_CL_FACTORY_SWAP_FEE_MANAGER_SLOT),
                U256::from(50_000) << 160,
            ),
        ];
        state.0.insert(
            (FACTORY, U256::from(AERO_CL_FACTORY_SWAP_FEE_MANAGER_SLOT)),
            U256::ZERO,
        );
        for (log, address, slot, expected) in cases {
            let result = decode_event(&registration, &log, &state, &context()).expect("routed");
            apply(&mut state, result);
            assert_eq!(state.0[&(address, slot)], expected, "{log:?}");
        }
    }

    #[test]
    fn runtime_rebinding_and_unconfirmed_setters_fail_closed() {
        let registration = registration();
        let state = State::default();
        let rebinding = log(
            FACTORY,
            abi::DefaultSwapHookChanged {
                swapHook: Address::repeat_byte(0x77),
            },
        );
        let result = decode_event(&registration, &rebinding, &state, &context()).unwrap();
        let event = result.event.unwrap();
        assert_eq!(event.quality, UpdateQuality::RequiresRepair);
        assert_eq!(event.repair, RepairAction::PurgeStorage(BASE_POOL));
        let unconfirmed = log(HOOK, abi::DynamicFeeReset { pool: BASE_POOL });
        let result = decode_event(&registration, &unconfirmed, &state, &context()).unwrap();
        let event = result.event.unwrap();
        assert_eq!(event.quality, UpdateQuality::RequiresRepair);
        let key = config_key(aero_cl_governance_runtime_for_pool(BASE_POOL).unwrap());
        assert_eq!(event.repair, RepairAction::VerifySlots(vec![(HOOK, key)]));
        // A packed write needs the parent word: missing state verifies it.
        let packed = log(
            HOOK,
            abi::CustomFeeSet {
                pool: BASE_POOL,
                fee: alloy_primitives::aliases::U24::from(1_u32),
            },
        );
        let result = decode_event(&registration, &packed, &state, &context()).unwrap();
        assert!(result.error.is_some());
        assert_eq!(
            result.event.unwrap().repair,
            RepairAction::VerifySlots(vec![(HOOK, key)])
        );
    }

    #[test]
    fn signatures_match_the_deployed_contracts() {
        let prefixes: [(B256, [u8; 4]); 17] = [
            (abi::CustomFeeSet::SIGNATURE_HASH, [0xe2, 0x15, 0x0e, 0x40]),
            (abi::FeeCapSet::SIGNATURE_HASH, [0x58, 0x0e, 0x5c, 0xae]),
            (
                abi::ScalingFactorSet::SIGNATURE_HASH,
                [0x30, 0xc2, 0x47, 0xef],
            ),
            (
                abi::DefaultScalingFactorSet::SIGNATURE_HASH,
                [0xac, 0xcd, 0xee, 0x31],
            ),
            (
                abi::DefaultFeeCapSet::SIGNATURE_HASH,
                [0x7e, 0x32, 0xf9, 0x8c],
            ),
            (
                abi::RecordCallGasLimitSet::SIGNATURE_HASH,
                [0xaf, 0x82, 0x4d, 0xca],
            ),
            (
                abi::MevTaxModuleSet::SIGNATURE_HASH,
                [0xff, 0xd5, 0x75, 0x93],
            ),
            (abi::MultiplierSet::SIGNATURE_HASH, [0x3d, 0x81, 0x4c, 0x33]),
            (
                abi::MinThresholdSet::SIGNATURE_HASH,
                [0x58, 0x35, 0x9f, 0xd8],
            ),
            (
                abi::BaseFeeFactorSet::SIGNATURE_HASH,
                [0x30, 0xad, 0xb8, 0x0d],
            ),
            (
                abi::TickSpacingEnabled::SIGNATURE_HASH,
                [0xeb, 0xaf, 0xae, 0x46],
            ),
            (
                abi::DefaultUnstakedFeeChanged::SIGNATURE_HASH,
                [0xcb, 0xca, 0x61, 0x14],
            ),
            (
                abi::SwapFeeManagerChanged::SIGNATURE_HASH,
                [0x7a, 0xe0, 0x00, 0x72],
            ),
            (
                abi::UnstakedFeeModuleChanged::SIGNATURE_HASH,
                [0x65, 0x20, 0xf4, 0x04],
            ),
            (
                abi::DefaultSwapHookChanged::SIGNATURE_HASH,
                [0x21, 0xf0, 0x9b, 0xe7],
            ),
            (
                abi::DiscountRegistryChanged::SIGNATURE_HASH,
                [0x1b, 0xda, 0x15, 0xac],
            ),
            (
                abi::ClPoolTapeChanged::SIGNATURE_HASH,
                [0xa6, 0xd3, 0x46, 0xec],
            ),
        ];
        for (hash, prefix) in prefixes {
            assert_eq!(&hash.as_slice()[..4], &prefix, "{hash:#x}");
        }
        assert_eq!(
            &abi::DiscountSet::SIGNATURE_HASH.as_slice()[..4],
            &[0x5d, 0xe8, 0x7a, 0x2a]
        );
        assert_eq!(
            &abi::PoolDiscountSet::SIGNATURE_HASH.as_slice()[..4],
            &[0xb5, 0x44, 0xa8, 0x20]
        );
    }
}
