use super::cold_start::{
    AdapterColdStartPlanner, ColdStartPlan, ColdStartResults, ColdStartRunReport, ColdStartStep,
    SlotFetch,
};
use super::factory::{FactoryConfig, PoolFactory, SolidlyFactory};
use super::sim::{SimConfig, SimError, SwapQuote, getAmountOutCall, quote_via_call};
use super::storage::{SolidlyStorageLayout, decode_address_slot};
use super::{
    AdapterCache, AdapterEvent, AdapterEventError, AdapterEventKind, AdapterEventResult,
    AmmAdapter, ColdStartOutcome, ColdStartPolicy, ColdStartReport, DeferredWork, EventSource,
    PoolRegistration, PoolStatus, ProtocolId, ProtocolMetadata, RepairAction, SlotChange,
    SolidlyV2Metadata, StateUpdate, StateView, UnsupportedReason, UpdateQuality,
};
use alloy_primitives::{Address, Bytes, Log, U256};
use alloy_sol_types::{SolCall, SolEvent, sol};

sol! {
    // Velodrome V2 / Aerodrome pools emit reserves as two separate uint256 values
    // (unlike Uniswap V2's packed uint112,uint112).
    event Sync(uint256 reserve0, uint256 reserve1);
}

/// Adapter for Solidly V2 (Aerodrome / Velodrome V2) reserves pools.
///
/// Mirrors the Uniswap V2 adapter (reserves + `Sync`), but reserves live in TWO
/// separate `uint256` storage slots (not V2's single packed slot), so reactive
/// writes are two plain slot writes. Swap simulation calls the pool's own
/// `getAmountOut(amountIn, tokenIn)`, which applies the stable (x³y+y³x) or
/// volatile (xy=k) invariant in-EVM — no math is reimplemented here.
///
/// The storage layout is config-supplied via [`SolidlyV2Metadata::storage_layout`]
/// ([`SolidlyStorageLayout`]); slot indices are fork-specific, so there is no
/// derivable default (cold-start returns [`UnsupportedReason::MissingMetadata`]
/// without one). Validate a fork's layout with the gated RPC-parity test.
#[derive(Clone, Debug, Default)]
pub struct SolidlyV2Adapter {
    _private: (),
}

fn solidly_layout(pool: &PoolRegistration) -> Option<SolidlyStorageLayout> {
    match &pool.metadata {
        ProtocolMetadata::SolidlyV2(metadata) => metadata.storage_layout,
        _ => None,
    }
}

impl AmmAdapter for SolidlyV2Adapter {
    fn protocol(&self) -> ProtocolId {
        ProtocolId::SolidlyV2
    }

    fn event_sources(&self, pool: &PoolRegistration) -> Vec<EventSource> {
        pool.key
            .address()
            .map(|address| EventSource::direct(address, vec![Sync::SIGNATURE_HASH]))
            .into_iter()
            .collect()
    }

    fn pool_factories(&self, config: &FactoryConfig) -> Vec<Box<dyn PoolFactory>> {
        config
            .solidly
            .iter()
            .map(|solidly| {
                // The config-level `verify_derivations` is a global off-switch: a
                // config's CREATE2 cross-check runs only when both it and the
                // global flag opt in.
                let mut solidly = solidly.clone();
                solidly.verify_derivations &= config.verify_derivations;
                Box::new(SolidlyFactory::new(solidly)) as Box<dyn PoolFactory>
            })
            .collect()
    }

    fn cold_start_planner(
        &self,
        pool: &PoolRegistration,
        policy: ColdStartPolicy,
    ) -> Result<Box<dyn AdapterColdStartPlanner>, UnsupportedReason> {
        let Some(address) = pool.key.address() else {
            return Err(UnsupportedReason::Custom(
                "Solidly V2 pool key is not address-keyed".into(),
            ));
        };
        let Some(layout) = solidly_layout(pool) else {
            return Err(UnsupportedReason::MissingMetadata(
                "Solidly V2 storage layout",
            ));
        };
        // reserve0/reserve1/token0/token1 are four distinct storage variables on
        // a real pool; a colliding layout would silently corrupt the cold-start
        // verdict and token decode (and clobber one reserve write), so reject it.
        let slots = [
            layout.reserve0_slot,
            layout.reserve1_slot,
            layout.token0_slot,
            layout.token1_slot,
        ];
        for i in 0..slots.len() {
            for j in (i + 1)..slots.len() {
                if slots[i] == slots[j] {
                    return Err(UnsupportedReason::Custom(
                        "Solidly V2 storage layout slots must be pairwise distinct".into(),
                    ));
                }
            }
        }
        Ok(Box::new(SolidlyV2ColdStartPlanner::new(
            address, layout, policy,
        )))
    }

    fn decode_event(
        &self,
        pool: &PoolRegistration,
        log: &Log,
        _view: &dyn StateView,
    ) -> AdapterEventResult {
        if log.topics().first() != Some(&Sync::SIGNATURE_HASH) {
            return AdapterEventResult::ignored();
        }

        if Sync::decode_log_data_validate(&log.data).is_err() {
            return AdapterEventResult::error(AdapterEventError::MalformedLog(
                "malformed Solidly V2 Sync log",
            ));
        }

        let Some(address) = pool.key.address() else {
            return AdapterEventResult::error(AdapterEventError::MalformedLog(
                "Solidly V2 pool key is not address-keyed",
            ));
        };
        let Some(layout) = solidly_layout(pool) else {
            // A missing layout is a config issue (the pool was not cold-started),
            // not a malformed log. Skip the event rather than returning an error
            // that would fail the whole reactive batch — and without a layout
            // there are no slots to target anyway.
            return AdapterEventResult::ignored();
        };

        let Some(reserve0) = data_word(log, 0) else {
            return AdapterEventResult::error(AdapterEventError::MalformedLog(
                "missing Solidly V2 reserve0",
            ));
        };
        let Some(reserve1) = data_word(log, 1) else {
            return AdapterEventResult::error(AdapterEventError::MalformedLog(
                "missing Solidly V2 reserve1",
            ));
        };

        // Two exact full-slot writes from the event payload (no fetch) — Solidly
        // stores the reserves unpacked, one uint256 per slot.
        AdapterEventResult::event(AdapterEvent {
            pool: pool.key.clone(),
            emitter: log.address,
            topic0: Sync::SIGNATURE_HASH,
            kind: AdapterEventKind::Sync,
            updates: vec![
                StateUpdate::slot(address, layout.reserve0_slot, reserve0),
                StateUpdate::slot(address, layout.reserve1_slot, reserve1),
            ],
            quality: UpdateQuality::ExactIfApplied,
            repair: RepairAction::None,
        })
    }

    // No `after_apply` override (unlike V2): Solidly stores each reserve in its
    // own full uint256 slot, so decode_event's absolute `StateUpdate::slot`
    // writes are always exact — a full Slot write is never cold-skipped (unlike
    // V2's masked write into a packed slot), so `StateDiff::has_skipped()` can
    // never be true here and the default `RepairAction::None` is correct. No
    // cold-slot resync is ever needed.

    /// Quote via the pool's own `getAmountOut(amountIn, tokenIn)` (chain code, no
    /// reimplemented math). Beyond the warmed reserves the pool also reads its
    /// `stable` flag + `token0`/`decimals0`/`decimals1` and STATICCALLs the
    /// factory's `getFee`, so the quote is NOT reproducible from warmed reserves
    /// alone — those slots and the factory's bytecode must be reachable (lazily
    /// fetched from a live backend, or installed for offline tests). `token_out`
    /// is implied (the pool's other token), so it is not part of the call.
    fn simulate_swap(
        &self,
        pool: &PoolRegistration,
        cache: &mut dyn AdapterCache,
        token_in: Address,
        _token_out: Address,
        amount_in: U256,
        _config: &SimConfig,
    ) -> Result<SwapQuote, SimError> {
        let pool_address = pool
            .key
            .address()
            .ok_or(SimError::MissingMetadata("Solidly V2 pool address"))?;

        let calldata = Bytes::from(
            getAmountOutCall {
                amountIn: amount_in,
                tokenIn: token_in,
            }
            .abi_encode(),
        );

        let output = quote_via_call(cache, pool_address, calldata)?;
        let amount_out = getAmountOutCall::abi_decode_returns_validate(&output)
            .map_err(|_| SimError::MalformedOutput("getAmountOut return"))?;
        Ok(SwapQuote::new(amount_out))
    }
}

/// The classified verdict of the mandatory reserve slots.
#[derive(Clone, Copy)]
enum SolidlyVerdict {
    /// Both reserves warmed; pool is ready.
    Ready,
    /// A reserve read a genuine on-chain zero (degenerate pool).
    DegenerateZero,
    /// A reserve could not be fetched (archive / historical miss).
    FetchFailed,
}

/// Cold-start planner for a Solidly V2 pool: a single verify-only round.
///
/// Verifies `reserve0`/`reserve1` (always — both mandatory) plus the token slots
/// under `Strict`/`Eager`. The reserves are classified from their per-slot
/// [`SlotFetch`] verdict, so a genuine zero and an archive miss map to *distinct*
/// repairs. Token addresses are decoded from the warmed slots and merged into the
/// metadata; the config-supplied `stable`/`storage_layout` are preserved. Under
/// `Lazy` the token slots are recorded as deferred work.
struct SolidlyV2ColdStartPlanner {
    address: Address,
    layout: SolidlyStorageLayout,
    policy: ColdStartPolicy,
    verified_slots: Vec<(Address, U256)>,
    changed_slots: Vec<SlotChange>,
    decoded_token0: Option<Address>,
    decoded_token1: Option<Address>,
    verdict: Option<SolidlyVerdict>,
}

impl SolidlyV2ColdStartPlanner {
    fn new(address: Address, layout: SolidlyStorageLayout, policy: ColdStartPolicy) -> Self {
        Self {
            address,
            layout,
            policy,
            verified_slots: Vec::new(),
            changed_slots: Vec::new(),
            decoded_token0: None,
            decoded_token1: None,
            verdict: None,
        }
    }
}

/// Classify a single reserve slot from its per-slot [`SlotFetch`] outcome.
fn classify_slot(results: &ColdStartResults, address: Address, slot: U256) -> SolidlyVerdict {
    match results
        .fetched
        .iter()
        .find(|o| o.address == address && o.slot == slot)
        .map(|o| &o.fetch)
    {
        Some(SlotFetch::Value(_)) => SolidlyVerdict::Ready,
        Some(SlotFetch::Zero) => SolidlyVerdict::DegenerateZero,
        Some(SlotFetch::FetchFailed { .. }) | Some(SlotFetch::NotAttempted) | None => {
            SolidlyVerdict::FetchFailed
        }
    }
}

impl AdapterColdStartPlanner for SolidlyV2ColdStartPlanner {
    fn initial_plan(&mut self, _state: &dyn StateView) -> ColdStartPlan {
        // Both reserve slots are always warmed so subsequent reactive `Sync`
        // writes land exactly. The token slots are warmed up-front except under
        // `Lazy`/`HotSlotsOnly`.
        let verify: Vec<(Address, U256)> = match self.policy {
            ColdStartPolicy::Strict | ColdStartPolicy::Eager => vec![
                (self.address, self.layout.reserve0_slot),
                (self.address, self.layout.reserve1_slot),
                (self.address, self.layout.token0_slot),
                (self.address, self.layout.token1_slot),
            ],
            ColdStartPolicy::Lazy | ColdStartPolicy::HotSlotsOnly => vec![
                (self.address, self.layout.reserve0_slot),
                (self.address, self.layout.reserve1_slot),
            ],
        };
        self.verified_slots = verify.clone();
        ColdStartPlan {
            verify,
            ..Default::default()
        }
    }

    fn on_results(&mut self, results: &ColdStartResults, state: &dyn StateView) -> ColdStartStep {
        self.changed_slots.extend(results.verified.iter().cloned());

        // Both reserves are mandatory: combine their per-slot verdicts —
        // FetchFailed dominates (archive miss), then DegenerateZero.
        let r0 = classify_slot(results, self.address, self.layout.reserve0_slot);
        let r1 = classify_slot(results, self.address, self.layout.reserve1_slot);
        self.verdict = Some(match (r0, r1) {
            (SolidlyVerdict::FetchFailed, _) | (_, SolidlyVerdict::FetchFailed) => {
                SolidlyVerdict::FetchFailed
            }
            (SolidlyVerdict::DegenerateZero, _) | (_, SolidlyVerdict::DegenerateZero) => {
                SolidlyVerdict::DegenerateZero
            }
            (SolidlyVerdict::Ready, SolidlyVerdict::Ready) => SolidlyVerdict::Ready,
        });

        self.decoded_token0 = state
            .storage(self.address, self.layout.token0_slot)
            .map(decode_address_slot);
        self.decoded_token1 = state
            .storage(self.address, self.layout.token1_slot)
            .map(decode_address_slot);

        ColdStartStep::Done
    }

    fn finish(
        &mut self,
        pool: &mut PoolRegistration,
        _report: &ColdStartRunReport,
    ) -> ColdStartOutcome {
        let mut cold_report = ColdStartReport::new(pool.key.clone(), self.policy);
        cold_report.verified_slots = self.verified_slots.clone();
        cold_report.changed_slots = self.changed_slots.clone();

        let reserve_slots = vec![
            (self.address, self.layout.reserve0_slot),
            (self.address, self.layout.reserve1_slot),
        ];

        match self.verdict {
            Some(SolidlyVerdict::DegenerateZero) => {
                cold_report.status = PoolStatus::Degraded;
                // Distinct from the archive-miss repair: a genuine zero is a
                // degenerate pool, so purge the stale slots rather than re-verify.
                ColdStartOutcome::NeedsRepair(
                    cold_report,
                    RepairAction::PurgeSlots {
                        address: self.address,
                        slots: vec![self.layout.reserve0_slot, self.layout.reserve1_slot],
                    },
                )
            }
            Some(SolidlyVerdict::FetchFailed) | None => {
                cold_report.status = PoolStatus::Degraded;
                ColdStartOutcome::NeedsRepair(cold_report, RepairAction::VerifySlots(reserve_slots))
            }
            Some(SolidlyVerdict::Ready) => {
                // Merge decoded tokens; preserve config `stable`/`storage_layout`.
                let metadata = match &pool.metadata {
                    ProtocolMetadata::SolidlyV2(existing) => SolidlyV2Metadata {
                        token0: self.decoded_token0,
                        token1: self.decoded_token1,
                        stable: existing.stable,
                        storage_layout: existing.storage_layout,
                    },
                    _ => SolidlyV2Metadata {
                        token0: self.decoded_token0,
                        token1: self.decoded_token1,
                        stable: None,
                        storage_layout: Some(self.layout),
                    },
                };
                pool.metadata = ProtocolMetadata::SolidlyV2(metadata);
                pool.status = PoolStatus::Ready;
                cold_report.status = PoolStatus::Ready;

                if self.policy == ColdStartPolicy::Lazy {
                    let deferred = vec![DeferredWork::VerifySlots(vec![
                        (self.address, self.layout.token0_slot),
                        (self.address, self.layout.token1_slot),
                    ])];
                    cold_report.deferred = deferred.clone();
                    ColdStartOutcome::ReadyWithDeferred(cold_report, deferred)
                } else {
                    ColdStartOutcome::Ready(cold_report)
                }
            }
        }
    }
}

fn data_word(log: &Log, index: usize) -> Option<U256> {
    let start = index.checked_mul(32)?;
    log.data
        .data
        .get(start..start + 32)
        .map(U256::from_be_slice)
}
