use alloy_primitives::{Address, B256, Log, U256};
use alloy_sol_types::{SolEvent, sol};
use anyhow::Result;

use super::cache::AdapterCache;
use super::{
    AdapterEvent, AdapterEventError, AdapterEventKind, AdapterEventResult, AmmAdapter,
    ColdStartOutcome, ColdStartPolicy, ColdStartReport, DeferredWork, EventSource,
    PoolRegistration, PoolStatus, ProtocolId, ProtocolMetadata, RepairAction, StateDiff,
    StateUpdate, StateView, UnsupportedReason, UpdateQuality, V3Metadata,
};
use crate::adapters::storage::{
    V3StorageLayout, v3_tick_bitmap_storage_key_with_base, v3_tick_info_storage_keys_with_base,
    v3_word_position,
};

sol! {
    event Swap(address indexed sender, address indexed recipient, int256 amount0, int256 amount1, uint160 sqrtPriceX96, uint128 liquidity, int24 tick);
    event Mint(address sender, address indexed owner, int24 indexed tickLower, int24 indexed tickUpper, uint128 amount, uint256 amount0, uint256 amount1);
    event Burn(address indexed owner, int24 indexed tickLower, int24 indexed tickUpper, uint128 amount, uint256 amount0, uint256 amount1);
}

const SLOT0_PRICE_TICK_BITS: usize = 184;
const SLOT0_TICK_SHIFT: usize = 160;

#[derive(Clone, Debug, Default)]
pub struct UniswapV3Adapter {
    _private: (),
}

impl AmmAdapter for UniswapV3Adapter {
    fn protocol(&self) -> ProtocolId {
        ProtocolId::UniswapV3
    }

    fn event_sources(&self, pool: &PoolRegistration) -> Vec<EventSource> {
        pool.key
            .address()
            .map(|address| {
                EventSource::direct(
                    address,
                    vec![
                        Swap::SIGNATURE_HASH,
                        Mint::SIGNATURE_HASH,
                        Burn::SIGNATURE_HASH,
                    ],
                )
            })
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
                "Uniswap V3 pool key is not address-keyed".into(),
            )));
        };

        // Resolve the storage layout before any fetch: the missing-layout case
        // must surface as `Unsupported` even when no batch fetcher is
        // configured, so it has to short-circuit ahead of `verify_slots`.
        let Some(layout) = layout_for(pool) else {
            return Ok(ColdStartOutcome::Unsupported(
                UnsupportedReason::MissingMetadata("V3 storage layout"),
            ));
        };

        // Accumulators for the report; populated as each verification round runs.
        let mut verified_slots: Vec<(Address, U256)> = Vec::new();
        let mut changed_slots = Vec::new();

        // Round 1: slot0 + global liquidity. slot0 is mandatory (it carries the
        // current tick that drives the bounded tick warm-up); liquidity is an
        // absolute write that the reactive Swap always applies.
        let round1 = [
            (address, layout.slot0_slot),
            (address, layout.liquidity_slot),
        ];
        verified_slots.extend_from_slice(&round1);
        changed_slots.extend(cache.verify_slots(&round1)?);

        let mut report = ColdStartReport::new(pool.key.clone(), policy);

        // slot0 is mandatory: if it is still cold after verification
        // (unfetchable or genuinely zero), surface a repair rather than a silent
        // partial.
        let Some(slot0) = cache.cached_storage(address, layout.slot0_slot) else {
            report.verified_slots = verified_slots;
            report.changed_slots = changed_slots;
            report.status = PoolStatus::Degraded;
            return Ok(ColdStartOutcome::NeedsRepair(
                report,
                RepairAction::VerifySlots(vec![(address, layout.slot0_slot)]),
            ));
        };

        // Decode the current tick from the warm slot0 word (bits [160, 184),
        // 24-bit signed), reusing the same decode as the reactive Swap path.
        let tick = int24_from_word(slot0 >> SLOT0_TICK_SHIFT);
        let word = v3_word_position(tick, layout.tick_spacing);
        let bitmap_key = v3_tick_bitmap_storage_key_with_base(word, layout.tick_bitmap_base_slot);

        let mut deferred: Vec<DeferredWork> = Vec::new();

        match policy {
            ColdStartPolicy::Strict | ColdStartPolicy::Eager => {
                // Round 2: warm the bitmap word containing the current tick.
                let round2 = [(address, bitmap_key)];
                verified_slots.extend_from_slice(&round2);
                changed_slots.extend(cache.verify_slots(&round2)?);

                // Round 3: warm the {0, 3} info slots of every tick initialized
                // in that word. The bitmap word is extracted adapter-locally
                // (no `cache_sync` dependency): bit `i` set => tick
                // `(word * 256 + i) * tick_spacing`.
                let bitmap = cache
                    .cached_storage(address, bitmap_key)
                    .unwrap_or(U256::ZERO);
                let mut tick_slots: Vec<(Address, U256)> = Vec::new();
                for bit in 0..256u32 {
                    if (bitmap >> bit) & U256::from(1) == U256::from(1) {
                        let tick_i = (word as i32 * 256 + bit as i32) * layout.tick_spacing;
                        let keys =
                            v3_tick_info_storage_keys_with_base(tick_i, layout.ticks_base_slot);
                        tick_slots.push((address, keys[0]));
                        tick_slots.push((address, keys[3]));
                    }
                }
                if !tick_slots.is_empty() {
                    verified_slots.extend_from_slice(&tick_slots);
                    changed_slots.extend(cache.verify_slots(&tick_slots)?);
                }
            }
            ColdStartPolicy::HotSlotsOnly => {
                // slot0 + liquidity only; the tick word is left to the reactive
                // path / lazy RPC.
            }
            ColdStartPolicy::Lazy => {
                // Warm the hot slots now; defer the tick word.
                deferred.push(DeferredWork::VerifySlots(vec![(address, bitmap_key)]));
            }
        }

        // Preserve the config-supplied V3 metadata (token0/token1/fee/
        // tick_spacing/layout are not at predictable storage slots and are not
        // re-fetched here).
        pool.status = PoolStatus::Ready;

        report.verified_slots = verified_slots;
        report.changed_slots = changed_slots;
        report.status = PoolStatus::Ready;

        if deferred.is_empty() {
            Ok(ColdStartOutcome::Ready(report))
        } else {
            report.deferred = deferred.clone();
            Ok(ColdStartOutcome::ReadyWithDeferred(report, deferred))
        }
    }

    fn decode_event(
        &self,
        pool: &PoolRegistration,
        log: &Log,
        _view: &dyn StateView,
    ) -> AdapterEventResult {
        let Some(topic0) = log.topics().first().copied() else {
            return AdapterEventResult::ignored();
        };

        if topic0 == Swap::SIGNATURE_HASH {
            self.decode_swap(pool, log)
        } else if topic0 == Mint::SIGNATURE_HASH {
            self.decode_tick_range_repair(pool, log, true)
        } else if topic0 == Burn::SIGNATURE_HASH {
            self.decode_tick_range_repair(pool, log, false)
        } else {
            AdapterEventResult::ignored()
        }
    }

    fn after_apply(
        &self,
        _pool: &PoolRegistration,
        event: &AdapterEvent,
        diff: &StateDiff,
    ) -> RepairAction {
        if event.kind != AdapterEventKind::Swap || !diff.has_skipped() {
            return RepairAction::None;
        }

        let mut slots = Vec::new();
        for skipped in &diff.skipped_masks {
            slots.push((skipped.address, skipped.slot));
        }
        for skipped in &diff.skipped {
            slots.push((skipped.address, skipped.slot));
        }

        if slots.is_empty() {
            RepairAction::None
        } else {
            RepairAction::VerifySlots(slots)
        }
    }
}

impl UniswapV3Adapter {
    fn decode_swap(&self, pool: &PoolRegistration, log: &Log) -> AdapterEventResult {
        if Swap::decode_log_data_validate(&log.data).is_err() {
            return AdapterEventResult::error(AdapterEventError::MalformedLog(
                "malformed V3 Swap log",
            ));
        }

        let Some(address) = pool.key.address() else {
            return AdapterEventResult::error(AdapterEventError::MalformedLog(
                "V3 pool key is not address-keyed",
            ));
        };
        let Some(layout) = layout_for(pool) else {
            return AdapterEventResult::error(AdapterEventError::Unsupported(
                super::UnsupportedReason::MissingMetadata("V3 storage layout"),
            ));
        };

        let Some(sqrt_price) = data_word(log, 2) else {
            return AdapterEventResult::error(AdapterEventError::MalformedLog(
                "missing V3 sqrtPriceX96",
            ));
        };
        let Some(liquidity) = data_word(log, 3) else {
            return AdapterEventResult::error(AdapterEventError::MalformedLog(
                "missing V3 liquidity",
            ));
        };
        let Some(tick_word) = data_word(log, 4) else {
            return AdapterEventResult::error(AdapterEventError::MalformedLog("missing V3 tick"));
        };

        let tick = int24_from_word(tick_word);
        let tick24 = U256::from((tick as u32) & 0x00FF_FFFF);
        let mask = low_mask(SLOT0_PRICE_TICK_BITS);
        let value = sqrt_price | (tick24 << SLOT0_TICK_SHIFT);

        AdapterEventResult::event(AdapterEvent {
            pool: pool.key.clone(),
            emitter: log.address,
            topic0: Swap::SIGNATURE_HASH,
            kind: AdapterEventKind::Swap,
            updates: vec![
                StateUpdate::slot_masked(address, layout.slot0_slot, mask, value),
                StateUpdate::slot(address, layout.liquidity_slot, liquidity),
            ],
            quality: UpdateQuality::ExactIfApplied,
            repair: RepairAction::None,
        })
    }

    fn decode_tick_range_repair(
        &self,
        pool: &PoolRegistration,
        log: &Log,
        is_mint: bool,
    ) -> AdapterEventResult {
        let decode_ok = if is_mint {
            Mint::decode_log_data_validate(&log.data).is_ok()
        } else {
            Burn::decode_log_data_validate(&log.data).is_ok()
        };
        if !decode_ok {
            return AdapterEventResult::error(AdapterEventError::MalformedLog(
                "malformed V3 liquidity log",
            ));
        }

        let Some(tick_lower_topic) = log.topics().get(2) else {
            return AdapterEventResult::error(AdapterEventError::MalformedLog(
                "missing V3 tickLower topic",
            ));
        };
        let Some(tick_upper_topic) = log.topics().get(3) else {
            return AdapterEventResult::error(AdapterEventError::MalformedLog(
                "missing V3 tickUpper topic",
            ));
        };

        let topic0 = if is_mint {
            Mint::SIGNATURE_HASH
        } else {
            Burn::SIGNATURE_HASH
        };
        let kind = if is_mint {
            AdapterEventKind::LiquidityAdded
        } else {
            AdapterEventKind::LiquidityRemoved
        };
        let tick_lower = topic_to_i32(tick_lower_topic);
        let tick_upper = topic_to_i32(tick_upper_topic);

        AdapterEventResult::event(AdapterEvent {
            pool: pool.key.clone(),
            emitter: log.address,
            topic0,
            kind,
            updates: Vec::new(),
            quality: UpdateQuality::RequiresRepair,
            repair: RepairAction::V3TickRange {
                pool: pool.key.clone(),
                tick_lower,
                tick_upper,
            },
        })
    }
}

pub(crate) fn layout_for(pool: &PoolRegistration) -> Option<V3StorageLayout> {
    match &pool.metadata {
        ProtocolMetadata::UniswapV3(metadata) => {
            layout_from_metadata(metadata, ProtocolId::UniswapV3)
        }
        ProtocolMetadata::PancakeV3(metadata) => {
            layout_from_metadata(metadata, ProtocolId::PancakeV3)
        }
        ProtocolMetadata::Slipstream(metadata) => {
            layout_from_metadata(metadata, ProtocolId::Slipstream)
        }
        _ => None,
    }
}

fn layout_from_metadata(metadata: &V3Metadata, protocol: ProtocolId) -> Option<V3StorageLayout> {
    metadata.storage_layout.or_else(|| {
        let spacing = metadata.tick_spacing?;
        match protocol {
            ProtocolId::UniswapV3 => Some(V3StorageLayout::uniswap(spacing)),
            ProtocolId::PancakeV3 => Some(V3StorageLayout::pancake(spacing)),
            ProtocolId::Slipstream => Some(V3StorageLayout::slipstream(spacing)),
            _ => None,
        }
    })
}

fn data_word(log: &Log, index: usize) -> Option<U256> {
    let start = index.checked_mul(32)?;
    log.data
        .data
        .get(start..start + 32)
        .map(U256::from_be_slice)
}

fn int24_from_word(word: U256) -> i32 {
    let raw = (word & U256::from(0x00FF_FFFFu32)).to::<u32>();
    if raw & 0x0080_0000 != 0 {
        (raw | 0xFF00_0000) as i32
    } else {
        raw as i32
    }
}

fn topic_to_i32(topic: &B256) -> i32 {
    let mut bytes = [0u8; 4];
    bytes.copy_from_slice(&topic.as_slice()[28..32]);
    i32::from_be_bytes(bytes)
}

fn low_mask(bits: usize) -> U256 {
    (U256::from(1) << bits) - U256::from(1)
}
