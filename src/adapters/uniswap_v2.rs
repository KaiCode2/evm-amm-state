use alloy_primitives::{Address, Log, U256};
use alloy_sol_types::{SolEvent, sol};
use anyhow::Result;

use super::cache::AdapterCache;
use super::storage::{V2_RESERVES_SLOT, V2_TOKEN0_SLOT, V2_TOKEN1_SLOT, decode_address_slot};
use super::{
    AdapterEvent, AdapterEventError, AdapterEventKind, AdapterEventResult, AmmAdapter,
    ColdStartOutcome, ColdStartPolicy, ColdStartReport, DeferredWork, EventSource,
    PoolRegistration, PoolStatus, ProtocolId, ProtocolMetadata, RepairAction, StateDiff,
    StateUpdate, StateView, UniswapV2Metadata, UnsupportedReason, UpdateQuality,
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

    fn cold_start(
        &self,
        pool: &mut PoolRegistration,
        cache: &mut dyn AdapterCache,
        policy: ColdStartPolicy,
    ) -> Result<ColdStartOutcome> {
        let Some(address) = pool.key.address() else {
            return Ok(ColdStartOutcome::Unsupported(UnsupportedReason::Custom(
                "Uniswap V2 pool key is not address-keyed".into(),
            )));
        };

        // Slot set by policy: the token slots (6/7) are warmed up-front except
        // under `Lazy`, where they are deferred. The reserves slot (8) is always
        // warmed now so subsequent reactive `Sync` writes land exactly.
        let verified_slots: Vec<(Address, U256)> = match policy {
            ColdStartPolicy::Strict | ColdStartPolicy::Eager => vec![
                (address, V2_TOKEN0_SLOT),
                (address, V2_TOKEN1_SLOT),
                (address, V2_RESERVES_SLOT),
            ],
            ColdStartPolicy::Lazy | ColdStartPolicy::HotSlotsOnly => {
                vec![(address, V2_RESERVES_SLOT)]
            }
        };

        // Authoritative, block-pinned re-fetch through the facade; only changed
        // slots are injected. Fetch failures propagate as `Err`.
        let changed_slots = cache.verify_slots(&verified_slots)?;

        let mut report = ColdStartReport::new(pool.key.clone(), policy);
        report.verified_slots = verified_slots;
        report.changed_slots = changed_slots;

        // The reserves slot is mandatory: if it is still cold after verification
        // (unfetchable or genuinely zero), surface a repair rather than a silent
        // partial.
        if cache.cached_storage(address, V2_RESERVES_SLOT).is_none() {
            report.status = PoolStatus::Degraded;
            return Ok(ColdStartOutcome::NeedsRepair(
                report,
                RepairAction::VerifySlots(vec![(address, V2_RESERVES_SLOT)]),
            ));
        }

        // Decode the token addresses from the now-warm slots. Under `Lazy` these
        // slots were not fetched, so they remain `None`.
        let token0 = cache
            .cached_storage(address, V2_TOKEN0_SLOT)
            .map(decode_address_slot);
        let token1 = cache
            .cached_storage(address, V2_TOKEN1_SLOT)
            .map(decode_address_slot);

        // Merge into existing metadata so any config-supplied `fee_bps` (V2 has
        // no on-chain fee) survives the cold-start.
        let metadata = match &pool.metadata {
            ProtocolMetadata::UniswapV2(existing) => UniswapV2Metadata {
                token0,
                token1,
                fee_bps: existing.fee_bps,
            },
            _ => UniswapV2Metadata {
                token0,
                token1,
                fee_bps: None,
            },
        };
        pool.metadata = ProtocolMetadata::UniswapV2(metadata);
        pool.status = PoolStatus::Ready;
        report.status = PoolStatus::Ready;

        if policy == ColdStartPolicy::Lazy {
            let deferred = vec![DeferredWork::VerifySlots(vec![
                (address, V2_TOKEN0_SLOT),
                (address, V2_TOKEN1_SLOT),
            ])];
            report.deferred = deferred.clone();
            Ok(ColdStartOutcome::ReadyWithDeferred(report, deferred))
        } else {
            Ok(ColdStartOutcome::Ready(report))
        }
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
