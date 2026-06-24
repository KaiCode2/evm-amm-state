use alloy_primitives::{Address, B256, Bytes, Log, U256, aliases::U24};
use alloy_sol_types::{SolCall, SolEvent, sol};
use evm_fork_cache::cold_start::{
    ColdStartPlan, ColdStartResults, ColdStartRunReport, ColdStartStep, SlotFetch,
};

use super::cold_start::AdapterColdStartPlanner;
use super::sim::{
    QuoteExactInputSingleParams, SimConfig, SimError, SwapQuote, quoteExactInputSingleCall,
    run_quote,
};
use super::{
    AdapterCache, AdapterEvent, AdapterEventError, AdapterEventKind, AdapterEventResult,
    AmmAdapter, ColdStartOutcome, ColdStartPolicy, ColdStartReport, DeferredWork, EventSource,
    PoolRegistration, PoolStatus, ProtocolId, ProtocolMetadata, RepairAction, SlotChange,
    StateDiff, StateUpdate, StateView, UnsupportedReason, UpdateQuality, V3Metadata,
};
use crate::adapters::storage::{
    V3StorageLayout, layout_for, v3_tick_bitmap_storage_key_with_base,
    v3_tick_info_storage_keys_with_base, v3_word_position,
};

sol! {
    event Swap(address indexed sender, address indexed recipient, int256 amount0, int256 amount1, uint160 sqrtPriceX96, uint128 liquidity, int24 tick);
    event Mint(address sender, address indexed owner, int24 indexed tickLower, int24 indexed tickUpper, uint128 amount, uint256 amount0, uint256 amount1);
    event Burn(address indexed owner, int24 indexed tickLower, int24 indexed tickUpper, uint128 amount, uint256 amount0, uint256 amount1);
}

const SLOT0_PRICE_TICK_BITS: usize = 184;
const SLOT0_TICK_SHIFT: usize = 160;

/// Adapter for the Uniswap V3 storage-layout family.
///
/// A single instance serves Uniswap V3, Pancake V3, and Slipstream: those
/// protocols differ only in storage-slot offsets, which `layout_for` resolves
/// per-pool from the registration metadata. The struct is registered once and
/// claims all three ids via [`AmmAdapter::protocols`].
#[derive(Clone, Debug, Default)]
pub struct V3FamilyAdapter {
    _private: (),
}

impl AmmAdapter for V3FamilyAdapter {
    fn protocol(&self) -> ProtocolId {
        ProtocolId::UniswapV3
    }

    fn protocols(&self) -> Vec<ProtocolId> {
        vec![
            ProtocolId::UniswapV3,
            ProtocolId::PancakeV3,
            ProtocolId::Slipstream,
        ]
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

    fn cold_start_planner(
        &self,
        pool: &PoolRegistration,
        policy: ColdStartPolicy,
    ) -> Result<Box<dyn AdapterColdStartPlanner>, UnsupportedReason> {
        let Some(address) = pool.key.address() else {
            return Err(UnsupportedReason::Custom(
                "Uniswap V3 pool key is not address-keyed".into(),
            ));
        };

        // Resolve the storage layout before any fetch: the missing-layout case
        // must surface as `Unsupported` even when no batch fetcher is configured,
        // so the factory short-circuits here rather than running any round.
        let Some(layout) = layout_for(pool) else {
            return Err(UnsupportedReason::MissingMetadata("V3 storage layout"));
        };

        Ok(Box::new(UniswapV3ColdStartPlanner::new(
            address, layout, policy,
        )))
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

    /// Quote via `QuoterV2.quoteExactInputSingle((tokenIn, tokenOut, amountIn,
    /// fee, sqrtPriceLimitX96 = 0))`.
    ///
    /// The Quoter executes a real V3 swap against the warmed pool slots and
    /// returns the encoded `amountOut` (chain code, not reimplemented math). The
    /// pool `fee` is taken from the V3-family metadata; tick-crossing swaps stay
    /// correct because the cache lazily fetches any cold tick/bitmap slot from
    /// the backend.
    fn simulate_swap(
        &self,
        pool: &PoolRegistration,
        cache: &mut dyn AdapterCache,
        token_in: Address,
        token_out: Address,
        amount_in: U256,
        config: &SimConfig,
    ) -> Result<SwapQuote, SimError> {
        let fee = v3_fee(pool).ok_or(SimError::MissingMetadata("V3 fee"))?;

        let params = QuoteExactInputSingleParams {
            tokenIn: token_in,
            tokenOut: token_out,
            amountIn: amount_in,
            fee: U24::from(fee),
            sqrtPriceLimitX96: U256::ZERO.to(),
        };
        let calldata = Bytes::from(quoteExactInputSingleCall { params }.abi_encode());

        let output = run_quote(cache, config.v3_quoter, calldata)?;
        let decoded = quoteExactInputSingleCall::abi_decode_returns_validate(&output)
            .map_err(|_| SimError::MalformedOutput("quoteExactInputSingle return"))?;

        Ok(SwapQuote::new(decoded.amountOut))
    }
}

/// Read the pool `fee` (in hundredths of a bip, e.g. `500` for 0.05%) from the
/// V3-family metadata, regardless of which family variant the pool registered.
fn v3_fee(pool: &PoolRegistration) -> Option<u32> {
    let metadata: &V3Metadata = match &pool.metadata {
        ProtocolMetadata::UniswapV3(m)
        | ProtocolMetadata::PancakeV3(m)
        | ProtocolMetadata::Slipstream(m) => m,
        _ => return None,
    };
    metadata.fee
}

impl V3FamilyAdapter {
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

/// Cold-start planner for the Uniswap V3 storage-layout family.
///
/// Re-expresses the A3 bounded current-tick warm-up as planner rounds:
///
/// - Round 1 verifies `slot0` + global `liquidity`. `slot0` is mandatory; its
///   [`SlotFetch`] verdict decides ready vs. repair. From the warmed `slot0` the
///   current tick (and so the current `tickBitmap` word) is decoded.
/// - Round 2 (`Strict`/`Eager` only) verifies the current-tick bitmap word.
/// - Round 3 (`Strict`/`Eager` only) verifies the `{0, 3}` info slots of every
///   tick initialized in that word.
///
/// `HotSlotsOnly` stops after round 1 (slot0 + liquidity). `Lazy` stops after
/// round 1 and records the bitmap word as deferred work. The multi-word adaptive
/// scan stays deferred (future rounds, not this slice). Config-supplied V3
/// metadata is preserved unchanged.
struct UniswapV3ColdStartPlanner {
    address: Address,
    layout: V3StorageLayout,
    policy: ColdStartPolicy,
    phase: V3Phase,
    /// The current-tick bitmap word key, resolved from the warmed slot0.
    bitmap_key: Option<U256>,
    /// The current-tick bitmap word position, resolved from the warmed slot0.
    word: i16,
    verified_slots: Vec<(Address, U256)>,
    changed_slots: Vec<SlotChange>,
    deferred: Vec<DeferredWork>,
    /// `true` once round 1 found `slot0` cold (unfetchable / genuine zero).
    slot0_cold: bool,
}

/// Which cold-start round the V3 planner just completed.
#[derive(Clone, Copy, PartialEq, Eq)]
enum V3Phase {
    /// Round 1: slot0 + liquidity (the next `on_results` classifies slot0).
    Slot0Liquidity,
    /// Round 2: the current-tick bitmap word (the next `on_results` extracts
    /// the initialized ticks).
    BitmapWord,
    /// Round 3: the tick-info slots (the next `on_results` finishes).
    TickInfo,
}

impl UniswapV3ColdStartPlanner {
    fn new(address: Address, layout: V3StorageLayout, policy: ColdStartPolicy) -> Self {
        Self {
            address,
            layout,
            policy,
            phase: V3Phase::Slot0Liquidity,
            bitmap_key: None,
            word: 0,
            verified_slots: Vec::new(),
            changed_slots: Vec::new(),
            deferred: Vec::new(),
            slot0_cold: false,
        }
    }
}

impl AdapterColdStartPlanner for UniswapV3ColdStartPlanner {
    fn initial_plan(&mut self, _state: &dyn StateView) -> ColdStartPlan {
        // Round 1: slot0 + global liquidity. slot0 is mandatory (it carries the
        // current tick that drives the bounded tick warm-up); liquidity is an
        // absolute write that the reactive Swap always reapplies.
        let verify = vec![
            (self.address, self.layout.slot0_slot),
            (self.address, self.layout.liquidity_slot),
        ];
        self.verified_slots.extend_from_slice(&verify);
        ColdStartPlan {
            verify,
            ..Default::default()
        }
    }

    fn on_results(&mut self, results: &ColdStartResults, state: &dyn StateView) -> ColdStartStep {
        self.changed_slots.extend(results.verified.iter().cloned());

        match self.phase {
            V3Phase::Slot0Liquidity => {
                // slot0 is mandatory: classify it from its per-slot `SlotFetch`
                // rather than a `cached_storage(..).is_none()` proxy.
                let slot0_outcome = results
                    .fetched
                    .iter()
                    .find(|o| o.address == self.address && o.slot == self.layout.slot0_slot);
                let slot0_value = match slot0_outcome.map(|o| &o.fetch) {
                    Some(SlotFetch::Value(value)) => Some(*value),
                    // A genuine zero or a fetch failure leaves slot0 cold/unusable.
                    _ => None,
                };

                let Some(slot0) = slot0_value else {
                    self.slot0_cold = true;
                    return ColdStartStep::Done;
                };

                // Decode the current tick from the warm slot0 word (bits
                // [160, 184), 24-bit signed), reusing the reactive Swap decode.
                let tick = int24_from_word(slot0 >> SLOT0_TICK_SHIFT);
                let word = v3_word_position(tick, self.layout.tick_spacing);
                let bitmap_key =
                    v3_tick_bitmap_storage_key_with_base(word, self.layout.tick_bitmap_base_slot);
                self.word = word;
                self.bitmap_key = Some(bitmap_key);

                match self.policy {
                    ColdStartPolicy::Strict | ColdStartPolicy::Eager => {
                        // Round 2: warm the bitmap word containing the current tick.
                        self.phase = V3Phase::BitmapWord;
                        let verify = vec![(self.address, bitmap_key)];
                        self.verified_slots.extend_from_slice(&verify);
                        ColdStartStep::Continue(ColdStartPlan {
                            verify,
                            ..Default::default()
                        })
                    }
                    ColdStartPolicy::HotSlotsOnly => ColdStartStep::Done,
                    ColdStartPolicy::Lazy => {
                        // Warm the hot slots now; defer the tick word.
                        self.deferred
                            .push(DeferredWork::VerifySlots(vec![(self.address, bitmap_key)]));
                        ColdStartStep::Done
                    }
                }
            }
            V3Phase::BitmapWord => {
                // Round 3: warm the {0, 3} info slots of every tick initialized in
                // that word. The bitmap word is extracted adapter-locally: bit `i`
                // set => tick `(word * 256 + i) * tick_spacing`.
                let bitmap_key = self.bitmap_key.unwrap_or(U256::ZERO);
                let bitmap = state
                    .storage(self.address, bitmap_key)
                    .unwrap_or(U256::ZERO);
                let mut tick_slots: Vec<(Address, U256)> = Vec::new();
                for bit in 0..256u32 {
                    if (bitmap >> bit) & U256::from(1) == U256::from(1) {
                        let tick_i =
                            (self.word as i32 * 256 + bit as i32) * self.layout.tick_spacing;
                        let keys = v3_tick_info_storage_keys_with_base(
                            tick_i,
                            self.layout.ticks_base_slot,
                        );
                        tick_slots.push((self.address, keys[0]));
                        tick_slots.push((self.address, keys[3]));
                    }
                }

                if tick_slots.is_empty() {
                    ColdStartStep::Done
                } else {
                    self.phase = V3Phase::TickInfo;
                    self.verified_slots.extend_from_slice(&tick_slots);
                    ColdStartStep::Continue(ColdStartPlan {
                        verify: tick_slots,
                        ..Default::default()
                    })
                }
            }
            V3Phase::TickInfo => ColdStartStep::Done,
        }
    }

    fn finish(
        &mut self,
        pool: &mut PoolRegistration,
        _report: &ColdStartRunReport,
    ) -> ColdStartOutcome {
        let mut report = ColdStartReport::new(pool.key.clone(), self.policy);
        report.verified_slots = self.verified_slots.clone();
        report.changed_slots = self.changed_slots.clone();

        if self.slot0_cold {
            report.status = PoolStatus::Degraded;
            return ColdStartOutcome::NeedsRepair(
                report,
                RepairAction::VerifySlots(vec![(self.address, self.layout.slot0_slot)]),
            );
        }

        // Preserve the config-supplied V3 metadata (token0/token1/fee/
        // tick_spacing/layout are not at predictable storage slots and are not
        // re-fetched here).
        pool.status = PoolStatus::Ready;
        report.status = PoolStatus::Ready;

        if self.deferred.is_empty() {
            ColdStartOutcome::Ready(report)
        } else {
            report.deferred = self.deferred.clone();
            ColdStartOutcome::ReadyWithDeferred(report, self.deferred.clone())
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
