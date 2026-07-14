use std::borrow::Cow;
use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use alloy_network::Ethereum;
use alloy_primitives::{Address, B256, U256};
use alloy_rpc_types_eth::Filter;
use evm_fork_cache::reactive::{
    HandlerError, HandlerId, HandlerOutcome, HookSignal, InvalidationReason, InvalidationRequest,
    LogInterest, LogMatcher, LogRouteIndex, LogRouteKey, ReactiveContext, ReactiveEffect,
    ReactiveHandler, ReactiveInput, ReactiveInterest, ReportTag, ResyncBlock, ResyncId,
    ResyncPriority, ResyncReason, ResyncRequest, ResyncTarget, RouteKeySpec, StateEffectQuality,
};

use super::state::UpstreamStateView;
use super::{
    AdapterEvent, AdapterEventError, AdapterRegistry, AmmAdapter, EventRoute, EventSource,
    PoolInstanceId, PoolKey, PoolRegistration, PurgeScope, RepairAction, SkippedDelta, SkippedMask,
    StateDiff, StateUpdate, StateView, UpdateQuality,
};

const HANDLER_ID: &str = "evm-amm-state.adapters";
const HOOK_NAMESPACE: &str = "evm-amm-state";
const POOL_HANDLER_NAMESPACE: &str = "evm-amm-state.pool";

/// Typed in-process payload carried by AMM reactive hook signals.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum AmmReactiveSignal {
    /// A routed event decoded successfully.
    Event(AdapterEvent),
    /// A pool-scoped handler decoded an event for one exact pool generation.
    PoolEvent {
        /// Generation-scoped pool that owned the handler invocation.
        instance: PoolInstanceId,
        /// Decoded adapter event.
        event: AdapterEvent,
    },
    /// A watched/routed event could not be decoded safely.
    DecodeError {
        /// Pool the log routed to.
        pool: PoolKey,
        /// Structured adapter decode failure.
        error: AdapterEventError,
    },
    /// A pool-scoped handler could not decode an event safely.
    PoolDecodeError {
        /// Generation-scoped pool that owned the handler invocation.
        instance: PoolInstanceId,
        /// Structured adapter decode failure.
        error: AdapterEventError,
    },
    /// A pool-scoped handler emitted follow-up work for one exact generation.
    PoolRepair {
        /// Generation-scoped pool that owns the repair work.
        instance: PoolInstanceId,
        /// Typed repair action for schedulers and observers.
        action: RepairAction,
    },
}

/// Error constructing a pool-scoped reactive handler.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum AmmPoolReactiveHandlerError {
    /// The requested pool is not present in the supplied registry snapshot.
    UnknownPool(PoolKey),
    /// The pool has no registered protocol adapter.
    MissingAdapter(PoolKey),
}

impl std::fmt::Display for AmmPoolReactiveHandlerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownPool(pool) => write!(f, "unknown pool for reactive handler: {pool:?}"),
            Self::MissingAdapter(pool) => {
                write!(f, "no adapter registered for reactive pool: {pool:?}")
            }
        }
    }
}

impl std::error::Error for AmmPoolReactiveHandlerError {}

/// Shared copy-on-write registry view used only for adapter-defined routing.
///
/// Generic direct/indexed pool matchers are fully self-contained and do not
/// take this lock. Lifecycle code mutates the copy-on-write view between
/// batches so existing third-party adapter-defined handlers observe the current
/// pool universe without reconstructing the runtime; full replacement remains
/// available as a compatibility operation.
#[derive(Clone)]
pub struct AmmReactiveRoutingContext {
    registry: Arc<RwLock<Arc<AdapterRegistry>>>,
}

impl std::fmt::Debug for AmmReactiveRoutingContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AmmReactiveRoutingContext")
            .field("registry", &self.registry())
            .finish()
    }
}

impl AmmReactiveRoutingContext {
    /// Construct a routing context at `registry`.
    pub fn new(registry: Arc<AdapterRegistry>) -> Self {
        Self {
            registry: Arc::new(RwLock::new(registry)),
        }
    }

    /// Return the current immutable registry snapshot.
    pub fn registry(&self) -> Arc<AdapterRegistry> {
        self.registry
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Atomically replace the registry snapshot used by adapter-defined routes.
    pub fn replace_registry(&self, registry: Arc<AdapterRegistry>) -> Arc<AdapterRegistry> {
        let mut current = self
            .registry
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        std::mem::replace(&mut *current, registry)
    }

    pub(crate) fn register_pool(&self, pool: PoolRegistration) -> Result<(), super::RegistryError> {
        let mut current = self
            .registry
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Arc::make_mut(&mut current).register_pool(pool)
    }

    pub(crate) fn unregister_pool(&self, pool: &PoolKey) -> Option<PoolRegistration> {
        let mut current = self
            .registry
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Arc::make_mut(&mut current).unregister_pool(pool)
    }

    pub(crate) fn update_pool(&self, pool: PoolRegistration) -> bool {
        let mut current = self
            .registry
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let registry = Arc::make_mut(&mut current);
        let Some(existing) = registry.pool_mut(&pool.key) else {
            return false;
        };
        *existing = pool;
        true
    }

    pub(crate) fn register_adapter(
        &self,
        adapter: Arc<dyn AmmAdapter>,
    ) -> Result<(), super::RegistryError> {
        let mut current = self
            .registry
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Arc::make_mut(&mut current).register_adapter(adapter)
    }

    pub(crate) fn unregister_adapter(
        &self,
        protocol: super::ProtocolId,
    ) -> Result<Option<Arc<dyn AmmAdapter>>, super::RegistryError> {
        let mut current = self
            .registry
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Arc::make_mut(&mut current).unregister_adapter(protocol)
    }

    pub(crate) fn unregister_adapter_prevalidated(
        &self,
        protocol: super::ProtocolId,
    ) -> Option<Arc<dyn AmmAdapter>> {
        let mut current = self
            .registry
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Arc::make_mut(&mut current).unregister_adapter_prevalidated(protocol)
    }
}

/// Reactive handler scoped to one concrete logical pool registration.
///
/// The handler owns only that pool's interests and verifies shared-emitter
/// routing locally before decode. A shared updateable registry view is retained
/// solely for third-party [`EventRoute::AdapterDefined`] routing, whose
/// compatibility trait receives the current registry.
#[derive(Clone)]
pub struct AmmPoolReactiveHandler {
    id: HandlerId,
    instance: PoolInstanceId,
    routing: AmmReactiveRoutingContext,
    pool: PoolRegistration,
    adapter: Arc<dyn AmmAdapter>,
    sources: Vec<EventSource>,
}

impl std::fmt::Debug for AmmPoolReactiveHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AmmPoolReactiveHandler")
            .field("id", &self.id)
            .field("instance", &self.instance)
            .field("sources", &self.sources)
            .finish_non_exhaustive()
    }
}

impl AmmPoolReactiveHandler {
    /// Construct a handler for `pool` with a private routing context initialized
    /// from one immutable registry snapshot.
    pub fn new(
        registry: Arc<AdapterRegistry>,
        instance: PoolInstanceId,
    ) -> Result<Self, AmmPoolReactiveHandlerError> {
        Self::with_routing_context(AmmReactiveRoutingContext::new(registry), instance)
    }

    /// Construct a handler sharing an updateable adapter-defined routing view.
    pub fn with_routing_context(
        routing: AmmReactiveRoutingContext,
        instance: PoolInstanceId,
    ) -> Result<Self, AmmPoolReactiveHandlerError> {
        let registry = routing.registry();
        let pool = instance.key().clone();
        let registration = registry
            .pool(&pool)
            .cloned()
            .ok_or_else(|| AmmPoolReactiveHandlerError::UnknownPool(pool.clone()))?;
        let adapter = registry
            .adapter(registration.protocol())
            .cloned()
            .ok_or_else(|| AmmPoolReactiveHandlerError::MissingAdapter(pool.clone()))?;
        let sources = registry.event_sources_for(&registration);
        Ok(Self {
            id: Self::handler_id(&instance),
            instance,
            routing,
            pool: registration,
            adapter,
            sources,
        })
    }

    pub(crate) fn from_registration(
        routing: AmmReactiveRoutingContext,
        instance: PoolInstanceId,
        pool: PoolRegistration,
        adapter: Arc<dyn AmmAdapter>,
        sources: Vec<EventSource>,
    ) -> Self {
        Self {
            id: Self::handler_id(&instance),
            instance,
            routing,
            pool,
            adapter,
            sources,
        }
    }

    /// Stable handler id for one generation-scoped pool instance.
    pub fn handler_id(instance: &PoolInstanceId) -> HandlerId {
        HandlerId::new(format!(
            "{POOL_HANDLER_NAMESPACE}.{:?}.{}",
            instance.key(),
            instance.generation().get()
        ))
    }

    /// Generation-scoped pool instance owned by this handler.
    pub const fn instance(&self) -> &PoolInstanceId {
        &self.instance
    }

    /// Logical pool owned by this handler.
    pub const fn pool(&self) -> &PoolRegistration {
        &self.pool
    }

    /// This pool handler's exact log interests.
    pub fn interests(&self) -> Vec<ReactiveInterest<Ethereum>> {
        self.sources
            .iter()
            .cloned()
            .map(|source| {
                ReactiveInterest::Logs(pool_log_interest(
                    source,
                    self.pool.key.clone(),
                    self.adapter.clone(),
                    self.routing.clone(),
                ))
            })
            .collect()
    }
}

/// Reactive-runtime bridge for the AMM adapter registry.
#[derive(Clone, Debug)]
pub struct AmmReactiveHandler {
    registry: AdapterRegistry,
}

impl AmmReactiveHandler {
    /// Wrap an [`AdapterRegistry`] as a reactive handler.
    pub fn new(registry: AdapterRegistry) -> Self {
        Self { registry }
    }

    /// This handler's stable id in the reactive runtime.
    pub fn id(&self) -> HandlerId {
        HandlerId::new(HANDLER_ID)
    }

    /// The log interests (emitter/topic filters) for every tracked pool.
    pub fn interests(&self) -> Vec<ReactiveInterest<Ethereum>> {
        self.registry
            .pools()
            .flat_map(|pool| self.registry.event_sources_for(pool))
            .map(|source| ReactiveInterest::Logs(log_interest(source)))
            .collect()
    }

    /// The wrapped registry.
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
        state: &dyn evm_fork_cache::StateView,
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

        handle_routed_log(ctx, &rpc_log.inner, state, pool, adapter.as_ref(), None)
    }
}

impl ReactiveHandler<Ethereum> for AmmPoolReactiveHandler {
    fn id(&self) -> HandlerId {
        self.id.clone()
    }

    fn interests(&self) -> Vec<ReactiveInterest<Ethereum>> {
        self.interests()
    }

    fn log_route_index(&self) -> Option<LogRouteIndex> {
        let mut keys = Vec::new();
        for source in &self.sources {
            let key = match source.route {
                EventRoute::Direct => LogRouteKey::Emitter(source.emitter),
                EventRoute::IndexedAddress { topic_index } => LogRouteKey::Topic {
                    index: topic_index,
                    value: indexed_address_topic(self.pool.key.address()?),
                },
                EventRoute::IndexedBytes32 { topic_index } => LogRouteKey::Topic {
                    index: topic_index,
                    value: self.pool.key.bytes32()?,
                },
                // The adapter trait does not yet require an exhaustive exact
                // key declaration. Keep third-party routing on the compatible
                // fallback path rather than risking a false negative.
                EventRoute::AdapterDefined => return None,
            };
            if !keys.contains(&key) {
                keys.push(key);
            }
        }
        let (primary, additional) = keys.split_first()?;
        Some(LogRouteIndex::new(
            primary.clone(),
            additional.iter().cloned(),
        ))
    }

    fn handle(
        &self,
        ctx: &ReactiveContext,
        input: &ReactiveInput<Ethereum>,
        state: &dyn evm_fork_cache::StateView,
    ) -> Result<HandlerOutcome, HandlerError> {
        let ReactiveInput::Log(rpc_log) = input else {
            return Ok(HandlerOutcome::empty(StateEffectQuality::NoStateEffect));
        };
        if rpc_log.removed
            || !self.sources.iter().any(|source| {
                pool_source_matches(
                    source,
                    &self.pool.key,
                    self.adapter.as_ref(),
                    &self.routing,
                    &rpc_log.inner,
                )
            })
        {
            return Ok(HandlerOutcome::empty(StateEffectQuality::NoStateEffect));
        }

        handle_routed_log(
            ctx,
            &rpc_log.inner,
            state,
            &self.pool,
            self.adapter.as_ref(),
            Some(&self.instance),
        )
    }
}

fn indexed_address_topic(address: Address) -> B256 {
    let mut topic = [0_u8; 32];
    topic[12..].copy_from_slice(address.as_slice());
    B256::from(topic)
}

fn handle_routed_log(
    ctx: &ReactiveContext,
    log: &alloy_primitives::Log,
    state: &dyn evm_fork_cache::StateView,
    pool: &PoolRegistration,
    adapter: &dyn AmmAdapter,
    instance: Option<&PoolInstanceId>,
) -> Result<HandlerOutcome, HandlerError> {
    // Wrap the upstream state view once; adapter code (`decode_event`,
    // `predict_cold_skips`) speaks the crate-owned `StateView`.
    let state = UpstreamStateView(state);
    let state: &dyn StateView = &state;
    let protocol = pool.protocol();

    let result = adapter.decode_event(pool, log, state);
    if let Some(error) = result.error {
        // A malformed / undecodable log for a watched topic must NOT abort
        // the batch: other pools' events in the same `ingest_batch` still
        // need to apply. Skip this log with a `NoStateEffect` outcome and
        // surface the failure as an observability hook instead of a hard
        // `HandlerError`.
        let labels = vec![
            ReportTag::new("protocol", format!("{protocol:?}")),
            ReportTag::new("emitter", format!("{:?}", log.address)),
            ReportTag::new("error", format!("{error:?}")),
        ];
        let signal = match instance {
            Some(instance) => AmmReactiveSignal::PoolDecodeError {
                instance: instance.clone(),
                error,
            },
            None => AmmReactiveSignal::DecodeError {
                pool: pool.key.clone(),
                error,
            },
        };
        return Ok(HandlerOutcome {
            effects: vec![ReactiveEffect::Hook(hook_signal_with_payload(
                "amm.decode_error",
                labels.clone(),
                Arc::new(signal),
            ))],
            quality: StateEffectQuality::NoStateEffect,
            tags: labels,
        });
    }

    let Some(event) = result.event else {
        return Ok(HandlerOutcome::empty(StateEffectQuality::NoStateEffect));
    };

    let predicted = predict_cold_skips(&event.updates, state);
    let predicted_verify = verify_slots_for_predicted_skips(&predicted);
    let post_apply_repair = adapter.after_apply(pool, &event, &predicted);
    let repair = event
        .repair
        .clone()
        .combine(post_apply_repair)
        .combine(predicted_verify);

    let mut effects = Vec::new();
    effects.extend(
        event
            .updates
            .iter()
            .cloned()
            .map(|update| ReactiveEffect::StateUpdate(update.into())),
    );
    effects.extend(repair_effects(
        ctx,
        pool,
        &event,
        &repair,
        predicted.has_skipped(),
        instance,
    ));
    let quality = quality_for_event(&event, predicted.has_skipped());
    let tags = event_labels(pool, &event, quality);
    let required_repair = (!matches!(repair, RepairAction::None)).then(|| {
        repair_hook_signal(
            "amm.repair.required",
            repair_labels(&event),
            instance,
            &repair,
        )
    });
    let signal = match instance {
        Some(instance) => AmmReactiveSignal::PoolEvent {
            instance: instance.clone(),
            event,
        },
        None => AmmReactiveSignal::Event(event),
    };
    effects.push(ReactiveEffect::Hook(hook_signal_with_payload(
        "amm.event",
        tags.clone(),
        Arc::new(signal),
    )));
    if let Some(required_repair) = required_repair {
        effects.push(ReactiveEffect::Hook(required_repair));
    }

    Ok(HandlerOutcome {
        effects,
        quality,
        tags,
    })
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

#[cfg(feature = "live-runtime")]
pub(crate) fn event_source_interest(source: EventSource) -> ReactiveInterest<Ethereum> {
    ReactiveInterest::Logs(log_interest(source))
}

fn pool_log_interest(
    source: EventSource,
    pool: PoolKey,
    adapter: Arc<dyn AmmAdapter>,
    routing: AmmReactiveRoutingContext,
) -> LogInterest {
    let mut interest = log_interest(source.clone());
    interest.local_matcher = Some(Arc::new(PoolLogMatcher {
        source,
        pool,
        adapter,
        routing,
    }));
    interest
}

struct PoolLogMatcher {
    source: EventSource,
    pool: PoolKey,
    adapter: Arc<dyn AmmAdapter>,
    routing: AmmReactiveRoutingContext,
}

impl LogMatcher for PoolLogMatcher {
    fn matches(&self, log: &alloy_rpc_types_eth::Log) -> bool {
        pool_source_matches(
            &self.source,
            &self.pool,
            self.adapter.as_ref(),
            &self.routing,
            &log.inner,
        )
    }
}

fn pool_source_matches(
    source: &EventSource,
    pool: &PoolKey,
    adapter: &dyn AmmAdapter,
    routing: &AmmReactiveRoutingContext,
    log: &alloy_primitives::Log,
) -> bool {
    if source.emitter != log.address {
        return false;
    }
    if !source.topics.is_empty()
        && !log
            .topics()
            .first()
            .is_some_and(|topic| source.topics.contains(topic))
    {
        return false;
    }

    match source.route {
        EventRoute::AdapterDefined => {
            adapter.route_log(log, &routing.registry()).as_ref() == Some(pool)
        }
        EventRoute::Direct
        | EventRoute::IndexedAddress { .. }
        | EventRoute::IndexedBytes32 { .. } => {
            super::registry::event_source_matches(source, pool, log)
        }
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
        registry
            .event_sources_for(pool)
            .iter()
            .any(|source| super::registry::event_source_matches(source, &pool.key, log))
    })
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
    pool: &PoolRegistration,
    event: &AdapterEvent,
    repair: &RepairAction,
    skipped_state_effect: bool,
    instance: Option<&PoolInstanceId>,
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
            instance,
        ),
        RepairAction::PurgeStorage(address) => {
            vec![ReactiveEffect::Invalidate(InvalidationRequest {
                scope: PurgeScope::AllStorage.into(),
                address: *address,
                reason: InvalidationReason::HandlerRequested,
            })]
        }
        RepairAction::PurgeSlots { address, slots } => {
            vec![ReactiveEffect::Invalidate(InvalidationRequest {
                scope: PurgeScope::Slots(slots.clone()).into(),
                address: *address,
                reason: InvalidationReason::HandlerRequested,
            })]
        }
        RepairAction::ColdStart { pool, policy } => {
            let mut labels = repair_labels(event);
            labels.push(ReportTag::new("pool", format!("{pool:?}")));
            labels.push(ReportTag::new("policy", format!("{policy:?}")));
            vec![ReactiveEffect::Hook(repair_hook_signal(
                "amm.repair.cold_start",
                labels,
                instance,
                repair,
            ))]
        }
        RepairAction::V3TickRange {
            pool: pool_key,
            tick_lower,
            tick_upper,
        } => {
            // Lower the repair intention into an executable, hash-pinned resync
            // (or a conservative invalidation when the layout is missing)...
            let mut effects = super::repair::v3_tick_range_effects(
                pool,
                event,
                *tick_lower,
                *tick_upper,
                ctx,
                instance,
            );
            // ...then preserve the A1 observability hook alongside it.
            let mut labels = repair_labels(event);
            labels.push(ReportTag::new("pool", format!("{pool_key:?}")));
            labels.push(ReportTag::new("tick_lower", tick_lower.to_string()));
            labels.push(ReportTag::new("tick_upper", tick_upper.to_string()));
            effects.push(ReactiveEffect::Hook(repair_hook_signal(
                "amm.repair.v3_tick_range",
                labels,
                instance,
                repair,
            )));
            effects
        }
        RepairAction::V3Incremental { pool } => {
            let mut labels = repair_labels(event);
            labels.push(ReportTag::new("pool", format!("{pool:?}")));
            vec![ReactiveEffect::Hook(repair_hook_signal(
                "amm.repair.v3_incremental",
                labels,
                instance,
                repair,
            ))]
        }
        RepairAction::V3Full { pool } => {
            let mut labels = repair_labels(event);
            labels.push(ReportTag::new("pool", format!("{pool:?}")));
            vec![ReactiveEffect::Hook(repair_hook_signal(
                "amm.repair.v3_full",
                labels,
                instance,
                repair,
            ))]
        }
    }
}

fn verify_slot_resyncs(
    ctx: &ReactiveContext,
    event: &AdapterEvent,
    slots: &[(Address, U256)],
    reason: ResyncReason,
    instance: Option<&PoolInstanceId>,
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
                id: ResyncId::new(resync_id(instance, event, address, &slots, &block, ctx)),
                reason: reason.clone(),
                block: block.clone(),
                targets: vec![ResyncTarget::StorageSlots { address, slots }],
                priority: ResyncPriority::High,
            })
        })
        .collect()
}

pub(crate) fn resync_block(ctx: &ReactiveContext) -> ResyncBlock {
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

pub(crate) fn resync_id(
    instance: Option<&PoolInstanceId>,
    event: &AdapterEvent,
    address: Address,
    slots: &[U256],
    block: &ResyncBlock,
    ctx: &ReactiveContext,
) -> String {
    match instance {
        Some(instance) => format!(
            "evm-amm-state:{instance:?}:{:?}:{:?}:{address:?}:{slots:?}:{block:?}:{:?}:{:?}",
            event.pool, event.kind, ctx.transaction_index, ctx.log_index,
        ),
        None => format!(
            "evm-amm-state:{:?}:{:?}:{address:?}:{slots:?}:{block:?}",
            event.pool, event.kind,
        ),
    }
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

fn repair_hook_signal(
    kind: &'static str,
    labels: Vec<ReportTag>,
    instance: Option<&PoolInstanceId>,
    action: &RepairAction,
) -> HookSignal {
    match instance {
        Some(instance) => hook_signal_with_payload(
            kind,
            labels,
            Arc::new(AmmReactiveSignal::PoolRepair {
                instance: instance.clone(),
                action: action.clone(),
            }),
        ),
        None => hook_signal(kind, labels),
    }
}

fn hook_signal_with_payload(
    kind: &'static str,
    labels: Vec<ReportTag>,
    payload: Arc<dyn std::any::Any + Send + Sync>,
) -> HookSignal {
    HookSignal {
        namespace: Cow::Borrowed(HOOK_NAMESPACE),
        kind: Cow::Borrowed(kind),
        labels,
        payload: Some(payload),
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::B256;

    use super::super::AdapterEventKind;
    use super::*;

    #[test]
    fn compatibility_resync_id_retains_the_pre_pool_handler_format() {
        let address = Address::repeat_byte(0x44);
        let event = AdapterEvent::new(
            PoolKey::UniswapV2(address),
            address,
            B256::repeat_byte(0x45),
            AdapterEventKind::Sync,
            UpdateQuality::RequiresRepair,
        );
        let slots = [U256::from(8)];
        let block = ResyncBlock::Number(7);
        let ctx = ReactiveContext {
            chain_id: Some(1),
            source: evm_fork_cache::reactive::InputSource::Synthetic,
            chain_status: evm_fork_cache::reactive::ChainStatus::Pending,
            block: None,
            transaction_index: Some(3),
            log_index: Some(4),
        };

        let id = resync_id(None, &event, address, &slots, &block, &ctx);
        assert_eq!(
            id,
            format!(
                "evm-amm-state:{:?}:{:?}:{address:?}:{slots:?}:{block:?}",
                event.pool, event.kind,
            )
        );
        assert!(!id.contains("None"));
    }
}
