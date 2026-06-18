use alloy_primitives::{Log, U256};
use alloy_sol_types::{SolEvent, sol};

use super::storage::V2_RESERVES_SLOT;
use super::{
    AdapterEvent, AdapterEventError, AdapterEventKind, AdapterEventResult, AmmAdapter, EventSource,
    PoolRegistration, ProtocolId, RepairAction, StateDiff, StateUpdate, StateView, UpdateQuality,
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
