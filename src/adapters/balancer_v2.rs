use alloy_primitives::Log;
use alloy_sol_types::{SolEvent, sol};

use super::{
    AdapterEvent, AdapterEventError, AdapterEventKind, AdapterEventResult, AmmAdapter, EventSource,
    PoolRegistration, ProtocolId, ProtocolMetadata, RepairAction, StateView, UpdateQuality,
};

sol! {
    event Swap(bytes32 indexed poolId, address indexed tokenIn, address indexed tokenOut, uint256 amountIn, uint256 amountOut);
}

#[derive(Clone, Debug, Default)]
pub struct BalancerV2Adapter {
    _private: (),
}

impl AmmAdapter for BalancerV2Adapter {
    fn protocol(&self) -> ProtocolId {
        ProtocolId::BalancerV2
    }

    fn event_sources(&self, pool: &PoolRegistration) -> Vec<EventSource> {
        let vault = match &pool.metadata {
            ProtocolMetadata::BalancerV2(metadata) => metadata
                .vault
                .or_else(|| pool.state_addresses.first().copied()),
            _ => pool.state_addresses.first().copied(),
        };

        vault
            .map(|vault| EventSource::indexed_bytes32(vault, vec![Swap::SIGNATURE_HASH], 1))
            .into_iter()
            .collect()
    }

    fn decode_event(
        &self,
        pool: &PoolRegistration,
        log: &Log,
        _view: &dyn StateView,
    ) -> AdapterEventResult {
        if log.topics().first() != Some(&Swap::SIGNATURE_HASH) {
            return AdapterEventResult::ignored();
        }

        if Swap::decode_log_data_validate(&log.data).is_err() {
            return AdapterEventResult::error(AdapterEventError::MalformedLog(
                "malformed Balancer V2 Swap log",
            ));
        }

        AdapterEventResult::event(AdapterEvent {
            pool: pool.key.clone(),
            emitter: log.address,
            topic0: Swap::SIGNATURE_HASH,
            kind: AdapterEventKind::Swap,
            updates: Vec::new(),
            quality: UpdateQuality::ConservativeInvalidation,
            repair: RepairAction::None,
        })
    }
}
