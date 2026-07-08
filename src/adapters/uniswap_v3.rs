use super::bytecode::{AdapterCodeSeed, BytecodeTemplateError, v3_code_seed_from_metadata};
use super::cold_start::{
    AdapterColdStartPlanner, ColdStartPlan, ColdStartResults, ColdStartRunReport, ColdStartStep,
    SlotFetch,
};
use super::factory::{ConcentratedLiquidityFactory, FactoryConfig, PoolFactory};
use super::sim::{
    QuoteExactInputSingleParams, SimConfig, SimError, SwapQuote, quote_via_call_from,
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
use alloy_sol_types::{SolCall, SolEvent};

/// `sol!`-generated pool event bindings (crate-internal, not public API).
mod abi {
    alloy_sol_types::sol! {
        event Swap(address indexed sender, address indexed recipient, int256 amount0, int256 amount1, uint160 sqrtPriceX96, uint128 liquidity, int24 tick);
        event Mint(address sender, address indexed owner, int24 indexed tickLower, int24 indexed tickUpper, uint128 amount, uint256 amount0, uint256 amount1);
        event Burn(address indexed owner, int24 indexed tickLower, int24 indexed tickUpper, uint128 amount, uint256 amount0, uint256 amount1);
    }
}
use abi::{Burn, Mint, Swap};

/// PancakeSwap V3 `Swap` appends `protocolFeesToken0`/`protocolFeesToken1`
/// (`uint128`) to the Uniswap V3 event, so its `topic0` differs (`0x19b47279…`
/// vs Uniswap's `0xc42079f9…`). The extra fields append after `tick`, so
/// `sqrtPriceX96`/`liquidity`/`tick` stay at data words 2/3/4 and the body decode
/// is shared with the Uniswap [`Swap`]. `Mint`/`Burn` are unchanged from Uniswap
/// V3, so their hashes are shared. Wrapped in a module so the 9-field event's
/// `sol!`-generated constructor can be exempted from `clippy::too_many_arguments`
/// without relaxing the lint for the rest of the file.
mod pancake_v3 {
    #![allow(clippy::too_many_arguments)]
    alloy_sol_types::sol! {
        event Swap(address indexed sender, address indexed recipient, int256 amount0, int256 amount1, uint160 sqrtPriceX96, uint128 liquidity, int24 tick, uint128 protocolFeesToken0, uint128 protocolFeesToken1);
    }
}
use pancake_v3::Swap as PancakeV3Swap;

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
                        swap_topic_for(pool.protocol()),
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
        view: &dyn StateView,
    ) -> AdapterEventResult {
        let Some(topic0) = log.topics().first().copied() else {
            return AdapterEventResult::ignored();
        };

        if topic0 == Swap::SIGNATURE_HASH {
            self.decode_swap(pool, log, topic0, SwapAbi::Uniswap)
        } else if topic0 == PancakeV3Swap::SIGNATURE_HASH {
            self.decode_swap(pool, log, topic0, SwapAbi::Pancake)
        } else if topic0 == Mint::SIGNATURE_HASH {
            self.decode_liquidity_event(pool, log, view, true)
        } else if topic0 == Burn::SIGNATURE_HASH {
            self.decode_liquidity_event(pool, log, view, false)
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

        let output = quote_via_call_from(cache, config.from, quoter, calldata)?;
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

/// Which `Swap` ABI a routed log matched — the Uniswap V3 shape or the
/// PancakeSwap V3 shape (two extra `uint128` fields, so a distinct `topic0`).
#[derive(Clone, Copy)]
enum SwapAbi {
    Uniswap,
    Pancake,
}

/// The `Swap` event `topic0` to subscribe/route for `protocol`.
///
/// PancakeSwap V3 emits an extended `Swap` (extra `protocolFeesToken0/1`), so its
/// `topic0` differs from Uniswap's; every other V3-family fork (Uniswap V3,
/// Slipstream) uses the canonical Uniswap `Swap` hash.
fn swap_topic_for(protocol: ProtocolId) -> B256 {
    match protocol {
        ProtocolId::PancakeV3 => PancakeV3Swap::SIGNATURE_HASH,
        _ => Swap::SIGNATURE_HASH,
    }
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
    fn decode_swap(
        &self,
        pool: &PoolRegistration,
        log: &Log,
        topic0: B256,
        abi: SwapAbi,
    ) -> AdapterEventResult {
        // Validate against the ABI whose topic0 matched. Cross-protocol safety
        // comes from topic0 routing — a pool only subscribes its own Swap hash
        // (`swap_topic_for`), so `decode_event` always pairs the matched topic0
        // with its `SwapAbi` — NOT from payload length: alloy's log decoder reads
        // only the leading static words, so the Uniswap validator tolerates the
        // Pancake body's two trailing `uint128`s (the Pancake validator, which
        // needs more words, does reject the shorter Uniswap body). Either way
        // `sqrtPriceX96`/`liquidity`/`tick` share data words 2/3/4, so the body
        // decode below is ABI-agnostic once the matched validator passes.
        let valid = match abi {
            SwapAbi::Uniswap => Swap::decode_log_data_validate(&log.data).is_ok(),
            SwapAbi::Pancake => PancakeV3Swap::decode_log_data_validate(&log.data).is_ok(),
        };
        if !valid {
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
            topic0,
            kind: AdapterEventKind::Swap,
            updates: vec![
                StateUpdate::slot_masked(address, layout.slot0_slot, mask, value),
                StateUpdate::slot(address, layout.liquidity_slot, liquidity),
            ],
            quality: UpdateQuality::ExactIfApplied,
            repair: RepairAction::None,
        })
    }

    /// Decode a Uniswap V3 `Mint`/`Burn` and **event-source** the affected state
    /// directly wherever it is already warm — no RPC — falling back to a targeted
    /// resync only for boundary ticks whose base value is cold (outside the warmed
    /// window).
    ///
    /// The event carries the exact liquidity delta (`amount`) and the boundary
    /// ticks; the current tick comes from cached `slot0`. For each **warm**
    /// boundary tick this read-modify-writes the packed `Tick.Info` word 0
    /// (`liquidityGross` in the low 128 bits, `liquidityNet` in the high 128 —
    /// moving in *opposite* directions for the lower vs. upper tick) and toggles
    /// the `tickBitmap` bit when the tick initializes or clears (the contract's
    /// `flipTick` is exactly an XOR of that bit). The global `liquidity` slot is
    /// adjusted by `±amount` when the position straddles the current tick. Those
    /// are precisely the slots a `QuoterV2` swap reads; `feeGrowthOutside` and the
    /// `positions` mapping are accounting-only (they do not affect `amountOut`) and
    /// are intentionally not maintained here.
    ///
    /// A **cold** boundary tick (its word 0 not cached) cannot be
    /// read-modify-written, so its info + bitmap slots are emitted as a
    /// [`RepairAction::VerifySlots`] resync instead — the hybrid write-where-warm /
    /// resync-cold policy. A pool with no resolvable layout falls back to a
    /// conservative whole-storage invalidation.
    fn decode_liquidity_event(
        &self,
        pool: &PoolRegistration,
        log: &Log,
        view: &dyn StateView,
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
        let tick_lower = topic_to_i32(tick_lower_topic);
        let tick_upper = topic_to_i32(tick_upper_topic);

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

        let Some(address) = pool.key.address() else {
            return AdapterEventResult::error(AdapterEventError::MalformedLog(
                "V3 pool key is not address-keyed",
            ));
        };

        // The `amount` (uint128 liquidity L) is the first NON-indexed data word:
        // index 1 for Mint (a non-indexed `sender` precedes it) and index 0 for
        // Burn (no leading non-indexed field).
        let amount_word_index = if is_mint { 1 } else { 0 };
        let Some(amount_word) = data_word(log, amount_word_index) else {
            return AdapterEventResult::error(AdapterEventError::MalformedLog(
                "missing V3 liquidity amount",
            ));
        };
        let amount = u128_low(amount_word);

        // Without a resolvable layout the protocol slots cannot be named safely, so
        // conservatively invalidate all of the pool's storage (prior behavior).
        let Some(layout) = layout_for(pool) else {
            return AdapterEventResult::event(
                AdapterEvent::new(
                    pool.key.clone(),
                    log.address,
                    topic0,
                    kind,
                    UpdateQuality::RequiresRepair,
                )
                .with_repair(RepairAction::PurgeStorage(address)),
            );
        };

        let mut updates: Vec<StateUpdate> = Vec::new();
        let mut resync: Vec<(Address, U256)> = Vec::new();

        // Current tick from cached slot0 drives the in-range check for the global
        // liquidity. slot0 is a mandatory cold-start slot; the boundary-tick writes
        // below are independent of it.
        let current_tick = view
            .storage(address, layout.slot0_slot)
            .map(|slot0| int24_from_word(slot0 >> SLOT0_TICK_SHIFT));

        match current_tick {
            // In range: apply ±amount to the warm global liquidity, or resync it.
            Some(tick) if tick_lower <= tick && tick < tick_upper => {
                match view.storage(address, layout.liquidity_slot) {
                    Some(old) => {
                        let new = if is_mint {
                            old.saturating_add(U256::from(amount))
                        } else {
                            old.saturating_sub(U256::from(amount))
                        };
                        updates.push(StateUpdate::slot(address, layout.liquidity_slot, new));
                    }
                    None => resync.push((address, layout.liquidity_slot)),
                }
            }
            // Out of range: the position does not straddle the current tick, so the
            // global liquidity is unaffected.
            Some(_) => {}
            // slot0 cold (a degraded pool): the in-range decision cannot be made, so
            // conservatively resync the global liquidity slot to its on-chain truth
            // rather than silently dropping a possible delta (self-healing, and it
            // correctly forces RequiresRepair).
            None => resync.push((address, layout.liquidity_slot)),
        }

        // Bitmap-bit flips are accumulated per bitmap word as an XOR mask, then
        // emitted as ONE combined write per word below. Both boundary ticks can
        // land in the same word; two separate full-slot writes — each computed
        // from the same pre-event `view` — would not compose (the second would
        // clobber the first), so they must be merged before writing.
        let mut bitmap_toggles: Vec<(U256, U256)> = Vec::new();

        // Each boundary tick: read-modify-write the packed `Tick.Info` word 0 (and
        // record a bitmap-bit flip on an init/clear) when warm; resync when cold.
        for (tick, is_lower) in [(tick_lower, true), (tick_upper, false)] {
            let keys = v3_tick_info_storage_keys_with_base(tick, layout.ticks_base_slot);
            let word_pos = v3_word_position(tick, layout.tick_spacing);
            let bitmap_key =
                v3_tick_bitmap_storage_key_with_base(word_pos, layout.tick_bitmap_base_slot);

            let cold_fallback = |resync: &mut Vec<(Address, U256)>| {
                resync.extend(keys.iter().map(|slot| (address, *slot)));
                resync.push((address, bitmap_key));
            };

            let Some(old_word0) = view.storage(address, keys[0]) else {
                // Cold tick: cannot read-modify-write; resync its info + bitmap slots.
                cold_fallback(&mut resync);
                continue;
            };

            let Some((new_word0, was_init, now_init)) =
                apply_liquidity_delta(old_word0, amount, is_mint, is_lower)
            else {
                // Arithmetic out of range (should not happen for valid chain data):
                // resync this tick rather than write a wrong value.
                cold_fallback(&mut resync);
                continue;
            };

            updates.push(StateUpdate::slot(address, keys[0], new_word0));

            // The bitmap bit flips exactly when the tick's initialized state
            // changes (Uniswap `flipTick` XORs the bit).
            if was_init != now_init {
                if view.storage(address, bitmap_key).is_some() {
                    let mask = U256::from(1u8) << v3_bit_position(tick, layout.tick_spacing);
                    match bitmap_toggles
                        .iter_mut()
                        .find(|(key, _)| *key == bitmap_key)
                    {
                        Some((_, acc)) => *acc ^= mask,
                        None => bitmap_toggles.push((bitmap_key, mask)),
                    }
                } else {
                    // Cold bitmap word: cannot toggle without the base; resync it.
                    resync.push((address, bitmap_key));
                }
            }
        }

        // Emit one combined write per touched bitmap word (base XOR accumulated
        // mask), so both ticks' flips in a shared word compose correctly.
        for (bitmap_key, mask) in bitmap_toggles {
            match view.storage(address, bitmap_key) {
                Some(base) => updates.push(StateUpdate::slot(address, bitmap_key, base ^ mask)),
                None => resync.push((address, bitmap_key)),
            }
        }

        // Dedup the resync set (both boundary ticks can share a bitmap word).
        resync.sort_unstable();
        resync.dedup();

        let (quality, repair) = if resync.is_empty() {
            (UpdateQuality::Exact, RepairAction::None)
        } else {
            (
                UpdateQuality::RequiresRepair,
                RepairAction::VerifySlots(resync),
            )
        };

        AdapterEventResult::event(
            AdapterEvent::new(pool.key.clone(), log.address, topic0, kind, quality)
                .with_updates(updates)
                .with_repair(repair),
        )
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
/// - Round 3 (`Strict`/`Eager` only) verifies all four `Tick.Info` words of
///   every tick initialized across the whole window in one round.
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
                // Round 3: warm ALL FOUR `Tick.Info` words of every tick
                // initialized across the whole window. A tick-crossing swap quote
                // reads the full struct — `liquidityGross`/`liquidityNet` (word 0),
                // both `feeGrowthOutside{0,1}X128` (words 1/2), and the packed
                // `tickCumulative`/`secondsPerLiquidity`/`secondsOutside`/
                // `initialized` (word 3) — so warming only {0, 3} left a hard
                // tick-crossing quote lazily fetching words 1/2 (correct online,
                // but not fully offline). Warming all four matches the one-shot
                // full-sync program. Each window word's bitmap is extracted
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
                            tick_slots.extend(keys.iter().map(|key| (self.address, *key)));
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

/// The low 128 bits of a 256-bit word (a `Tick.Info` word 0's `liquidityGross`).
fn u128_low(word: U256) -> u128 {
    let limbs = word.as_limbs();
    (limbs[0] as u128) | ((limbs[1] as u128) << 64)
}

/// The high 128 bits of a 256-bit word, as raw bits (word 0's `liquidityNet`,
/// two's-complement `int128`).
fn u128_high(word: U256) -> u128 {
    let limbs = word.as_limbs();
    (limbs[2] as u128) | ((limbs[3] as u128) << 64)
}

/// Pack `liquidityGross` (low 128) and `liquidityNet` (high 128, two's complement)
/// back into a `Tick.Info` word-0 value.
fn pack_gross_net(gross: u128, net: i128) -> U256 {
    U256::from(gross) | (U256::from(net as u128) << 128)
}

/// The `tickBitmap` bit index (0..256) for `tick`, matching the V3
/// `TickBitmap.position` low byte (`compressed % 256`, floor-toward-negative).
/// `tick_spacing` must be positive (guaranteed by [`layout_for`]).
fn v3_bit_position(tick: i32, tick_spacing: i32) -> usize {
    tick.div_euclid(tick_spacing).rem_euclid(256) as usize
}

/// Apply a liquidity `amount` delta to a `Tick.Info` word 0, returning the new
/// packed word plus the tick's initialized state before/after.
///
/// `liquidityGross` always moves by `+amount` (mint) / `-amount` (burn);
/// `liquidityNet` moves `+amount` for the lower tick and `-amount` for the upper
/// on a mint (negated on a burn) — captured by `add_to_net = is_mint == is_lower`.
/// Returns `None` on arithmetic overflow/underflow (invalid chain data) so the
/// caller can resync the tick instead of writing a wrong value.
fn apply_liquidity_delta(
    word0: U256,
    amount: u128,
    is_mint: bool,
    is_lower: bool,
) -> Option<(U256, bool, bool)> {
    let old_gross = u128_low(word0);
    let old_net = u128_high(word0) as i128;
    let signed = i128::try_from(amount).ok()?;

    let new_gross = if is_mint {
        old_gross.checked_add(amount)?
    } else {
        old_gross.checked_sub(amount)?
    };
    let add_to_net = is_mint == is_lower;
    let new_net = if add_to_net {
        old_net.checked_add(signed)?
    } else {
        old_net.checked_sub(signed)?
    };

    let was_init = old_gross != 0;
    let now_init = new_gross != 0;
    Some((pack_gross_net(new_gross, new_net), was_init, now_init))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gross(word0: U256) -> u128 {
        u128_low(word0)
    }
    fn net(word0: U256) -> i128 {
        u128_high(word0) as i128
    }

    #[test]
    fn pack_unpack_round_trips_including_negative_net() {
        for (g, n) in [
            (0u128, 0i128),
            (5, 7),
            (u128::MAX, -1),
            (123, i128::MIN),
            (1, i128::MAX),
        ] {
            let w = pack_gross_net(g, n);
            assert_eq!(gross(w), g);
            assert_eq!(net(w), n);
        }
    }

    #[test]
    fn mint_lower_adds_gross_and_net() {
        // gross += amount (low), net += amount (high, lower tick).
        let (w, was, now) = apply_liquidity_delta(pack_gross_net(10, 3), 4, true, true).unwrap();
        assert_eq!(gross(w), 14);
        assert_eq!(net(w), 7);
        assert!(was && now);
    }

    #[test]
    fn mint_upper_adds_gross_subtracts_net() {
        let (w, _, _) = apply_liquidity_delta(pack_gross_net(10, 3), 4, true, false).unwrap();
        assert_eq!(gross(w), 14);
        assert_eq!(net(w), -1);
    }

    #[test]
    fn burn_lower_subtracts_both() {
        let (w, _, _) = apply_liquidity_delta(pack_gross_net(10, 3), 4, false, true).unwrap();
        assert_eq!(gross(w), 6);
        assert_eq!(net(w), -1);
    }

    #[test]
    fn burn_upper_subtracts_gross_adds_net() {
        let (w, _, _) = apply_liquidity_delta(pack_gross_net(10, 3), 4, false, false).unwrap();
        assert_eq!(gross(w), 6);
        assert_eq!(net(w), 7);
    }

    #[test]
    fn mint_onto_empty_tick_reports_initialization() {
        // A tick with zero gross that gains liquidity flips uninitialized→initialized.
        let (w, was, now) = apply_liquidity_delta(U256::ZERO, 5, true, true).unwrap();
        assert_eq!(gross(w), 5);
        assert_eq!(net(w), 5);
        assert!(!was && now);
    }

    #[test]
    fn burn_to_zero_reports_clear_and_zeroes_word() {
        // Burning all of a tick's gross flips initialized→uninitialized; the lower
        // tick's net returns to zero, so word 0 is fully zero.
        let (w, was, now) = apply_liquidity_delta(pack_gross_net(5, 5), 5, false, true).unwrap();
        assert_eq!(w, U256::ZERO);
        assert!(was && !now);
    }

    #[test]
    fn burn_more_than_gross_is_rejected() {
        assert!(apply_liquidity_delta(pack_gross_net(3, 3), 4, false, true).is_none());
    }

    // Pin the contract at the exact 128-bit boundaries: checked arithmetic
    // (`None` -> the caller resyncs the tick) — never a wrap or saturation
    // silently packed into a wrong word.
    #[test]
    fn liquidity_delta_boundary_values_reject_not_wrap() {
        // Filling gross to exactly u128::MAX is representable...
        let (w, was, now) =
            apply_liquidity_delta(pack_gross_net(u128::MAX - 4, 0), 4, true, true).unwrap();
        assert_eq!(gross(w), u128::MAX);
        assert!(was && now);
        // ...one more unit is None, not a wrap to zero.
        assert!(apply_liquidity_delta(pack_gross_net(u128::MAX, 0), 1, true, true).is_none());
        // Net overflow at i128::MAX (mint at the lower tick adds to net).
        assert!(apply_liquidity_delta(pack_gross_net(0, i128::MAX), 1, true, true).is_none());
        // Net underflow at i128::MIN (mint at the upper tick subtracts).
        assert!(apply_liquidity_delta(pack_gross_net(0, i128::MIN), 1, true, false).is_none());
        // An amount above i128::MAX cannot be a valid net move: rejected up front.
        assert!(apply_liquidity_delta(pack_gross_net(0, 0), 1u128 << 127, true, true).is_none());
        // The largest representable amount round-trips exactly.
        let amount = i128::MAX as u128;
        let (w, _, _) = apply_liquidity_delta(pack_gross_net(0, 0), amount, true, true).unwrap();
        assert_eq!(gross(w), amount);
        assert_eq!(net(w), i128::MAX);
    }

    #[test]
    fn bit_position_matches_uniswap_position_low_byte() {
        // spacing 1: compressed == tick; bit = tick mod 256 (floor for negatives).
        assert_eq!(v3_bit_position(0, 1), 0);
        assert_eq!(v3_bit_position(255, 1), 255);
        assert_eq!(v3_bit_position(256, 1), 0);
        assert_eq!(v3_bit_position(-1, 1), 255); // word -1, top bit
        assert_eq!(v3_bit_position(-256, 1), 0);
        // spacing 60: compressed = tick/60.
        assert_eq!(v3_bit_position(60, 60), 1);
        assert_eq!(v3_bit_position(120, 60), 2);
    }
}
