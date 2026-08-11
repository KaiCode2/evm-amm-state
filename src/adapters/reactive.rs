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
    AdapterEvent, AdapterEventContext, AdapterEventError, AdapterRegistry, AmmAdapter, EventRoute,
    EventSource, PoolInstanceId, PoolKey, PoolRegistration, PoolStatus, PurgeScope, RepairAction,
    SkippedDelta, SkippedMask, SlipstreamFeeEvidenceError, SlipstreamSwapFeeEvidence, StateDiff,
    StateUpdate, StateView, UpdateQuality,
};

const HANDLER_ID: &str = "evm-amm-state.adapters";
const HOOK_NAMESPACE: &str = "evm-amm-state";
const POOL_HANDLER_NAMESPACE: &str = "evm-amm-state.pool";
const MAX_SLIPSTREAM_FEE_EVIDENCE_PER_CHAIN: usize = 2_048;

/// Result of inserting one effective Slipstream fee-evidence record.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlipstreamFeeEvidenceInsertOutcome {
    /// A new event identity was inserted.
    Inserted,
    /// Existing evidence for the same exact event identity was replaced.
    Replaced(SlipstreamSwapFeeEvidence),
    /// The per-chain bound was reached; the oldest retained event was evicted.
    InsertedAndEvicted(SlipstreamSwapFeeEvidence),
    /// The evidence did not pass its public constructor-equivalent validation.
    RejectedInvalid(SlipstreamFeeEvidenceError),
    /// A stale event was not retained because the per-chain store was full.
    RejectedStaleAtCapacity(SlipstreamSwapFeeEvidence),
}

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
    slipstream_fee_evidence:
        Arc<RwLock<BTreeMap<SlipstreamFeeEvidenceKey, SlipstreamSwapFeeEvidence>>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct SlipstreamFeeEvidenceKey {
    chain_id: u64,
    pool: Address,
    block_number: u64,
    block_hash: B256,
    parent_hash: B256,
    transaction_hash: B256,
    transaction_index: u64,
    log_index: u64,
}

impl SlipstreamFeeEvidenceKey {
    fn from_evidence(evidence: SlipstreamSwapFeeEvidence) -> Self {
        Self {
            chain_id: evidence.chain_id,
            pool: evidence.pool,
            block_number: evidence.block_number,
            block_hash: evidence.block_hash,
            parent_hash: evidence.parent_hash,
            transaction_hash: evidence.transaction_hash,
            transaction_index: evidence.transaction_index,
            log_index: evidence.log_index,
        }
    }

    fn from_context(pool: Address, context: &AdapterEventContext) -> Option<Self> {
        Some(Self {
            chain_id: context.chain_id?,
            pool,
            block_number: context.block_number?,
            block_hash: context.block_hash?,
            parent_hash: context.parent_hash?,
            transaction_hash: context.transaction_hash?,
            transaction_index: context.transaction_index?,
            log_index: context.log_index?,
        })
    }
}

impl std::fmt::Debug for AmmReactiveRoutingContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AmmReactiveRoutingContext")
            .field("registry", &self.registry())
            .field(
                "slipstream_fee_evidence_count",
                &self
                    .slipstream_fee_evidence
                    .read()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .len(),
            )
            .finish()
    }
}

impl AmmReactiveRoutingContext {
    /// Construct a routing context at `registry`.
    pub fn new(registry: Arc<AdapterRegistry>) -> Self {
        Self {
            registry: Arc::new(RwLock::new(registry)),
            slipstream_fee_evidence: Arc::new(RwLock::new(BTreeMap::new())),
        }
    }

    /// Inject effective dynamic-fee evidence for one exact Slipstream event.
    ///
    /// This is the production seam from a canonical transaction/state pipeline
    /// into provider-free adapter replay. Insertion performs no network I/O.
    pub fn inject_slipstream_fee_evidence(
        &self,
        evidence: SlipstreamSwapFeeEvidence,
    ) -> SlipstreamFeeEvidenceInsertOutcome {
        if let Err(error) = evidence.validate() {
            return SlipstreamFeeEvidenceInsertOutcome::RejectedInvalid(error);
        }
        let key = SlipstreamFeeEvidenceKey::from_evidence(evidence);
        let mut retained = self
            .slipstream_fee_evidence
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(previous) = retained.get(&key).copied() {
            retained.insert(key, evidence);
            return SlipstreamFeeEvidenceInsertOutcome::Replaced(previous);
        }
        let count = retained
            .keys()
            .filter(|candidate| candidate.chain_id == evidence.chain_id)
            .count();
        if count < MAX_SLIPSTREAM_FEE_EVIDENCE_PER_CHAIN {
            retained.insert(key, evidence);
            return SlipstreamFeeEvidenceInsertOutcome::Inserted;
        }
        let oldest = retained
            .keys()
            .filter(|candidate| candidate.chain_id == evidence.chain_id)
            .min_by_key(|candidate| {
                (
                    candidate.block_number,
                    candidate.transaction_index,
                    candidate.log_index,
                    candidate.pool,
                    candidate.block_hash,
                )
            })
            .copied()
            .expect("per-chain evidence count reached the nonzero bound");
        let order = |candidate: SlipstreamFeeEvidenceKey| {
            (
                candidate.block_number,
                candidate.transaction_index,
                candidate.log_index,
                candidate.pool,
                candidate.block_hash,
            )
        };
        if order(key) <= order(oldest) {
            return SlipstreamFeeEvidenceInsertOutcome::RejectedStaleAtCapacity(evidence);
        }
        let evicted = retained
            .remove(&oldest)
            .expect("selected evidence key is retained");
        retained.insert(key, evidence);
        SlipstreamFeeEvidenceInsertOutcome::InsertedAndEvicted(evicted)
    }

    /// Remove exact Slipstream fee evidence after it is no longer needed.
    pub fn remove_slipstream_fee_evidence(
        &self,
        evidence: SlipstreamSwapFeeEvidence,
    ) -> Option<SlipstreamSwapFeeEvidence> {
        self.slipstream_fee_evidence
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&SlipstreamFeeEvidenceKey::from_evidence(evidence))
    }

    /// Drop evidence older than `minimum_block` on `chain_id`.
    pub fn prune_slipstream_fee_evidence(&self, chain_id: u64, minimum_block: u64) -> usize {
        let mut evidence = self
            .slipstream_fee_evidence
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let before = evidence.len();
        evidence.retain(|key, _| key.chain_id != chain_id || key.block_number >= minimum_block);
        before - evidence.len()
    }

    /// At one reorged height, retain only evidence for the canonical block hash.
    pub fn retain_slipstream_fee_evidence_for_block(
        &self,
        chain_id: u64,
        block_number: u64,
        canonical_hash: B256,
    ) -> usize {
        let mut evidence = self
            .slipstream_fee_evidence
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let before = evidence.len();
        evidence.retain(|key, _| {
            key.chain_id != chain_id
                || key.block_number != block_number
                || key.block_hash == canonical_hash
        });
        before - evidence.len()
    }

    fn slipstream_fee_evidence(
        &self,
        pool: Address,
        context: &AdapterEventContext,
    ) -> Option<SlipstreamSwapFeeEvidence> {
        let key = SlipstreamFeeEvidenceKey::from_context(pool, context)?;
        self.slipstream_fee_evidence
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&key)
            .copied()
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

    pub(crate) fn update_pool_status(&self, pool: &PoolKey, status: PoolStatus) -> bool {
        let mut current = self
            .registry
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Arc::make_mut(&mut current)
            .update_pool_status(pool, status)
            .is_some()
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

        handle_routed_log(
            ctx,
            &rpc_log.inner,
            rpc_log.transaction_hash,
            state,
            pool,
            adapter.as_ref(),
            None,
            None,
        )
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
            rpc_log.transaction_hash,
            state,
            &self.pool,
            self.adapter.as_ref(),
            Some(&self.instance),
            Some(&self.routing),
        )
    }
}

fn indexed_address_topic(address: Address) -> B256 {
    let mut topic = [0_u8; 32];
    topic[12..].copy_from_slice(address.as_slice());
    B256::from(topic)
}

#[allow(clippy::too_many_arguments)]
fn handle_routed_log(
    ctx: &ReactiveContext,
    log: &alloy_primitives::Log,
    transaction_hash: Option<B256>,
    state: &dyn evm_fork_cache::StateView,
    pool: &PoolRegistration,
    adapter: &dyn AmmAdapter,
    instance: Option<&PoolInstanceId>,
    routing: Option<&AmmReactiveRoutingContext>,
) -> Result<HandlerOutcome, HandlerError> {
    // Wrap the upstream state view once; adapter code (`decode_event`,
    // `predict_cold_skips`) speaks the crate-owned `StateView`.
    let state = UpstreamStateView(state);
    let state: &dyn StateView = &state;
    let protocol = pool.protocol();

    let mut event_context = AdapterEventContext {
        chain_id: ctx.chain_id,
        block_number: ctx.block.as_ref().map(|block| block.number),
        block_hash: ctx.block.as_ref().map(|block| block.hash),
        parent_hash: ctx.block.as_ref().and_then(|block| block.parent_hash),
        block_timestamp: ctx.block.as_ref().and_then(|block| block.timestamp),
        transaction_hash,
        transaction_index: ctx.transaction_index,
        log_index: ctx.log_index,
        slipstream_fee_evidence: None,
    };
    if let (Some(routing), Some(address)) = (routing, pool.key.address())
        && let Some(evidence) = routing.slipstream_fee_evidence(address, &event_context)
    {
        event_context.slipstream_fee_evidence = Some(evidence);
    }
    let result = adapter.decode_event_with_context(pool, log, state, &event_context);
    let decode_error = result.error;
    if result.event.is_none()
        && let Some(error) = decode_error
    {
        // A malformed / undecodable log for a watched topic must NOT abort
        // the batch: other pools' events in the same `ingest_batch` still
        // need to apply. Skip this log with a `NoStateEffect` outcome and
        // surface the failure as an observability hook instead of a hard
        // `HandlerError`.
        let labels = vec![
            ReportTag::new("protocol", format!("{protocol:?}")),
            ReportTag::new("error_class", adapter_error_class(&error)),
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
    if let Some(error) = decode_error {
        let labels = vec![
            ReportTag::new("protocol", format!("{protocol:?}")),
            ReportTag::new("error_class", adapter_error_class(&error)),
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
        effects.push(ReactiveEffect::Hook(hook_signal_with_payload(
            "amm.decode_error",
            labels,
            Arc::new(signal),
        )));
    }
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

fn adapter_error_class(error: &AdapterEventError) -> &'static str {
    match error {
        AdapterEventError::MalformedLog(_) => "malformed_log",
        AdapterEventError::MissingState { .. } => "missing_state",
        AdapterEventError::Unsupported(_) => "unsupported",
        AdapterEventError::V3Transition(error) => match error {
            super::V3TransitionError::MissingContext(_) => "v3_missing_context",
            super::V3TransitionError::MissingSlipstreamFeeEvidence => {
                "v3_missing_slipstream_fee_evidence"
            }
            super::V3TransitionError::SlipstreamFeeEvidence(_) => "v3_slipstream_fee_evidence",
            super::V3TransitionError::SlipstreamFeeInferenceNoMatch => "v3_slipstream_fee_no_match",
            super::V3TransitionError::SlipstreamFeeInferenceAmbiguous { .. } => {
                "v3_slipstream_fee_ambiguous"
            }
            super::V3TransitionError::ContradictoryEvent(_) => "v3_contradictory_event",
            super::V3TransitionError::FinalStateMismatch { .. } => "v3_final_state_mismatch",
            super::V3TransitionError::Observation(_) => "v3_observation",
            super::V3TransitionError::InitializedTick { .. } => "v3_initialized_tick",
            super::V3TransitionError::Arithmetic(_) => "v3_arithmetic",
            super::V3TransitionError::WorkLimitExceeded { .. } => "v3_work_limit",
        },
        AdapterEventError::Custom(_) => "custom",
    }
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
    if matches!(
        &ctx.chain_status,
        evm_fork_cache::reactive::ChainStatus::Preconfirmed { .. }
    ) {
        return ResyncBlock::Pending;
    }
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
        evm_fork_cache::reactive::ChainStatus::Pending
        | evm_fork_cache::reactive::ChainStatus::Preconfirmed { .. }
        | _ => None,
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
    use alloy_primitives::{B256, U256, address, b256};

    use super::super::AdapterEventKind;
    use super::*;

    fn hash(value: u64) -> B256 {
        B256::from(U256::from(value).to_be_bytes::<32>())
    }

    fn base_fee_evidence(block_number: u64) -> SlipstreamSwapFeeEvidence {
        base_fee_evidence_with_identity(
            block_number,
            hash(block_number),
            hash(block_number.saturating_add(10_000)),
            hash(block_number.saturating_add(20_000)),
        )
    }

    fn base_fee_evidence_with_identity(
        block_number: u64,
        block_hash: B256,
        parent_hash: B256,
        transaction_hash: B256,
    ) -> SlipstreamSwapFeeEvidence {
        let family = super::super::SlipstreamRuntimeFamily::AerodromeBaseBifi;
        let identity = super::super::SlipstreamSnapshotIdentity::new(
            8_453,
            block_number,
            block_hash,
            parent_hash,
            1_700_000_000 + block_number,
            transaction_hash,
            0,
            0,
        )
        .expect("complete test identity");
        SlipstreamSwapFeeEvidence::new(
            family,
            8_453,
            address!("b378137c90444bbcecd44a1f766851fbf53d2a9e"),
            address!("5e7bb104d84c7cb9b682aac2f3d509f5f406809a"),
            b256!("acd6710f7037ad095b1e4d5f8ee5b2681069cb4dd316e77e4e0cb8f85716a2a1"),
            address!("ec8e5342b19977b4ef8892e02d8daecfa1315831"),
            b256!("772fb5c610b40a122036f544e5b9b5bce6becb19db9524331289d1aaed2d5888"),
            10_000,
            0,
            super::super::SlipstreamUnstakedFeeProof::unused_all_liquidity_staked(family, identity),
            block_number,
            block_hash,
            parent_hash,
            1_700_000_000 + block_number,
            transaction_hash,
            0,
            0,
        )
        .expect("reviewed Base evidence")
    }

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

    #[test]
    fn slipstream_fee_evidence_store_retains_the_newest_bounded_set() {
        let context = AmmReactiveRoutingContext::new(Arc::new(AdapterRegistry::default()));
        for block in 1..=MAX_SLIPSTREAM_FEE_EVIDENCE_PER_CHAIN as u64 {
            assert_eq!(
                context.inject_slipstream_fee_evidence(base_fee_evidence(block)),
                SlipstreamFeeEvidenceInsertOutcome::Inserted
            );
        }

        let oldest = base_fee_evidence(1);
        let stale_replacement = base_fee_evidence_with_identity(
            oldest.block_number,
            oldest.block_hash,
            oldest.parent_hash,
            hash(99_999),
        );
        assert_eq!(
            context.inject_slipstream_fee_evidence(stale_replacement),
            SlipstreamFeeEvidenceInsertOutcome::RejectedStaleAtCapacity(stale_replacement)
        );
        assert_eq!(
            context.remove_slipstream_fee_evidence(oldest),
            Some(oldest),
            "a stale injection must not displace newer retained evidence"
        );
        assert_eq!(
            context.inject_slipstream_fee_evidence(oldest),
            SlipstreamFeeEvidenceInsertOutcome::Inserted
        );
        let newest = base_fee_evidence(MAX_SLIPSTREAM_FEE_EVIDENCE_PER_CHAIN as u64 + 1);
        assert_eq!(
            context.inject_slipstream_fee_evidence(newest),
            SlipstreamFeeEvidenceInsertOutcome::InsertedAndEvicted(oldest)
        );
        assert_eq!(context.remove_slipstream_fee_evidence(oldest), None);
        assert_eq!(context.remove_slipstream_fee_evidence(newest), Some(newest));
    }

    #[test]
    fn slipstream_fee_evidence_replacement_and_reorg_pruning_are_explicit() {
        let context = AmmReactiveRoutingContext::new(Arc::new(AdapterRegistry::default()));
        let canonical = base_fee_evidence(4_000);
        assert_eq!(
            context.inject_slipstream_fee_evidence(canonical),
            SlipstreamFeeEvidenceInsertOutcome::Inserted
        );
        let replacement = canonical
            .with_effective_swap_fee(9_999)
            .expect("replacement fee remains valid");
        assert_eq!(
            context.inject_slipstream_fee_evidence(replacement),
            SlipstreamFeeEvidenceInsertOutcome::Replaced(canonical)
        );

        let reorged =
            base_fee_evidence_with_identity(4_000, hash(4_001), hash(13_999), hash(23_999));
        reorged.validate().expect("alternate lineage remains valid");
        assert_eq!(
            context.inject_slipstream_fee_evidence(reorged),
            SlipstreamFeeEvidenceInsertOutcome::Inserted
        );
        assert_eq!(
            context.retain_slipstream_fee_evidence_for_block(
                canonical.chain_id,
                canonical.block_number,
                canonical.block_hash,
            ),
            1
        );
        assert_eq!(context.remove_slipstream_fee_evidence(reorged), None);
        assert_eq!(
            context.remove_slipstream_fee_evidence(replacement),
            Some(replacement)
        );
    }
}
