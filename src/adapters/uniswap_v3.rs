use super::bytecode::{AdapterCodeSeed, BytecodeTemplateError, v3_code_seed_from_metadata};
use super::cold_start::{
    AdapterColdStartPlanner, ColdStartPlan, ColdStartResults, ColdStartRunReport, ColdStartStep,
    SlotFetch,
};
use super::factory::{ConcentratedLiquidityFactory, FactoryConfig, PoolFactory};
use super::sim::{
    QuoteExactInputSingleParams, SimConfig, SimError, SwapQuote, quote_via_call,
    quoteExactInputSingleCall,
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
use alloy_primitives::{Address, B256, Bytes, Log, U256, aliases::U24};
use alloy_sol_types::{SolCall, SolEvent, sol};

sol! {
    event Swap(address indexed sender, address indexed recipient, int256 amount0, int256 amount1, uint160 sqrtPriceX96, uint128 liquidity, int24 tick);
    event Mint(address sender, address indexed owner, int24 indexed tickLower, int24 indexed tickUpper, uint128 amount, uint256 amount0, uint256 amount1);
    event Burn(address indexed owner, int24 indexed tickLower, int24 indexed tickUpper, uint128 amount, uint256 amount0, uint256 amount1);
}

const SLOT0_PRICE_TICK_BITS: usize = 184;
const SLOT0_TICK_SHIFT: usize = 160;

/// The minimum/maximum tick a Uniswap V3 pool can reach (`±887272`). Ticks (and
/// the tick-bitmap words derived from them) outside this range never exist, so
/// the cold-start window is clamped to it to avoid warming non-existent slots.
const V3_MIN_TICK: i32 = -887272;
const V3_MAX_TICK: i32 = 887272;

/// Radius (in tick-bitmap words) of the cold-start tick warm-up window.
///
/// The warmed window is `[W0 - R, W0 + R]` — `2R + 1` words centred on the
/// current-tick word `W0`. One word covers `256 * tick_spacing` of tick range,
/// so `R = 2` pre-warms ±2 words: generous headroom for a moderate
/// tick-crossing swap while keeping the warm-up strictly bounded (never more
/// than `2R + 1` bitmap words plus their initialized ticks). A true
/// outward-adaptive scan (extend until N consecutive empty words) is a future
/// refinement; this single named constant is the tuning knob until then.
pub(crate) const V3_TICK_WORD_RADIUS: i16 = 2;

/// Adapter for the Uniswap V3 storage-layout family.
///
/// A single instance serves Uniswap V3, Pancake V3, and Slipstream: those
/// protocols differ only in storage-slot offsets, which `layout_for` resolves
/// per-pool from the registration metadata. The struct is registered once and
/// claims all three ids via [`AmmAdapter::protocols`].
#[derive(Clone, Debug, Default)]
pub struct ConcentratedLiquidityAdapter {
    _private: (),
}

impl AmmAdapter for ConcentratedLiquidityAdapter {
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

    fn pool_factories(&self, config: &FactoryConfig) -> Vec<Box<dyn PoolFactory>> {
        config
            .concentrated_liquidity
            .iter()
            .map(|spec| {
                // The config-level `verify_derivations` is a global off-switch: a
                // spec's CREATE2 cross-check runs only when both it and the global
                // flag opt in.
                let mut spec = spec.clone();
                spec.verify_derivations &= config.verify_derivations;
                Box::new(ConcentratedLiquidityFactory::new(spec)) as Box<dyn PoolFactory>
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
                "Uniswap V3 pool key is not address-keyed".into(),
            ));
        };

        // Resolve the storage layout before any fetch: the missing-layout case
        // must surface as `Unsupported` even when no batch fetcher is configured,
        // so the factory short-circuits here rather than running any round.
        let Some(layout) = layout_for(pool) else {
            return Err(UnsupportedReason::MissingMetadata("V3 storage layout"));
        };

        // Per-pool tick-warm radius from V3 metadata, defaulting to the crate
        // constant when the field (or the metadata) is absent.
        let radius = v3_warm_word_radius(pool).unwrap_or(V3_TICK_WORD_RADIUS);

        Ok(Box::new(UniswapV3ColdStartPlanner::new(
            address, layout, policy, radius,
        )))
    }

    fn code_seeds(
        &self,
        pool: &PoolRegistration,
    ) -> Result<Vec<AdapterCodeSeed>, BytecodeTemplateError> {
        let Some(address) = pool.key.address() else {
            return Ok(Vec::new());
        };
        let Some(metadata) = v3_metadata(pool) else {
            return Ok(Vec::new());
        };
        v3_code_seed_from_metadata(address, metadata).map(|opt| opt.into_iter().collect())
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
    ///
    /// The quote target is the pool's own [`V3Metadata::quoter`] when set (a
    /// fork's QuoterV2, e.g. PancakeSwap's, filled in by factory discovery),
    /// falling back to the caller's [`SimConfig::v3_quoter`] otherwise. The quote
    /// ABI is unchanged (the `fee`-param struct variant) — forks whose quoter
    /// takes a different struct (e.g. Slipstream's `tickSpacing`-keyed quoter)
    /// leave `quoter` unset and ride the caller's Uniswap-compatible quoter.
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
        let quoter = v3_metadata(pool)
            .and_then(|m| m.quoter)
            .unwrap_or(config.v3_quoter);

        let params = QuoteExactInputSingleParams {
            tokenIn: token_in,
            tokenOut: token_out,
            amountIn: amount_in,
            fee: U24::from(fee),
            sqrtPriceLimitX96: U256::ZERO.to(),
        };
        let calldata = Bytes::from(quoteExactInputSingleCall { params }.abi_encode());

        let output = quote_via_call(cache, quoter, calldata)?;
        let decoded = quoteExactInputSingleCall::abi_decode_returns_validate(&output)
            .map_err(|_| SimError::MalformedOutput("quoteExactInputSingle return"))?;

        Ok(SwapQuote::new(decoded.amountOut))
    }
}

/// Read the pool `fee` (in hundredths of a bip, e.g. `500` for 0.05%) from the
/// V3-family metadata, regardless of which family variant the pool registered.
fn v3_fee(pool: &PoolRegistration) -> Option<u32> {
    v3_metadata(pool).and_then(|m| m.fee)
}

/// Read the per-pool cold-start tick-warm radius (in tick-bitmap words) from the
/// V3-family metadata, regardless of which family variant the pool registered.
///
/// Returns `None` when the metadata is absent or `warm_word_radius` is unset, in
/// which case callers fall back to [`V3_TICK_WORD_RADIUS`].
fn v3_warm_word_radius(pool: &PoolRegistration) -> Option<i16> {
    v3_metadata(pool).and_then(|m| m.warm_word_radius)
}

/// Borrow the [`V3Metadata`] for a pool if it registered as any V3-family
/// variant (Uniswap V3 / Pancake V3 / Slipstream), else `None`.
fn v3_metadata(pool: &PoolRegistration) -> Option<&V3Metadata> {
    match &pool.metadata {
        ProtocolMetadata::UniswapV3(m)
        | ProtocolMetadata::PancakeV3(m)
        | ProtocolMetadata::Slipstream(m) => Some(m),
        _ => None,
    }
}

impl ConcentratedLiquidityAdapter {
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
/// Warms a bounded **window** of tick-bitmap words around the current tick as
/// planner rounds:
///
/// - Round 1 verifies `slot0` + global `liquidity`. `slot0` is mandatory; its
///   [`SlotFetch`] verdict decides ready vs. repair. From the warmed `slot0` the
///   current tick — and so the current `tickBitmap` word `W0` — is decoded, then
///   the window `[W0 - R, W0 + R]` (`R = ` the pool's
///   [`V3Metadata::warm_word_radius`], defaulting to [`V3_TICK_WORD_RADIUS`]) is
///   computed, clamped to the valid V3 word range, and each word's bitmap key
///   resolved.
/// - Round 2 (`Strict`/`Eager` only) verifies **all** window bitmap words in one
///   round.
/// - Round 3 (`Strict`/`Eager` only) verifies the `{0, 3}` info slots of every
///   tick initialized across the whole window in one round.
///
/// `HotSlotsOnly` stops after round 1 (slot0 + liquidity — no bitmap/tick
/// warming). `Lazy` stops after round 1 and defers the **window** of bitmap
/// words. Config-supplied V3 metadata is preserved unchanged.
struct UniswapV3ColdStartPlanner {
    address: Address,
    layout: V3StorageLayout,
    policy: ColdStartPolicy,
    /// ± radius, in tick-bitmap words, of the cold-start tick-warm window (from
    /// the pool's [`V3Metadata::warm_word_radius`], or [`V3_TICK_WORD_RADIUS`]
    /// when unset). Clamped to `>= 0` in [`Self::resolve_window`].
    radius: i16,
    phase: V3Phase,
    /// The cold-start window: each `(word, bitmap_key)` pair in
    /// `[W0 - R, W0 + R]` clamped to the valid V3 word range, resolved from the
    /// warmed slot0. Empty until round 1 decodes the current tick.
    window: Vec<(i16, U256)>,
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
    /// Round 2: the window of bitmap words (the next `on_results` extracts the
    /// initialized ticks across the whole window).
    BitmapWord,
    /// Round 3: the tick-info slots (the next `on_results` finishes).
    TickInfo,
}

impl UniswapV3ColdStartPlanner {
    fn new(
        address: Address,
        layout: V3StorageLayout,
        policy: ColdStartPolicy,
        radius: i16,
    ) -> Self {
        Self {
            address,
            layout,
            policy,
            radius,
            phase: V3Phase::Slot0Liquidity,
            window: Vec::new(),
            verified_slots: Vec::new(),
            changed_slots: Vec::new(),
            deferred: Vec::new(),
            slot0_cold: false,
        }
    }

    /// Resolve the bounded window of bitmap words `[W0 - R, W0 + R]` around the
    /// current-tick word, clamped to the valid V3 word range, returning each
    /// `(word, bitmap_key)` pair.
    ///
    /// `R` is `self.radius` (the pool's [`V3Metadata::warm_word_radius`], or
    /// [`V3_TICK_WORD_RADIUS`] when unset), clamped to `>= 0` so a negative
    /// radius is treated as `0` (current word only) rather than underflowing the
    /// window math.
    ///
    /// The word clamp derives from `MIN_TICK`/`MAX_TICK = ±887272`: words outside
    /// the pool's reachable word range are skipped. All arithmetic is done in
    /// `i32` before the final `i16` cast so the radius offset can never overflow.
    fn resolve_window(&self, current_word: i16) -> Vec<(i16, U256)> {
        let radius = self.radius.max(0) as i32;
        let min_word = v3_word_position(V3_MIN_TICK, self.layout.tick_spacing) as i32;
        let max_word = v3_word_position(V3_MAX_TICK, self.layout.tick_spacing) as i32;

        let lo = (current_word as i32 - radius).max(min_word);
        let hi = (current_word as i32 + radius).min(max_word);

        let mut window = Vec::new();
        let mut word = lo;
        while word <= hi {
            let word_i16 = word as i16;
            let key =
                v3_tick_bitmap_storage_key_with_base(word_i16, self.layout.tick_bitmap_base_slot);
            window.push((word_i16, key));
            word += 1;
        }
        window
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
                // [160, 184), 24-bit signed), reusing the reactive Swap decode,
                // then resolve the bounded window of bitmap words around it.
                let tick = int24_from_word(slot0 >> SLOT0_TICK_SHIFT);
                let current_word = v3_word_position(tick, self.layout.tick_spacing);
                self.window = self.resolve_window(current_word);

                match self.policy {
                    ColdStartPolicy::Strict | ColdStartPolicy::Eager => {
                        // Round 2: warm every bitmap word in the window in one round.
                        self.phase = V3Phase::BitmapWord;
                        let verify: Vec<(Address, U256)> = self
                            .window
                            .iter()
                            .map(|(_, key)| (self.address, *key))
                            .collect();
                        self.verified_slots.extend_from_slice(&verify);
                        ColdStartStep::Continue(ColdStartPlan {
                            verify,
                            ..Default::default()
                        })
                    }
                    ColdStartPolicy::HotSlotsOnly => ColdStartStep::Done,
                    ColdStartPolicy::Lazy => {
                        // Warm the hot slots now; defer the whole window of bitmap words.
                        let window_keys: Vec<(Address, U256)> = self
                            .window
                            .iter()
                            .map(|(_, key)| (self.address, *key))
                            .collect();
                        self.deferred.push(DeferredWork::VerifySlots(window_keys));
                        ColdStartStep::Done
                    }
                }
            }
            V3Phase::BitmapWord => {
                // Round 3: warm the {0, 3} info slots of every tick initialized
                // across the whole window. Each window word's bitmap is extracted
                // adapter-locally: bit `i` set => tick `(word * 256 + i) *
                // tick_spacing`, skipping any tick outside [MIN_TICK, MAX_TICK].
                let mut tick_slots: Vec<(Address, U256)> = Vec::new();
                for (word, bitmap_key) in &self.window {
                    let bitmap = state
                        .storage(self.address, *bitmap_key)
                        .unwrap_or(U256::ZERO);
                    for bit in 0..256u32 {
                        if (bitmap >> bit) & U256::from(1) == U256::from(1) {
                            // Compute the tick index in i32; word/bit/spacing are
                            // all bounded so this cannot overflow.
                            let tick_i =
                                (*word as i32 * 256 + bit as i32) * self.layout.tick_spacing;
                            if !(V3_MIN_TICK..=V3_MAX_TICK).contains(&tick_i) {
                                continue;
                            }
                            let keys = v3_tick_info_storage_keys_with_base(
                                tick_i,
                                self.layout.ticks_base_slot,
                            );
                            tick_slots.push((self.address, keys[0]));
                            tick_slots.push((self.address, keys[3]));
                        }
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
