use std::borrow::Cow;
use std::collections::BTreeMap;

use alloy_network::Ethereum;
use alloy_primitives::{Address, B256, U256};
use alloy_rpc_types_eth::Filter;
use evm_fork_cache::reactive::{
    HandlerError, HandlerId, HandlerOutcome, HookSignal, InvalidationReason, InvalidationRequest,
    LogInterest, ReactiveContext, ReactiveEffect, ReactiveHandler, ReactiveInput, ReactiveInterest,
    ReportTag, ResyncBlock, ResyncId, ResyncPriority, ResyncReason, ResyncRequest, ResyncTarget,
    RouteKeySpec, StateEffectQuality,
};

use super::{
    AdapterEvent, AdapterRegistry, EventRoute, EventSource, PoolKey, PoolRegistration, PurgeScope,
    RepairAction, SkippedDelta, SkippedMask, StateDiff, StateUpdate, StateView, UpdateQuality,
};

const HANDLER_ID: &str = "evm-amm-state.adapters";
const HOOK_NAMESPACE: &str = "evm-amm-state";

/// Reactive-runtime bridge for the AMM adapter registry.
#[derive(Clone, Debug)]
pub struct AmmReactiveHandler {
    registry: AdapterRegistry,
}

impl AmmReactiveHandler {
    pub fn new(registry: AdapterRegistry) -> Self {
        Self { registry }
    }

    pub fn id(&self) -> HandlerId {
        HandlerId::new(HANDLER_ID)
    }

    pub fn interests(&self) -> Vec<ReactiveInterest<Ethereum>> {
        self.registry
            .pools()
            .flat_map(|pool| event_sources_for_pool(&self.registry, pool))
            .map(|source| ReactiveInterest::Logs(log_interest(source)))
            .collect()
    }

    pub fn registry(&self) -> &AdapterRegistry {
        &self.registry
    }
}

impl ReactiveHandler<Ethereum> for AmmReactiveHandler {
    fn id(&self) -> HandlerId {
        self.id()
    }

    fn interests(&self) -> Vec<ReactiveInterest<Ethereum>> {
        self.interests()
    }

    fn handle(
        &self,
        ctx: &ReactiveContext,
        input: &ReactiveInput<Ethereum>,
        state: &dyn StateView,
    ) -> Result<HandlerOutcome, HandlerError> {
        let ReactiveInput::Log(rpc_log) = input else {
            return Ok(HandlerOutcome::empty(StateEffectQuality::NoStateEffect));
        };

        if rpc_log.removed {
            return Ok(HandlerOutcome::empty(StateEffectQuality::NoStateEffect));
        }

        let log = &rpc_log.inner;
        let Some(pool) = route_log(&self.registry, log) else {
            return Ok(HandlerOutcome::empty(StateEffectQuality::NoStateEffect));
        };
        let protocol = pool.protocol();
        let adapter = self.registry.adapter(protocol).ok_or_else(|| {
            HandlerError::new(format!("no adapter registered for protocol {protocol:?}"))
        })?;

        let result = adapter.decode_event(pool, log, state);
        if let Some(error) = result.error {
            return Err(HandlerError::new(format!(
                "adapter decode error for {protocol:?}: {error:?}"
            )));
        }

        let Some(event) = result.event else {
            return Ok(HandlerOutcome::empty(StateEffectQuality::NoStateEffect));
        };

        let predicted = predict_cold_skips(&event.updates, state);
        let predicted_verify = verify_slots_for_predicted_skips(&predicted);
        let post_apply_repair = adapter.after_apply(pool, &event, &predicted);
        let repair = combine_repair(
            combine_repair(event.repair.clone(), post_apply_repair),
            predicted_verify,
        );

        let mut effects = Vec::new();
        effects.extend(
            event
                .updates
                .iter()
                .cloned()
                .map(ReactiveEffect::StateUpdate),
        );
        effects.extend(repair_effects(
            ctx,
            &event,
            &repair,
            predicted.has_skipped(),
        ));

        let quality = quality_for_event(&event, predicted.has_skipped());
        let tags = event_labels(pool, &event, quality);
        effects.push(ReactiveEffect::Hook(hook_signal("amm.event", tags.clone())));

        Ok(HandlerOutcome {
            effects,
            quality,
            tags,
        })
    }
}

fn event_sources_for_pool(registry: &AdapterRegistry, pool: &PoolRegistration) -> Vec<EventSource> {
    let mut sources = pool.event_sources.clone();
    if let Some(adapter) = registry.adapter(pool.protocol()) {
        for source in adapter.event_sources(pool) {
            if !sources.contains(&source) {
                sources.push(source);
            }
        }
    }
    sources
}

fn log_interest(source: EventSource) -> LogInterest {
    let mut provider_filter = Filter::new().address(source.emitter);
    if !source.topics.is_empty() {
        provider_filter = provider_filter.event_signature(source.topics.clone());
    }

    LogInterest {
        provider_filter,
        local_matcher: None,
        route_key: route_key_spec(source.route),
    }
}

fn route_key_spec(route: EventRoute) -> Option<RouteKeySpec> {
    match route {
        EventRoute::Direct => Some(RouteKeySpec::EmitterAddress),
        EventRoute::IndexedAddress { topic_index } | EventRoute::IndexedBytes32 { topic_index } => {
            Some(RouteKeySpec::Topic { index: topic_index })
        }
        EventRoute::AdapterDefined => None,
    }
}

fn route_log<'a>(
    registry: &'a AdapterRegistry,
    log: &alloy_primitives::Log,
) -> Option<&'a PoolRegistration> {
    // First try the registry's own routing (stored event sources plus each
    // adapter's `route_log`). If that misses, fall back to adapter-*derived*
    // event sources that are not persisted on the pool registration.
    if let Some(pool) = registry.route_log(log) {
        return Some(pool);
    }

    registry.pools().find(|pool| {
        event_sources_for_pool(registry, pool)
            .iter()
            .any(|source| source_matches_pool(source, &pool.key, log))
    })
}

fn source_matches_pool(source: &EventSource, key: &PoolKey, log: &alloy_primitives::Log) -> bool {
    if source.emitter != log.address {
        return false;
    }

    let topics = log.topics();
    if !source.topics.is_empty()
        && !topics
            .first()
            .is_some_and(|topic0| source.topics.contains(topic0))
    {
        return false;
    }

    match source.route {
        EventRoute::Direct => true,
        EventRoute::IndexedAddress { topic_index } => topics
            .get(topic_index)
            .map(topic_address)
            .is_some_and(|address| key.address() == Some(address)),
        EventRoute::IndexedBytes32 { topic_index } => topics
            .get(topic_index)
            .is_some_and(|topic| key.bytes32() == Some(*topic)),
        EventRoute::AdapterDefined => false,
    }
}

fn topic_address(topic: &B256) -> Address {
    Address::from_slice(&topic.as_slice()[12..])
}

fn predict_cold_skips(updates: &[StateUpdate], state: &dyn StateView) -> StateDiff {
    let mut diff = StateDiff::default();

    for update in updates {
        match update {
            StateUpdate::SlotDelta {
                address,
                slot,
                delta,
            } if state.storage(*address, *slot).is_none() => {
                diff.skipped.push(SkippedDelta {
                    address: *address,
                    slot: *slot,
                    delta: *delta,
                });
            }
            StateUpdate::SlotMasked {
                address,
                slot,
                mask,
                value,
            } if state.storage(*address, *slot).is_none() => {
                diff.skipped_masks.push(SkippedMask {
                    address: *address,
                    slot: *slot,
                    mask: *mask,
                    value: *value,
                });
            }
            _ => {}
        }
    }

    diff
}

fn verify_slots_for_predicted_skips(diff: &StateDiff) -> RepairAction {
    let mut slots = Vec::new();
    for skipped in &diff.skipped {
        slots.push((skipped.address, skipped.slot));
    }
    for skipped in &diff.skipped_masks {
        slots.push((skipped.address, skipped.slot));
    }

    if slots.is_empty() {
        RepairAction::None
    } else {
        RepairAction::VerifySlots(slots)
    }
}

fn quality_for_event(event: &AdapterEvent, has_predicted_skips: bool) -> StateEffectQuality {
    match event.quality {
        UpdateQuality::Exact => StateEffectQuality::ExactFromInput,
        UpdateQuality::ExactIfApplied if has_predicted_skips => {
            StateEffectQuality::AppliedWithPendingResync
        }
        UpdateQuality::ExactIfApplied => StateEffectQuality::ExactFromInput,
        UpdateQuality::RequiresRepair | UpdateQuality::ConservativeInvalidation => {
            StateEffectQuality::RequiresRepair
        }
        UpdateQuality::Ignored => StateEffectQuality::NoStateEffect,
    }
}

fn repair_effects(
    ctx: &ReactiveContext,
    event: &AdapterEvent,
    repair: &RepairAction,
    skipped_state_effect: bool,
) -> Vec<ReactiveEffect> {
    match repair {
        RepairAction::None => Vec::new(),
        RepairAction::VerifySlots(slots) => verify_slot_resyncs(
            ctx,
            event,
            slots,
            if skipped_state_effect {
                ResyncReason::SkippedStateEffect
            } else {
                ResyncReason::HandlerRequested
            },
        ),
        RepairAction::PurgeStorage(address) => {
            vec![ReactiveEffect::Invalidate(InvalidationRequest {
                scope: PurgeScope::AllStorage,
                address: *address,
                reason: InvalidationReason::HandlerRequested,
            })]
        }
        RepairAction::PurgeSlots { address, slots } => {
            vec![ReactiveEffect::Invalidate(InvalidationRequest {
                scope: PurgeScope::Slots(slots.clone()),
                address: *address,
                reason: InvalidationReason::HandlerRequested,
            })]
        }
        RepairAction::ColdStart { pool, policy } => {
            let mut labels = repair_labels(event);
            labels.push(ReportTag::new("pool", format!("{pool:?}")));
            labels.push(ReportTag::new("policy", format!("{policy:?}")));
            vec![ReactiveEffect::Hook(hook_signal(
                "amm.repair.cold_start",
                labels,
            ))]
        }
        RepairAction::V3TickRange {
            pool,
            tick_lower,
            tick_upper,
        } => {
            let mut labels = repair_labels(event);
            labels.push(ReportTag::new("pool", format!("{pool:?}")));
            labels.push(ReportTag::new("tick_lower", tick_lower.to_string()));
            labels.push(ReportTag::new("tick_upper", tick_upper.to_string()));
            vec![ReactiveEffect::Hook(hook_signal(
                "amm.repair.v3_tick_range",
                labels,
            ))]
        }
        RepairAction::V3Incremental { pool } => {
            let mut labels = repair_labels(event);
            labels.push(ReportTag::new("pool", format!("{pool:?}")));
            vec![ReactiveEffect::Hook(hook_signal(
                "amm.repair.v3_incremental",
                labels,
            ))]
        }
        RepairAction::V3Full { pool } => {
            let mut labels = repair_labels(event);
            labels.push(ReportTag::new("pool", format!("{pool:?}")));
            vec![ReactiveEffect::Hook(hook_signal(
                "amm.repair.v3_full",
                labels,
            ))]
        }
    }
}

fn verify_slot_resyncs(
    ctx: &ReactiveContext,
    event: &AdapterEvent,
    slots: &[(Address, U256)],
    reason: ResyncReason,
) -> Vec<ReactiveEffect> {
    let mut grouped: BTreeMap<Address, Vec<U256>> = BTreeMap::new();
    for (address, slot) in slots {
        let entry = grouped.entry(*address).or_default();
        if !entry.contains(slot) {
            entry.push(*slot);
        }
    }

    let block = resync_block(ctx);
    grouped
        .into_iter()
        .map(|(address, mut slots)| {
            slots.sort_unstable();
            ReactiveEffect::Resync(ResyncRequest {
                id: ResyncId::new(resync_id(event, address, &slots, &block)),
                reason: reason.clone(),
                block: block.clone(),
                targets: vec![ResyncTarget::StorageSlots { address, slots }],
                priority: ResyncPriority::High,
            })
        })
        .collect()
}

fn resync_block(ctx: &ReactiveContext) -> ResyncBlock {
    if let Some(block) = context_block(ctx) {
        return ResyncBlock::Hash {
            number: block.number,
            hash: block.hash,
            require_canonical: true,
        };
    }

    ResyncBlock::Latest
}

fn context_block(ctx: &ReactiveContext) -> Option<&evm_fork_cache::reactive::BlockRef> {
    ctx.block.as_ref().or(match &ctx.chain_status {
        evm_fork_cache::reactive::ChainStatus::Included { block, .. }
        | evm_fork_cache::reactive::ChainStatus::Safe { block }
        | evm_fork_cache::reactive::ChainStatus::Finalized { block } => Some(block),
        evm_fork_cache::reactive::ChainStatus::Reorged { dropped_from } => Some(dropped_from),
        evm_fork_cache::reactive::ChainStatus::Pending => None,
    })
}

fn resync_id(
    event: &AdapterEvent,
    address: Address,
    slots: &[U256],
    block: &ResyncBlock,
) -> String {
    format!(
        "evm-amm-state:{:?}:{:?}:{address:?}:{slots:?}:{block:?}",
        event.pool, event.kind
    )
}

fn event_labels(
    pool: &PoolRegistration,
    event: &AdapterEvent,
    quality: StateEffectQuality,
) -> Vec<ReportTag> {
    vec![
        ReportTag::new("protocol", format!("{:?}", pool.protocol())),
        ReportTag::new("pool", format!("{:?}", event.pool)),
        ReportTag::new("event_kind", format!("{:?}", event.kind)),
        ReportTag::new("quality", format!("{quality:?}")),
    ]
}

fn repair_labels(event: &AdapterEvent) -> Vec<ReportTag> {
    vec![
        ReportTag::new("pool", format!("{:?}", event.pool)),
        ReportTag::new("event_kind", format!("{:?}", event.kind)),
    ]
}

fn hook_signal(kind: &'static str, labels: Vec<ReportTag>) -> HookSignal {
    HookSignal {
        namespace: Cow::Borrowed(HOOK_NAMESPACE),
        kind: Cow::Borrowed(kind),
        labels,
        payload: None,
    }
}

fn combine_repair(event_repair: RepairAction, post_apply_repair: RepairAction) -> RepairAction {
    match (event_repair, post_apply_repair) {
        (RepairAction::None, repair) | (repair, RepairAction::None) => repair,
        (RepairAction::VerifySlots(mut left), RepairAction::VerifySlots(right)) => {
            for slot in right {
                if !left.contains(&slot) {
                    left.push(slot);
                }
            }
            RepairAction::VerifySlots(left)
        }
        (
            RepairAction::PurgeSlots {
                address: left_address,
                slots: mut left_slots,
            },
            RepairAction::PurgeSlots {
                address: right_address,
                slots: right_slots,
            },
        ) if left_address == right_address => {
            for slot in right_slots {
                if !left_slots.contains(&slot) {
                    left_slots.push(slot);
                }
            }
            RepairAction::PurgeSlots {
                address: left_address,
                slots: left_slots,
            }
        }
        (_, post_apply_repair) => post_apply_repair,
    }
}
