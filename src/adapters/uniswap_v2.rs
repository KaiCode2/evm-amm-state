use alloy_primitives::{Address, Log, U256};
use alloy_sol_types::{SolEvent, sol};
use evm_fork_cache::cold_start::{
    ColdStartPlan, ColdStartResults, ColdStartRunReport, ColdStartStep, SlotFetch,
};

use super::cold_start::AdapterColdStartPlanner;
use super::storage::{V2_RESERVES_SLOT, V2_TOKEN0_SLOT, V2_TOKEN1_SLOT, decode_address_slot};
use super::{
    AdapterEvent, AdapterEventError, AdapterEventKind, AdapterEventResult, AmmAdapter,
    ColdStartOutcome, ColdStartPolicy, ColdStartReport, DeferredWork, EventSource,
    PoolRegistration, PoolStatus, ProtocolId, ProtocolMetadata, RepairAction, SlotChange,
    StateDiff, StateUpdate, StateView, UniswapV2Metadata, UnsupportedReason, UpdateQuality,
};

sol! {
    event Sync(uint112 reserve0, uint112 reserve1);
}

#[derive(Clone, Debug, Default)]
pub struct UniswapV2Adapter {
    _private: (),
}

impl AmmAdapter for UniswapV2Adapter {
    fn protocol(&self) -> ProtocolId {
        ProtocolId::UniswapV2
    }

    fn event_sources(&self, pool: &PoolRegistration) -> Vec<EventSource> {
        pool.key
            .address()
            .map(|address| EventSource::direct(address, vec![Sync::SIGNATURE_HASH]))
            .into_iter()
            .collect()
    }

    fn cold_start_planner(
        &self,
        pool: &PoolRegistration,
        policy: ColdStartPolicy,
    ) -> Result<Box<dyn AdapterColdStartPlanner>, UnsupportedReason> {
        let Some(address) = pool.key.address() else {
            return Err(UnsupportedReason::Custom(
                "Uniswap V2 pool key is not address-keyed".into(),
            ));
        };
        Ok(Box::new(UniswapV2ColdStartPlanner::new(address, policy)))
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
                "malformed Uniswap V2 Sync log",
            ));
        }

        let Some(address) = pool.key.address() else {
            return AdapterEventResult::error(AdapterEventError::MalformedLog(
                "Uniswap V2 pool key is not address-keyed",
            ));
        };

        let Some(reserve0) = data_word(log, 0) else {
            return AdapterEventResult::error(AdapterEventError::MalformedLog(
                "missing Uniswap V2 reserve0",
            ));
        };
        let Some(reserve1) = data_word(log, 1) else {
            return AdapterEventResult::error(AdapterEventError::MalformedLog(
                "missing Uniswap V2 reserve1",
            ));
        };

        let value = reserve0 | (reserve1 << 112);
        let mask = low_mask(224);

        AdapterEventResult::event(AdapterEvent {
            pool: pool.key.clone(),
            emitter: log.address,
            topic0: Sync::SIGNATURE_HASH,
            kind: AdapterEventKind::Sync,
            updates: vec![StateUpdate::slot_masked(
                address,
                V2_RESERVES_SLOT,
                mask,
                value,
            )],
            quality: UpdateQuality::ExactIfApplied,
            repair: RepairAction::None,
        })
    }

    fn after_apply(
        &self,
        pool: &PoolRegistration,
        event: &AdapterEvent,
        diff: &StateDiff,
    ) -> RepairAction {
        if event.kind == AdapterEventKind::Sync
            && diff.has_skipped()
            && let Some(address) = pool.key.address()
        {
            RepairAction::VerifySlots(vec![(address, V2_RESERVES_SLOT)])
        } else {
            RepairAction::None
        }
    }
}

/// Cold-start planner for a Uniswap V2 pair: a single verify-only round.
///
/// Round 1 verifies the reserves slot (8) — always — plus the token slots (6/7)
/// under `Strict`/`Eager`. The reserves slot is mandatory: its
/// [`SlotFetch`] verdict decides ready vs. repair (a genuine on-chain zero and an
/// archive miss map to *distinct* repairs). The token addresses are decoded from
/// the warmed slots and merged into the existing metadata so any config-supplied
/// `fee_bps` (V2 has no on-chain fee) survives. Under `Lazy` the token slots are
/// recorded as deferred work rather than warmed up-front.
struct UniswapV2ColdStartPlanner {
    address: Address,
    policy: ColdStartPolicy,
    verified_slots: Vec<(Address, U256)>,
    changed_slots: Vec<SlotChange>,
    decoded_token0: Option<Address>,
    decoded_token1: Option<Address>,
    /// Set once `on_results` has classified the mandatory reserves slot.
    verdict: Option<V2Verdict>,
}

/// The classified verdict of the mandatory reserves slot.
enum V2Verdict {
    /// Reserves warmed; pool is ready.
    Ready,
    /// Reserves read a genuine on-chain zero (degenerate pool).
    DegenerateZero,
    /// Reserves could not be fetched (archive / historical miss).
    FetchFailed,
}

impl UniswapV2ColdStartPlanner {
    fn new(address: Address, policy: ColdStartPolicy) -> Self {
        Self {
            address,
            policy,
            verified_slots: Vec::new(),
            changed_slots: Vec::new(),
            decoded_token0: None,
            decoded_token1: None,
            verdict: None,
        }
    }
}

impl AdapterColdStartPlanner for UniswapV2ColdStartPlanner {
    fn initial_plan(&mut self, _state: &dyn StateView) -> ColdStartPlan {
        // The reserves slot (8) is always warmed now so subsequent reactive
        // `Sync` writes land exactly. The token slots (6/7) are warmed up-front
        // except under `Lazy`/`HotSlotsOnly`.
        let verify: Vec<(Address, U256)> = match self.policy {
            ColdStartPolicy::Strict | ColdStartPolicy::Eager => vec![
                (self.address, V2_TOKEN0_SLOT),
                (self.address, V2_TOKEN1_SLOT),
                (self.address, V2_RESERVES_SLOT),
            ],
            ColdStartPolicy::Lazy | ColdStartPolicy::HotSlotsOnly => {
                vec![(self.address, V2_RESERVES_SLOT)]
            }
        };
        self.verified_slots = verify.clone();
        ColdStartPlan {
            verify,
            ..Default::default()
        }
    }

    fn on_results(&mut self, results: &ColdStartResults, state: &dyn StateView) -> ColdStartStep {
        // Record the slots that actually changed (and were injected) this round.
        self.changed_slots.extend(results.verified.iter().cloned());

        // The reserves slot is mandatory: source the verdict from its per-slot
        // `SlotFetch` classification rather than a `cached_storage(..).is_none()`
        // proxy, so a genuine zero and an archive miss are distinguishable.
        let reserves = results
            .fetched
            .iter()
            .find(|o| o.address == self.address && o.slot == V2_RESERVES_SLOT);
        self.verdict = Some(match reserves.map(|o| &o.fetch) {
            Some(SlotFetch::Value(_)) => V2Verdict::Ready,
            Some(SlotFetch::Zero) => V2Verdict::DegenerateZero,
            // A missing outcome (only possible if the slot was never declared)
            // is treated like an unfetchable slot.
            Some(SlotFetch::FetchFailed { .. }) | Some(SlotFetch::NotAttempted) | None => {
                V2Verdict::FetchFailed
            }
        });

        // Decode the token addresses from the now-warm slots. Under `Lazy`/
        // `HotSlotsOnly` these slots were not fetched, so they stay `None`.
        self.decoded_token0 = state
            .storage(self.address, V2_TOKEN0_SLOT)
            .map(decode_address_slot);
        self.decoded_token1 = state
            .storage(self.address, V2_TOKEN1_SLOT)
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

        match self.verdict {
            Some(V2Verdict::DegenerateZero) => {
                cold_report.status = PoolStatus::Degraded;
                // Distinct from the archive-miss repair below: a genuine zero is
                // a degenerate pool, so purge the stale slot rather than re-verify.
                ColdStartOutcome::NeedsRepair(
                    cold_report,
                    RepairAction::PurgeSlots {
                        address: self.address,
                        slots: vec![V2_RESERVES_SLOT],
                    },
                )
            }
            Some(V2Verdict::FetchFailed) | None => {
                cold_report.status = PoolStatus::Degraded;
                // The archive-miss repair: re-verify the unfetchable slot.
                ColdStartOutcome::NeedsRepair(
                    cold_report,
                    RepairAction::VerifySlots(vec![(self.address, V2_RESERVES_SLOT)]),
                )
            }
            Some(V2Verdict::Ready) => {
                // Merge into existing metadata so any config-supplied `fee_bps`
                // (V2 has no on-chain fee) survives the cold-start.
                let metadata = match &pool.metadata {
                    ProtocolMetadata::UniswapV2(existing) => UniswapV2Metadata {
                        token0: self.decoded_token0,
                        token1: self.decoded_token1,
                        fee_bps: existing.fee_bps,
                    },
                    _ => UniswapV2Metadata {
                        token0: self.decoded_token0,
                        token1: self.decoded_token1,
                        fee_bps: None,
                    },
                };
                pool.metadata = ProtocolMetadata::UniswapV2(metadata);
                pool.status = PoolStatus::Ready;
                cold_report.status = PoolStatus::Ready;

                if self.policy == ColdStartPolicy::Lazy {
                    let deferred = vec![DeferredWork::VerifySlots(vec![
                        (self.address, V2_TOKEN0_SLOT),
                        (self.address, V2_TOKEN1_SLOT),
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

fn low_mask(bits: usize) -> U256 {
    (U256::from(1) << bits) - U256::from(1)
}
