//! Exact, provider-free transitions for concentrated-liquidity pools.
//!
//! The transition starts from an exact parent [`StateView`], replays the
//! swap-induced writes locally, validates every event postcondition, and only
//! then returns one atomic update batch.  Missing parent cells are errors: an
//! absent cache value is never interpreted as an EVM zero.
//!
//! Canonical Uniswap replay owns the complete swap-mutated accounting surface.
//! Reviewed Slipstream replay owns the complete quote/search surface for swaps
//! and liquidity changes; optional runtime-bound evidence additionally enables
//! its fee, reward, gauge, and crossed-tick accounting writes.

use alloy_primitives::{Address, U256, address, aliases::U512, b256};

use super::storage::{
    V3StorageLayout, slipstream_tick_info_storage_keys_with_base,
    v3_tick_bitmap_storage_key_with_base, v3_tick_info_storage_keys_with_base, v3_word_position,
};
use super::{
    AdapterEventContext, AdapterEventError, SlipstreamRuntimeFamily, SlipstreamSwapFeeEvidence,
    SlipstreamUnstakedFeeProofKind, StateUpdate, StateView, V3TransitionError,
};

const FEE_DENOMINATOR: u64 = 1_000_000;
const MAX_SWAP_STEPS: u32 = 4_096;
const Q96: U256 = U256::from_limbs([0, 1 << 32, 0, 0]);
const Q128: U256 = U256::from_limbs([0, 0, 1, 0]);

/// Storage surface whose semantics have been independently proven against the
/// canonical Uniswap V3 pool. Fork-family support must supply its own surface
/// and transition semantics rather than reusing this descriptor based only on
/// coincidentally similar slot offsets.
#[derive(Clone, Copy, Debug)]
struct SwapStorageSurface {
    fee_growth_0_slot: U256,
    fee_growth_1_slot: U256,
    protocol_fees_slot: U256,
    observations_base_slot: U256,
}

const UNISWAP_V3_SURFACE: SwapStorageSurface = SwapStorageSurface {
    fee_growth_0_slot: U256::from_limbs([1, 0, 0, 0]),
    fee_growth_1_slot: U256::from_limbs([2, 0, 0, 0]),
    protocol_fees_slot: U256::from_limbs([3, 0, 0, 0]),
    observations_base_slot: U256::from_limbs([8, 0, 0, 0]),
};

const SLIPSTREAM_SURFACE: SwapStorageSurface = SwapStorageSurface {
    fee_growth_0_slot: U256::from_limbs([7, 0, 0, 0]),
    fee_growth_1_slot: U256::from_limbs([8, 0, 0, 0]),
    // Slipstream has no canonical protocolFees word. This field is never used
    // by its independent transition implementation.
    protocol_fees_slot: U256::ZERO,
    observations_base_slot: U256::from_limbs([20, 0, 0, 0]),
};

const SLIPSTREAM_FACTORY_SLOT: U256 = U256::ZERO;
const SLIPSTREAM_REWARD_GROWTH_SLOT: U256 = U256::from_limbs([9, 0, 0, 0]);
const SLIPSTREAM_GAUGE_FEES_SLOT: U256 = U256::from_limbs([10, 0, 0, 0]);
const SLIPSTREAM_REWARD_RATE_SLOT: U256 = U256::from_limbs([11, 0, 0, 0]);
const SLIPSTREAM_REWARD_RESERVE_SLOT: U256 = U256::from_limbs([12, 0, 0, 0]);
const SLIPSTREAM_ROLLOVER_SLOT: U256 = U256::from_limbs([14, 0, 0, 0]);
const SLIPSTREAM_STAKED_LAST_SPACING_SLOT: U256 = U256::from_limbs([15, 0, 0, 0]);

#[cfg(test)]
const FEE_GROWTH_0_SLOT: U256 = UNISWAP_V3_SURFACE.fee_growth_0_slot;
#[cfg(test)]
const FEE_GROWTH_1_SLOT: U256 = UNISWAP_V3_SURFACE.fee_growth_1_slot;
#[cfg(test)]
const PROTOCOL_FEES_SLOT: U256 = UNISWAP_V3_SURFACE.protocol_fees_slot;
#[cfg(test)]
const OBSERVATIONS_BASE_SLOT: U256 = UNISWAP_V3_SURFACE.observations_base_slot;

const SLOT0_SQRT_MASK: U256 = U256::from_limbs([u64::MAX, u64::MAX, u32::MAX as u64, 0]);
const SLOT0_TICK_MASK: U256 = U256::from_limbs([0x00ff_ffff, 0, 0, 0]);
const WORD_128_MASK: U256 = U256::from_limbs([u64::MAX, u64::MAX, 0, 0]);
const WORD_160_MASK: U256 = SLOT0_SQRT_MASK;
const WORD_56_MASK: U256 = U256::from_limbs([(1_u64 << 56) - 1, 0, 0, 0]);
const WORD_32_MASK: U256 = U256::from_limbs([u32::MAX as u64, 0, 0, 0]);

fn contradiction(reason: &'static str) -> AdapterEventError {
    AdapterEventError::V3Transition(V3TransitionError::ContradictoryEvent(reason))
}

fn final_mismatch(field: &'static str, derived: U256, event: U256) -> AdapterEventError {
    AdapterEventError::V3Transition(V3TransitionError::FinalStateMismatch {
        field,
        derived,
        event,
    })
}

fn arithmetic(reason: &'static str) -> AdapterEventError {
    AdapterEventError::V3Transition(V3TransitionError::Arithmetic(reason))
}

#[derive(Clone, Copy, Debug)]
pub(super) struct DecodedSwap {
    pub amount0_negative: bool,
    pub amount0: U256,
    pub amount1_negative: bool,
    pub amount1: U256,
    pub sqrt_price_x96: U256,
    pub liquidity: U256,
    pub tick: i32,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct DecodedLiquidity {
    pub tick_lower: i32,
    pub tick_upper: i32,
    pub amount: u128,
    pub is_mint: bool,
}

#[derive(Clone, Copy, Debug)]
struct Slot0 {
    raw: U256,
    sqrt_price_x96: U256,
    tick: i32,
    observation_index: u16,
    observation_cardinality: u16,
    observation_cardinality_next: u16,
    fee_protocol: u8,
}

impl Slot0 {
    fn decode(raw: U256) -> Self {
        Self {
            raw,
            sqrt_price_x96: raw & SLOT0_SQRT_MASK,
            tick: signed_from_bits((raw >> 160_usize) & SLOT0_TICK_MASK, 24) as i32,
            observation_index: ((raw >> 184_usize) & U256::from(u16::MAX)).to::<u16>(),
            observation_cardinality: ((raw >> 200_usize) & U256::from(u16::MAX)).to::<u16>(),
            observation_cardinality_next: ((raw >> 216_usize) & U256::from(u16::MAX)).to::<u16>(),
            fee_protocol: ((raw >> 232_usize) & U256::from(u8::MAX)).to::<u8>(),
        }
    }

    fn encode_final(
        self,
        sqrt_price_x96: U256,
        tick: i32,
        observation_index: u16,
        observation_cardinality: u16,
    ) -> U256 {
        let mutable_mask: U256 = SLOT0_SQRT_MASK
            | (SLOT0_TICK_MASK << 160_usize)
            | (U256::from(u16::MAX) << 184_usize)
            | (U256::from(u16::MAX) << 200_usize);
        let tick = U256::from((tick as u32) & 0x00ff_ffff);
        (self.raw & !mutable_mask)
            | sqrt_price_x96
            | (tick << 160_usize)
            | (U256::from(observation_index) << 184_usize)
            | (U256::from(observation_cardinality) << 200_usize)
    }
}

#[derive(Clone, Copy, Debug)]
struct Observation {
    timestamp: u32,
    tick_cumulative: i64,
    seconds_per_liquidity_cumulative_x128: U256,
    initialized: bool,
}

impl Observation {
    fn decode(raw: U256) -> Self {
        Self {
            timestamp: (raw & WORD_32_MASK).to::<u32>(),
            tick_cumulative: signed_from_bits((raw >> 32_usize) & WORD_56_MASK, 56),
            seconds_per_liquidity_cumulative_x128: (raw >> 88_usize) & WORD_160_MASK,
            initialized: ((raw >> 248_usize) & U256::from(1)).to::<u8>() != 0,
        }
    }

    fn encode(self) -> U256 {
        U256::from(self.timestamp)
            | (unsigned_bits(self.tick_cumulative, 56) << 32_usize)
            | ((self.seconds_per_liquidity_cumulative_x128 & WORD_160_MASK) << 88_usize)
            | (U256::from(self.initialized as u8) << 248_usize)
    }

    fn transform(self, timestamp: u32, tick: i32, liquidity: U256) -> Self {
        let delta = timestamp.wrapping_sub(self.timestamp);
        let cumulative = wrapping_signed_add(
            self.tick_cumulative,
            i64::from(tick).wrapping_mul(i64::from(delta)),
            56,
        );
        let denominator = if liquidity.is_zero() {
            U256::from(1)
        } else {
            liquidity
        };
        let increment = (U256::from(delta) << 128_usize) / denominator;
        Self {
            timestamp,
            tick_cumulative: cumulative,
            seconds_per_liquidity_cumulative_x128: self
                .seconds_per_liquidity_cumulative_x128
                .wrapping_add(increment)
                & WORD_160_MASK,
            initialized: true,
        }
    }
}

type OracleAdvance = (u16, u16, Option<(U256, U256)>, Observation);

#[derive(Clone, Debug)]
struct InitializedTick {
    keys: [U256; 4],
    words: [U256; 4],
    liquidity_net: i128,
}

#[derive(Clone, Copy, Debug)]
struct Segment {
    liquidity: U256,
    amount_in: U256,
    amount_out: U256,
    reached_boundary: bool,
    crossed_tick: Option<usize>,
}

#[derive(Clone, Debug)]
struct StepBoundary {
    tick: i32,
    initialized: bool,
}

#[derive(Clone, Debug)]
struct SlipstreamInitializedTick {
    keys: [U256; 6],
    words: [U256; 6],
    liquidity_net: i128,
    staked_liquidity_net: i128,
}

#[derive(Clone, Copy, Debug)]
struct SlipstreamSegment {
    liquidity: U256,
    staked_liquidity: U256,
    amount_in: U256,
    amount_out: U256,
    reached_boundary: bool,
    crossed_tick: Option<usize>,
}

#[derive(Clone, Debug)]
struct SlipstreamStepBoundary {
    tick: i32,
    initialized: bool,
}

#[derive(Clone, Copy, Debug)]
struct ReviewedSlipstreamRuntime {
    chain_id: u64,
    pool: Address,
    factory: Address,
    proxy_runtime_code_hash: alloy_primitives::B256,
    implementation: Address,
    implementation_runtime_code_hash: alloy_primitives::B256,
}

/// Derive the complete canonical Uniswap V3 swap-induced update batch.
pub(super) fn derive_uniswap_v3_swap(
    address: Address,
    layout: V3StorageLayout,
    fee: u32,
    swap: DecodedSwap,
    state: &dyn StateView,
    context: &AdapterEventContext,
) -> Result<Vec<StateUpdate>, AdapterEventError> {
    let surface = UNISWAP_V3_SURFACE;
    validate_context(context)?;
    if layout != V3StorageLayout::uniswap(layout.tick_spacing) {
        return Err(contradiction(
            "exact Uniswap V3 transition requires the canonical storage layout",
        ));
    }
    if fee >= FEE_DENOMINATOR as u32 {
        return Err(contradiction("invalid Uniswap V3 fee"));
    }
    if layout.tick_spacing <= 0 {
        return Err(contradiction("Uniswap V3 tick spacing must be positive"));
    }
    if swap.sqrt_price_x96 > SLOT0_SQRT_MASK || swap.liquidity > WORD_128_MASK {
        return Err(AdapterEventError::MalformedLog(
            "V3 Swap final value exceeds its ABI width",
        ));
    }

    let slot0_raw = required(state, address, layout.slot0_slot)?;
    let slot0 = Slot0::decode(slot0_raw);
    if ((slot0.raw >> 240_usize) & U256::from(u8::MAX)).to::<u8>() != 1 {
        return Err(contradiction("parent slot0 is locked"));
    }
    validate_parent_slot0(slot0)?;
    let start_liquidity = required(state, address, layout.liquidity_slot)?;
    if start_liquidity > WORD_128_MASK {
        return Err(contradiction("parent V3 liquidity exceeds uint128"));
    }
    let mut fee_growth_0 = required(state, address, surface.fee_growth_0_slot)?;
    let mut fee_growth_1 = required(state, address, surface.fee_growth_1_slot)?;
    let mut protocol_fees = required(state, address, surface.protocol_fees_slot)?;

    let zero_for_one = match (
        swap.amount0_negative,
        swap.amount1_negative,
        swap.amount0.is_zero(),
        swap.amount1.is_zero(),
    ) {
        (false, true, false, false) => true,
        (true, false, false, false) => false,
        (false, false, false, true) => true,
        (false, false, true, false) => false,
        _ => {
            return Err(contradiction(
                "swap must contain one positive input and one negative output",
            ));
        }
    };
    let actual_output_is_zero = if zero_for_one {
        swap.amount1.is_zero()
    } else {
        swap.amount0.is_zero()
    };
    let unchanged_tiny_swap = actual_output_is_zero && swap.sqrt_price_x96 == slot0.sqrt_price_x96;
    if !unchanged_tiny_swap
        && ((zero_for_one && swap.sqrt_price_x96 >= slot0.sqrt_price_x96)
            || (!zero_for_one && swap.sqrt_price_x96 <= slot0.sqrt_price_x96))
    {
        return Err(contradiction("swap direction contradicts its final price"));
    }
    validate_final_tick(swap.sqrt_price_x96, swap.tick, zero_for_one)?;

    let (oracle_index, oracle_cardinality, oracle_write, oracle_now) = if slot0.tick != swap.tick {
        advance_oracle(address, surface, slot0, start_liquidity, state, context)?
    } else {
        (
            slot0.observation_index,
            slot0.observation_cardinality,
            None,
            Observation {
                timestamp: context.block_timestamp.expect("validated") as u32,
                tick_cumulative: 0,
                seconds_per_liquidity_cumulative_x128: U256::ZERO,
                initialized: true,
            },
        )
    };

    let mut ticks = Vec::new();
    let mut segments = Vec::new();
    let mut current_sqrt = slot0.sqrt_price_x96;
    let mut current_tick = slot0.tick;
    let mut current_liquidity = start_liquidity;
    let mut steps = 0_u32;
    // A zero-for-one swap can stand exactly on an uninitialized boundary,
    // traverse that zero-distance canonical step, and change `tick` (and, for
    // an initialized boundary, liquidity) without changing sqrtPriceX96. The
    // deployed-bytecode corpus includes the amountSpecified=1 case that takes
    // this branch. Replay the boundary based on parent geometry, not the event's
    // claimed tick, so a contradictory event cannot suppress it.
    if zero_for_one && current_sqrt == swap.sqrt_price_x96 {
        let boundary = next_step_boundary(address, layout, current_tick, true, state)?;
        let boundary_sqrt = sqrt_ratio_at_tick(boundary.tick)?;
        if boundary_sqrt == current_sqrt {
            consume_step(&mut steps)?;
            let crossed_tick = boundary
                .initialized
                .then(|| {
                    let initialized = load_initialized_tick(address, layout, boundary.tick, state)?;
                    let index = ticks.len();
                    ticks.push(initialized);
                    Ok::<_, AdapterEventError>(index)
                })
                .transpose()?;
            segments.push(Segment {
                liquidity: current_liquidity,
                amount_in: U256::ZERO,
                amount_out: U256::ZERO,
                reached_boundary: true,
                crossed_tick,
            });
            if let Some(index) = crossed_tick {
                current_liquidity =
                    apply_liquidity_net(current_liquidity, ticks[index].liquidity_net, true)?;
            }
            current_tick = boundary.tick - 1;
        }
    }
    while current_sqrt != swap.sqrt_price_x96 {
        consume_step(&mut steps)?;
        let boundary = next_step_boundary(address, layout, current_tick, zero_for_one, state)?;
        let boundary_sqrt = sqrt_ratio_at_tick(boundary.tick)?;
        let target = if zero_for_one {
            boundary_sqrt.max(swap.sqrt_price_x96)
        } else {
            boundary_sqrt.min(swap.sqrt_price_x96)
        };
        let reached_boundary = target == boundary_sqrt;
        let (amount_in, amount_out) =
            segment_amounts(current_sqrt, target, current_liquidity, zero_for_one)?;
        let crossed_tick = if reached_boundary && boundary.initialized {
            Some({
                let initialized = load_initialized_tick(address, layout, boundary.tick, state)?;
                let index = ticks.len();
                ticks.push(initialized);
                index
            })
        } else {
            None
        };
        segments.push(Segment {
            liquidity: current_liquidity,
            amount_in,
            amount_out,
            reached_boundary,
            crossed_tick,
        });
        current_sqrt = target;
        if reached_boundary {
            if let Some(index) = crossed_tick {
                current_liquidity = apply_liquidity_net(
                    current_liquidity,
                    ticks[index].liquidity_net,
                    zero_for_one,
                )?;
            }
            current_tick = if zero_for_one {
                boundary.tick - 1
            } else {
                boundary.tick
            };
        } else {
            // `validate_final_tick` proves the event tick is the canonical
            // TickMath interval containing this partial-step final price.
            current_tick = swap.tick;
        }
    }
    if segments.is_empty() {
        if unchanged_tiny_swap {
            segments.push(Segment {
                liquidity: current_liquidity,
                amount_in: U256::ZERO,
                amount_out: U256::ZERO,
                reached_boundary: false,
                crossed_tick: None,
            });
        } else {
            return Err(contradiction("swap did not move the square-root price"));
        }
    }
    if current_liquidity != swap.liquidity {
        return Err(final_mismatch(
            "liquidity",
            current_liquidity,
            swap.liquidity,
        ));
    }
    if current_tick != swap.tick {
        return Err(final_mismatch(
            "tick",
            U256::from((current_tick as u32) & 0x00ff_ffff),
            U256::from((swap.tick as u32) & 0x00ff_ffff),
        ));
    }
    let actual_input = if zero_for_one {
        swap.amount0
    } else {
        swap.amount1
    };
    let actual_output = if zero_for_one {
        swap.amount1
    } else {
        swap.amount0
    };
    let principal_input = checked_sum(segments.iter().map(|segment| segment.amount_in))?;
    let derived_output = checked_sum(segments.iter().map(|segment| segment.amount_out))?;
    if derived_output != actual_output {
        return Err(final_mismatch(
            "signed output",
            derived_output,
            actual_output,
        ));
    }
    let total_fee = actual_input
        .checked_sub(principal_input)
        .ok_or_else(|| final_mismatch("signed input principal", principal_input, actual_input))?;

    // Exact-input can leave a tiny remainder after reaching a step target. If
    // its after-fee amount rounds to zero, canonical SwapMath consumes it as a
    // final fee-only partial step without moving price. Preserve that distinct
    // step so fee-growth rounding and protocol allocation remain exact.
    if segments
        .last()
        .is_some_and(|segment| segment.reached_boundary)
    {
        let full_step_fees = checked_sum(
            segments
                .iter()
                .map(|segment| fee_for_full_step(segment.amount_in, fee))
                .collect::<Result<Vec<_>, _>>()?,
        )?;
        if let Some(residual) = total_fee.checked_sub(full_step_fees)
            && !residual.is_zero()
            && valid_partial_fee(U256::ZERO, residual, fee)?
        {
            segments.push(Segment {
                liquidity: current_liquidity,
                amount_in: U256::ZERO,
                amount_out: U256::ZERO,
                reached_boundary: false,
                crossed_tick: None,
            });
        }
    }

    let mut remaining_fee = total_fee;
    let protocol_divisor = if zero_for_one {
        slot0.fee_protocol & 0x0f
    } else {
        slot0.fee_protocol >> 4
    };
    let timestamp = context.block_timestamp.expect("validated") as u32;
    let mut tick_updates = Vec::with_capacity(ticks.len() * 4);
    for (segment_index, segment) in segments.iter().enumerate() {
        let is_partial_final = segment_index + 1 == segments.len() && !segment.reached_boundary;
        let fee_amount = if is_partial_final {
            if !valid_partial_fee(segment.amount_in, remaining_fee, fee)? {
                return Err(contradiction(
                    "final-step fee does not match exact-input or exact-output semantics",
                ));
            }
            remaining_fee
        } else {
            fee_for_full_step(segment.amount_in, fee)?
        };
        remaining_fee = remaining_fee
            .checked_sub(fee_amount)
            .ok_or_else(|| contradiction("fee allocation contradicts event input"))?;
        let protocol_fee = if protocol_divisor == 0 {
            U256::ZERO
        } else {
            fee_amount / U256::from(protocol_divisor)
        };
        let lp_fee = fee_amount - protocol_fee;
        let growth = if segment.liquidity.is_zero() {
            if !lp_fee.is_zero() {
                return Err(contradiction(
                    "a zero-liquidity step cannot accrue an LP fee",
                ));
            }
            U256::ZERO
        } else {
            mul_div(lp_fee, Q128, segment.liquidity)?
        };
        if zero_for_one {
            fee_growth_0 = fee_growth_0.wrapping_add(growth);
            // Canonical Uniswap V3 is Solidity 0.7.6: both the per-swap
            // uint128 cast and the final uint128 accumulator addition wrap.
            let token0 = (protocol_fees & WORD_128_MASK).wrapping_add(protocol_fee & WORD_128_MASK)
                & WORD_128_MASK;
            protocol_fees = (protocol_fees & (WORD_128_MASK << 128)) | token0;
        } else {
            fee_growth_1 = fee_growth_1.wrapping_add(growth);
            let token1 = ((protocol_fees >> 128_usize) & WORD_128_MASK)
                .wrapping_add(protocol_fee & WORD_128_MASK)
                & WORD_128_MASK;
            protocol_fees = (protocol_fees & WORD_128_MASK) | (token1 << 128_usize);
        }

        if let Some(tick_index) = segment.crossed_tick {
            let initialized = &mut ticks[tick_index];
            initialized.words[1] = fee_growth_0.wrapping_sub(initialized.words[1]);
            initialized.words[2] = fee_growth_1.wrapping_sub(initialized.words[2]);
            initialized.words[3] = cross_tick_word(
                initialized.words[3],
                oracle_now.tick_cumulative,
                oracle_now.seconds_per_liquidity_cumulative_x128,
                timestamp,
            );
            tick_updates.extend(
                initialized
                    .keys
                    .iter()
                    .zip(initialized.words)
                    .map(|(slot, value)| StateUpdate::slot(address, *slot, value)),
            );
        }
    }
    if !remaining_fee.is_zero() {
        return Err(contradiction(
            "fee allocation left an unexplained remainder",
        ));
    }

    let final_slot0 = slot0.encode_final(
        swap.sqrt_price_x96,
        swap.tick,
        oracle_index,
        oracle_cardinality,
    );
    let mut updates = vec![
        StateUpdate::slot(address, layout.slot0_slot, final_slot0),
        StateUpdate::slot(address, surface.fee_growth_0_slot, fee_growth_0),
        StateUpdate::slot(address, surface.fee_growth_1_slot, fee_growth_1),
        StateUpdate::slot(address, surface.protocol_fees_slot, protocol_fees),
        StateUpdate::slot(address, layout.liquidity_slot, current_liquidity),
    ];
    if let Some((slot, value)) = oracle_write {
        updates.push(StateUpdate::slot(address, slot, value));
    }
    updates.extend(tick_updates);
    Ok(updates)
}

/// One canonical `Tick.Info` boundary update: the four storage keys it
/// occupies, the words to write, and how its initialization state changed.
#[derive(Clone, Copy, Debug)]
struct CanonicalTickUpdate {
    keys: [U256; 4],
    words: [U256; 4],
    was_initialized: bool,
    is_initialized: bool,
}

/// Replay canonical `Tick.update` — and `Tick.clear` when the tick empties —
/// for one boundary tick of a `Mint`/`Burn`.
///
/// The event carries only the liquidity delta, so the accumulator writes come
/// from the exact parent instead: on first initialization Uniswap assumes all
/// growth so far happened *below* the tick, which writes the two global fee
/// accumulators and the current oracle reading into the outside words — but
/// only for a tick at or below the current one. A tick above the current one is
/// initialized with nothing but its flag. Getting that asymmetry wrong is
/// invisible to a quote and corrupts every later `getFeeGrowthInside`.
#[allow(clippy::too_many_arguments)]
fn update_canonical_liquidity_tick(
    address: Address,
    layout: V3StorageLayout,
    tick: i32,
    current_tick: i32,
    amount: i128,
    is_mint: bool,
    upper: bool,
    bitmap_indicates_initialized: bool,
    max_liquidity_per_tick: U256,
    fee_growth_0: U256,
    fee_growth_1: U256,
    oracle_now: Observation,
    timestamp: u32,
    state: &dyn StateView,
) -> Result<CanonicalTickUpdate, AdapterEventError> {
    let keys = v3_tick_info_storage_keys_with_base(tick, layout.ticks_base_slot);
    let mut words = [U256::ZERO; 4];
    // A tick is initialized exactly when its bitmap bit is set: `flipTick` is
    // called precisely on a flip, and `Tick.clear` zeroes the whole struct. So a
    // clear bit *proves* all four words are zero and they need not be read at
    // all -- which is what lets a mint opening a brand-new position stay fully
    // offline, since cold start warms bitmap words but not empty ticks.
    if bitmap_indicates_initialized {
        for (word, key) in words.iter_mut().zip(keys) {
            *word = required(state, address, key)?;
        }
    } else if state
        .storage(address, keys[0])
        .is_some_and(|word| !(word & WORD_128_MASK).is_zero())
    {
        // Skipping the read means a clear bit over a live tick would otherwise
        // pass silently. It costs nothing to catch when the parent happens to
        // hold the word regardless.
        return Err(AdapterEventError::V3Transition(
            V3TransitionError::InitializedTick {
                tick,
                reason: "bitmap bit is clear for a tick holding liquidity",
            },
        ));
    }
    let old_gross = words[0] & WORD_128_MASK;
    let old_net = ((words[0] >> 128_usize) & WORD_128_MASK).to::<u128>() as i128;
    let was_initialized = !old_gross.is_zero();
    let initialized_flag = ((words[3] >> 248_usize) & U256::from(u8::MAX)).to::<u8>() == 1;
    if was_initialized != initialized_flag {
        return Err(AdapterEventError::V3Transition(
            V3TransitionError::InitializedTick {
                tick,
                reason: "Tick.Info initialized flag disagrees with liquidityGross",
            },
        ));
    }
    // `Tick.clear` zeroes the whole struct, so a tick that is uninitialized in
    // the parent cannot carry residue. Nonzero residue means the parent was
    // assembled from something other than canonical history.
    if !was_initialized && words.iter().any(|word| !word.is_zero()) {
        return Err(AdapterEventError::V3Transition(
            V3TransitionError::InitializedTick {
                tick,
                reason: "uninitialized Tick.Info contains nonzero state",
            },
        ));
    }
    let amount_u256 = U256::from(amount as u128);
    let new_gross = if is_mint {
        old_gross
            .checked_add(amount_u256)
            .ok_or_else(|| arithmetic("tick liquidityGross overflow"))?
    } else {
        old_gross
            .checked_sub(amount_u256)
            .ok_or_else(|| arithmetic("tick liquidityGross underflow"))?
    };
    if new_gross > max_liquidity_per_tick {
        return Err(arithmetic(
            "tick liquidityGross exceeds maxLiquidityPerTick",
        ));
    }
    let delta = if is_mint { amount } else { -amount };
    let net_delta = if upper { -delta } else { delta };
    let new_net = old_net
        .checked_add(net_delta)
        .ok_or_else(|| arithmetic("tick liquidityNet overflow"))?;
    let is_initialized = !new_gross.is_zero();

    if !was_initialized && is_initialized {
        if tick <= current_tick {
            words[1] = fee_growth_0;
            words[2] = fee_growth_1;
            words[3] = unsigned_bits(oracle_now.tick_cumulative, 56)
                | ((oracle_now.seconds_per_liquidity_cumulative_x128 & WORD_160_MASK) << 56_usize)
                | (U256::from(timestamp) << 216_usize)
                | (U256::from(1) << 248_usize);
        } else {
            words[3] = U256::from(1) << 248_usize;
        }
    }
    words[0] = new_gross | (U256::from(new_net as u128) << 128_usize);
    if !is_initialized {
        words = [U256::ZERO; 4];
    }
    Ok(CanonicalTickUpdate {
        keys,
        words,
        was_initialized,
        is_initialized,
    })
}

/// Derive the complete canonical Uniswap V3 `Mint`/`Burn` transition.
///
/// This reproduces `_modifyPosition` over the pool's pricing surface: both
/// boundary ticks through `Tick.update`/`Tick.clear`, the bitmap words they
/// flip, the oracle entry an in-range change writes, and active liquidity.
///
/// Position ownership and tokens-owed accounting sit outside the adapter's
/// declared search surface — `swap` never reads `positions` — so they are not
/// reconstructed here. The caller invalidates those slots rather than leaving a
/// warm one stale. Missing parent cells fail closed; an absent cache value is
/// never read as an EVM zero.
pub(super) struct CanonicalLiquidityTransition {
    /// Writes derived exactly from the parent. Always safe to apply.
    pub updates: Vec<StateUpdate>,
    /// Boundary cells the parent could not supply, so their post-event values
    /// are unknown. Empty means the transition is exact. Otherwise these — and
    /// only these — must be dropped and re-read authoritatively; the rest of the
    /// pool stays exact and quotable.
    pub cold_slots: Vec<U256>,
}

pub(super) fn derive_uniswap_v3_liquidity(
    address: Address,
    layout: V3StorageLayout,
    event: DecodedLiquidity,
    state: &dyn StateView,
    context: &AdapterEventContext,
) -> Result<CanonicalLiquidityTransition, AdapterEventError> {
    let surface = UNISWAP_V3_SURFACE;
    validate_context(context)?;
    if layout != V3StorageLayout::uniswap(layout.tick_spacing) {
        return Err(contradiction(
            "exact Uniswap V3 transition requires the canonical storage layout",
        ));
    }
    if layout.tick_spacing <= 0 {
        return Err(contradiction("Uniswap V3 tick spacing must be positive"));
    }
    // `checkTicks` bounds the range; `tickBitmap.flipTick` rejects a tick that
    // is not spacing-aligned, and a tick can only become initialized through a
    // flip, so every real boundary tick is aligned.
    if event.tick_lower < -887_272
        || event.tick_upper > 887_272
        || event.tick_lower >= event.tick_upper
        || event.tick_lower.rem_euclid(layout.tick_spacing) != 0
        || event.tick_upper.rem_euclid(layout.tick_spacing) != 0
    {
        return Err(contradiction(
            "Uniswap V3 liquidity range is invalid or not spacing-aligned",
        ));
    }
    if event.amount == 0 {
        if event.is_mint {
            return Err(contradiction("Uniswap V3 Mint liquidity must be positive"));
        }
        // A zero-amount Burn is a position fee poke: `_modifyPosition` guards
        // every tick, bitmap, oracle, and liquidity write on a nonzero delta,
        // so only position accounting moves.
        return Ok(CanonicalLiquidityTransition {
            updates: Vec::new(),
            cold_slots: Vec::new(),
        });
    }
    let signed_amount =
        i128::try_from(event.amount).map_err(|_| arithmetic("liquidity amount exceeds int128"))?;

    let slot0_raw = required(state, address, layout.slot0_slot)?;
    let slot0 = Slot0::decode(slot0_raw);
    if ((slot0.raw >> 240_usize) & U256::from(u8::MAX)).to::<u8>() != 1 {
        return Err(contradiction("parent slot0 is locked"));
    }
    validate_parent_slot0(slot0)?;
    let active_liquidity = required(state, address, layout.liquidity_slot)?;
    if active_liquidity > WORD_128_MASK {
        return Err(contradiction("parent V3 liquidity exceeds uint128"));
    }
    // Canonical Uniswap holds `maxLiquidityPerTick` as a constructor immutable
    // in the runtime, not in storage, so it is derived from tick spacing by the
    // same formula the deployed bytecode was built with.
    let max_liquidity_per_tick = super::uniswap_v3_max_liquidity_per_tick(layout.tick_spacing)
        .ok_or_else(|| contradiction("no canonical maxLiquidityPerTick for this tick spacing"))?;

    // `_updatePosition` reads the oracle for *any* nonzero delta, in range or
    // not, because a newly initialized tick at or below the current one seeds
    // its outside accumulators from it. The resulting ring write is applied
    // only in range.
    let (observation_index, observation_cardinality, oracle_write, oracle_now) =
        advance_oracle(address, surface, slot0, active_liquidity, state, context)?;
    let fee_growth_0 = required(state, address, surface.fee_growth_0_slot)?;
    let fee_growth_1 = required(state, address, surface.fee_growth_1_slot)?;
    let timestamp = context.block_timestamp.expect("validated") as u32;

    // The bitmap is loaded first because it decides whether each boundary tick
    // has to be read at all. Both ticks can share a word, and `flipTick` XORs,
    // so flips are merged per word rather than written twice.
    //
    // Every cell read above is mandatory cold-start state and is warm on any
    // usable pool. A boundary tick, by contrast, can sit outside the warmed
    // bitmap radius — an LP is free to open a position anywhere. That case is
    // resolved per boundary rather than failing the whole event, so a distant
    // mint costs a handful of slots instead of the pool's entire storage.
    let mut bitmap_values = std::collections::BTreeMap::<U256, Option<U256>>::new();
    let mut boundary_positions = Vec::with_capacity(2);
    for (tick, upper) in [(event.tick_lower, false), (event.tick_upper, true)] {
        let word = v3_word_position(tick, layout.tick_spacing);
        let slot = v3_tick_bitmap_storage_key_with_base(word, layout.tick_bitmap_base_slot);
        let value = *bitmap_values
            .entry(slot)
            .or_insert_with(|| state.storage(address, slot));
        let mask = U256::from(1) << (tick.div_euclid(layout.tick_spacing).rem_euclid(256) as usize);
        boundary_positions.push((tick, upper, slot, mask, value));
    }

    let mut boundaries = Vec::with_capacity(2);
    for (tick, upper, _, mask, bitmap) in &boundary_positions {
        let Some(bitmap) = bitmap else {
            boundaries.push(None);
            continue;
        };
        let was_initialized = !(*bitmap & *mask).is_zero();
        match update_canonical_liquidity_tick(
            address,
            layout,
            *tick,
            slot0.tick,
            signed_amount,
            event.is_mint,
            *upper,
            was_initialized,
            max_liquidity_per_tick,
            fee_growth_0,
            fee_growth_1,
            oracle_now,
            timestamp,
            state,
        ) {
            Ok(update) => boundaries.push(Some(update)),
            // An absent tick word is unknown state, not bad state. Anything
            // else means the parent contradicts canonical history and must
            // fail closed for the whole pool.
            Err(AdapterEventError::MissingState { .. }) => boundaries.push(None),
            Err(error) => return Err(error),
        }
    }

    let mut cold_slots = Vec::new();
    let mut updates = Vec::with_capacity(16);
    for (boundary, (tick, _, bitmap_slot, _, _)) in boundaries.iter().zip(&boundary_positions) {
        match boundary {
            Some(update) => updates.extend(
                update
                    .keys
                    .into_iter()
                    .zip(update.words)
                    .map(|(slot, value)| StateUpdate::slot(address, slot, value)),
            ),
            None => {
                cold_slots.extend(v3_tick_info_storage_keys_with_base(
                    *tick,
                    layout.ticks_base_slot,
                ));
                // Whether an unresolved boundary flips its bit is unknown, so
                // the word it lives in is unknown too.
                cold_slots.push(*bitmap_slot);
            }
        }
    }

    let mut changed_bitmaps = std::collections::BTreeSet::<U256>::new();
    for (boundary, (tick, _, slot, mask, _)) in boundaries.iter().zip(&boundary_positions) {
        let Some(update) = boundary else {
            continue;
        };
        let bitmap = bitmap_values
            .get_mut(slot)
            .expect("seeded above")
            .as_mut()
            .expect("a resolved boundary read its bitmap word");
        // Compared against the tick's own `liquidityGross`, not against the bit
        // the read was gated on. A set bit over an empty tick means the parent
        // was assembled from something other than canonical history, and the
        // empty-tick inference above would then be unsound.
        let bitmap_is_set = !(*bitmap & *mask).is_zero();
        if bitmap_is_set != update.was_initialized {
            return Err(AdapterEventError::V3Transition(
                V3TransitionError::InitializedTick {
                    tick: *tick,
                    reason: "bitmap disagrees with parent Tick.Info initialization",
                },
            ));
        }
        if update.was_initialized != update.is_initialized {
            *bitmap ^= *mask;
            changed_bitmaps.insert(*slot);
        }
    }
    // A word shared with an unresolved boundary cannot be written: the other
    // boundary's flip would be missing from the merged value.
    let cold_bitmaps: std::collections::BTreeSet<U256> = cold_slots.iter().copied().collect();
    updates.extend(
        bitmap_values
            .into_iter()
            .filter(|(slot, _)| changed_bitmaps.contains(slot) && !cold_bitmaps.contains(slot))
            .filter_map(|(slot, value)| value.map(|value| StateUpdate::slot(address, slot, value))),
    );

    if slot0.tick >= event.tick_lower && slot0.tick < event.tick_upper {
        let next_liquidity = if event.is_mint {
            active_liquidity
                .checked_add(U256::from(event.amount))
                .ok_or_else(|| arithmetic("active liquidity overflow"))?
        } else {
            active_liquidity
                .checked_sub(U256::from(event.amount))
                .ok_or_else(|| arithmetic("active liquidity underflow"))?
        };
        if next_liquidity > WORD_128_MASK {
            return Err(arithmetic("active liquidity exceeds uint128"));
        }
        if let Some((slot, value)) = oracle_write {
            updates.push(StateUpdate::slot(address, slot, value));
            updates.push(StateUpdate::slot(
                address,
                layout.slot0_slot,
                slot0.encode_final(
                    slot0.sqrt_price_x96,
                    slot0.tick,
                    observation_index,
                    observation_cardinality,
                ),
            ));
        }
        updates.push(StateUpdate::slot(
            address,
            layout.liquidity_slot,
            next_liquidity,
        ));
    }
    cold_slots.sort_unstable();
    cold_slots.dedup();
    Ok(CanonicalLiquidityTransition {
        updates,
        cold_slots,
    })
}

#[derive(Clone, Copy, Debug)]
pub(super) struct DecodedFlash {
    pub amount0: U256,
    pub amount1: U256,
    pub paid0: U256,
    pub paid1: U256,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct DecodedFeeProtocol {
    pub old0: u8,
    pub old1: u8,
    pub new0: u8,
    pub new1: u8,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct DecodedCollectProtocol {
    pub amount0: U256,
    pub amount1: U256,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct DecodedObservationGrowth {
    pub old: u16,
    pub new: u16,
}

/// Common preamble for the canonical accounting transitions: ordered context and
/// a provably canonical storage layout.
fn validate_canonical_accounting(
    layout: V3StorageLayout,
    context: &AdapterEventContext,
) -> Result<(), AdapterEventError> {
    validate_context(context)?;
    if layout != V3StorageLayout::uniswap(layout.tick_spacing) {
        return Err(contradiction(
            "exact Uniswap V3 transition requires the canonical storage layout",
        ));
    }
    if layout.tick_spacing <= 0 {
        return Err(contradiction("Uniswap V3 tick spacing must be positive"));
    }
    Ok(())
}

/// Derive the complete canonical Uniswap V3 `Flash` transition.
///
/// A flash loan moves no price and no liquidity: it only credits the fee it was
/// paid. The event carries the amounts actually paid, and the split between LP
/// growth and protocol fees follows from `slot0.feeProtocol` and active
/// liquidity, so nothing has to be read back from the chain.
pub(super) fn derive_uniswap_v3_flash(
    address: Address,
    layout: V3StorageLayout,
    fee: u32,
    flash: DecodedFlash,
    state: &dyn StateView,
    context: &AdapterEventContext,
) -> Result<Vec<StateUpdate>, AdapterEventError> {
    let surface = UNISWAP_V3_SURFACE;
    validate_canonical_accounting(layout, context)?;
    if fee >= FEE_DENOMINATOR as u32 {
        return Err(contradiction("invalid Uniswap V3 fee"));
    }
    let slot0 = Slot0::decode(required(state, address, layout.slot0_slot)?);
    let liquidity = required(state, address, layout.liquidity_slot)?;
    if liquidity.is_zero() {
        return Err(contradiction("flash requires nonzero active liquidity"));
    }
    if liquidity > WORD_128_MASK {
        return Err(contradiction("parent V3 liquidity exceeds uint128"));
    }
    let mut fee_growth_0 = required(state, address, surface.fee_growth_0_slot)?;
    let mut fee_growth_1 = required(state, address, surface.fee_growth_1_slot)?;
    let mut protocol_fees = required(state, address, surface.protocol_fees_slot)?;

    // `flash` reverts unless it is repaid at least the quoted fee, so a payment
    // below it cannot describe a canonical flash on this pool.
    let denominator = U256::from(FEE_DENOMINATOR);
    let quoted_fee_0 = mul_div_round_up(flash.amount0, U256::from(fee), denominator)?;
    let quoted_fee_1 = mul_div_round_up(flash.amount1, U256::from(fee), denominator)?;
    if flash.paid0 < quoted_fee_0 {
        return Err(final_mismatch("flash paid0", flash.paid0, quoted_fee_0));
    }
    if flash.paid1 < quoted_fee_1 {
        return Err(final_mismatch("flash paid1", flash.paid1, quoted_fee_1));
    }

    for (paid, protocol_share, is_token0) in [
        (flash.paid0, slot0.fee_protocol % 16, true),
        (flash.paid1, slot0.fee_protocol >> 4, false),
    ] {
        if paid.is_zero() {
            continue;
        }
        let protocol_fee = if protocol_share == 0 {
            U256::ZERO
        } else {
            paid / U256::from(protocol_share)
        };
        // Canonical Uniswap V3 is Solidity 0.7.6: the uint128 cast truncates and
        // the accumulator addition wraps.
        let truncated = protocol_fee & WORD_128_MASK;
        if !truncated.is_zero() {
            if is_token0 {
                let token0 =
                    (protocol_fees & WORD_128_MASK).wrapping_add(truncated) & WORD_128_MASK;
                protocol_fees = (protocol_fees & (WORD_128_MASK << 128_usize)) | token0;
            } else {
                let token1 = ((protocol_fees >> 128_usize) & WORD_128_MASK).wrapping_add(truncated)
                    & WORD_128_MASK;
                protocol_fees = (protocol_fees & WORD_128_MASK) | (token1 << 128_usize);
            }
        }
        let lp_share = paid
            .checked_sub(protocol_fee)
            .ok_or_else(|| arithmetic("flash protocol fee exceeds the amount paid"))?;
        let growth = mul_div(lp_share, Q128, liquidity)?;
        if is_token0 {
            fee_growth_0 = fee_growth_0.wrapping_add(growth);
        } else {
            fee_growth_1 = fee_growth_1.wrapping_add(growth);
        }
    }

    Ok(vec![
        StateUpdate::slot(address, surface.fee_growth_0_slot, fee_growth_0),
        StateUpdate::slot(address, surface.fee_growth_1_slot, fee_growth_1),
        StateUpdate::slot(address, surface.protocol_fees_slot, protocol_fees),
    ])
}

/// Derive the complete canonical Uniswap V3 `SetFeeProtocol` transition.
///
/// The event carries both the old and the new split, so the parent's own byte is
/// a checkable postcondition rather than an assumption.
pub(super) fn derive_uniswap_v3_set_fee_protocol(
    address: Address,
    layout: V3StorageLayout,
    event: DecodedFeeProtocol,
    state: &dyn StateView,
    context: &AdapterEventContext,
) -> Result<Vec<StateUpdate>, AdapterEventError> {
    validate_canonical_accounting(layout, context)?;
    // `setFeeProtocol` accepts only zero or a denominator in 4..=10, and packs
    // the pair into one byte, so each half must also fit in a nibble.
    for value in [event.new0, event.new1] {
        if value != 0 && !(4..=10).contains(&value) {
            return Err(contradiction(
                "fee protocol denominator outside the canonical range",
            ));
        }
    }
    let slot0_raw = required(state, address, layout.slot0_slot)?;
    let current = ((slot0_raw >> 232_usize) & U256::from(u8::MAX)).to::<u8>();
    let expected = event.old0 | (event.old1 << 4);
    if current != expected {
        return Err(final_mismatch(
            "slot0 feeProtocol",
            U256::from(current),
            U256::from(expected),
        ));
    }
    let packed = event.new0 | (event.new1 << 4);
    let mask = U256::from(u8::MAX) << 232_usize;
    Ok(vec![StateUpdate::slot(
        address,
        layout.slot0_slot,
        (slot0_raw & !mask) | (U256::from(packed) << 232_usize),
    )])
}

/// Derive the complete canonical Uniswap V3 `CollectProtocol` transition.
///
/// The event reports the amounts actually transferred, including the deliberate
/// one-wei remainder canonical Uniswap leaves behind to keep the slot warm, so
/// the new accumulator is an exact subtraction.
pub(super) fn derive_uniswap_v3_collect_protocol(
    address: Address,
    layout: V3StorageLayout,
    event: DecodedCollectProtocol,
    state: &dyn StateView,
    context: &AdapterEventContext,
) -> Result<Vec<StateUpdate>, AdapterEventError> {
    let surface = UNISWAP_V3_SURFACE;
    validate_canonical_accounting(layout, context)?;
    if event.amount0 > WORD_128_MASK || event.amount1 > WORD_128_MASK {
        return Err(AdapterEventError::MalformedLog(
            "CollectProtocol amount exceeds its ABI width",
        ));
    }
    let protocol_fees = required(state, address, surface.protocol_fees_slot)?;
    let token0 = (protocol_fees & WORD_128_MASK)
        .checked_sub(event.amount0)
        .ok_or_else(|| arithmetic("collected protocol fee exceeds the accrued token0 balance"))?;
    let token1 = ((protocol_fees >> 128_usize) & WORD_128_MASK)
        .checked_sub(event.amount1)
        .ok_or_else(|| arithmetic("collected protocol fee exceeds the accrued token1 balance"))?;
    Ok(vec![StateUpdate::slot(
        address,
        surface.protocol_fees_slot,
        token0 | (token1 << 128_usize),
    )])
}

/// Derive the complete canonical Uniswap V3
/// `IncreaseObservationCardinalityNext` transition.
///
/// `Oracle.grow` stamps `blockTimestamp = 1` across every newly reserved slot to
/// pay their storage cost up front. Those slots need no read: `write` never
/// reaches an index at or above `observationCardinalityNext`, and a previous
/// `grow` stopped exactly at the parent's value, so the whole written range is
/// provably untouched and the resulting word is exactly one.
pub(super) fn derive_uniswap_v3_observation_growth(
    address: Address,
    layout: V3StorageLayout,
    event: DecodedObservationGrowth,
    state: &dyn StateView,
    context: &AdapterEventContext,
) -> Result<Vec<StateUpdate>, AdapterEventError> {
    let surface = UNISWAP_V3_SURFACE;
    validate_canonical_accounting(layout, context)?;
    if event.new <= event.old {
        return Err(contradiction(
            "observation cardinality growth must increase the reservation",
        ));
    }
    let slot0_raw = required(state, address, layout.slot0_slot)?;
    let slot0 = Slot0::decode(slot0_raw);
    if slot0.observation_cardinality == 0 {
        return Err(AdapterEventError::V3Transition(
            V3TransitionError::Observation("observation cardinality is zero"),
        ));
    }
    if slot0.observation_cardinality_next != event.old {
        return Err(final_mismatch(
            "slot0 observationCardinalityNext",
            U256::from(slot0.observation_cardinality_next),
            U256::from(event.old),
        ));
    }
    let mut updates = Vec::with_capacity(usize::from(event.new - event.old) + 1);
    for index in event.old..event.new {
        updates.push(StateUpdate::slot(
            address,
            surface.observations_base_slot + U256::from(index),
            U256::from(1),
        ));
    }
    let mask = U256::from(u16::MAX) << 216_usize;
    updates.push(StateUpdate::slot(
        address,
        layout.slot0_slot,
        (slot0_raw & !mask) | (U256::from(event.new) << 216_usize),
    ));
    Ok(updates)
}

/// Base slot of canonical Uniswap V3's `positions` mapping.
const UNISWAP_V3_POSITIONS_SLOT: u8 = 7;

/// The four storage words of `positions[keccak256(owner, tickLower, tickUpper)]`
/// for a canonical Uniswap V3 pool.
///
/// The transition does not reconstruct these — `swap` never reads `positions`,
/// so they sit outside the pool's pricing surface — but a `Mint`/`Burn` does
/// change them, so a caller that may hold them warm invalidates them instead of
/// letting a stale value survive.
pub(super) fn uniswap_v3_position_slots(
    owner: Address,
    tick_lower: i32,
    tick_upper: i32,
) -> [U256; 4] {
    let mut packed = Vec::with_capacity(26);
    packed.extend_from_slice(owner.as_slice());
    packed.extend_from_slice(&tick_lower.to_be_bytes()[1..]);
    packed.extend_from_slice(&tick_upper.to_be_bytes()[1..]);
    let key = alloy_primitives::keccak256(packed);
    let mut encoded = [0_u8; 64];
    encoded[..32].copy_from_slice(key.as_slice());
    encoded[63] = UNISWAP_V3_POSITIONS_SLOT;
    let base = U256::from_be_slice(alloy_primitives::keccak256(encoded).as_slice());
    [
        base,
        base + U256::from(1),
        base + U256::from(2),
        base + U256::from(3),
    ]
}

/// Derive the complete swap-induced update batch for one reviewed deployed
/// Aerodrome/Velodrome Slipstream runtime.
pub(super) fn derive_slipstream_swap(
    address: Address,
    layout: V3StorageLayout,
    swap: DecodedSwap,
    state: &dyn StateView,
    context: &AdapterEventContext,
) -> Result<Vec<StateUpdate>, AdapterEventError> {
    let claimed_fee = context
        .slipstream_fee_evidence
        .map(|evidence| evidence.effective_swap_fee);
    let (updates, inferred_fee) =
        derive_slipstream_swap_inner(address, layout, swap, state, context)?;
    if claimed_fee.is_some_and(|claimed_fee| claimed_fee != inferred_fee) {
        return Err(AdapterEventError::V3Transition(
            V3TransitionError::SlipstreamFeeEvidence(
                "effective swap fee does not match unique event-derived fee",
            ),
        ));
    }
    Ok(updates)
}

/// Derive the complete quote/search-state transition for a reviewed
/// Aerodrome/Velodrome Slipstream `Mint` or `Burn` event.
///
/// Position ownership and tokens-owed accounting live outside the AMM
/// adapter's declared search surface. Every pool cell which can affect a
/// subsequent quote is nevertheless read from the exact parent and updated
/// here: both boundary ticks, their bitmap words, the observation ring, and
/// active liquidity. Missing parent cells fail closed rather than becoming
/// implicit zeroes.
pub(super) fn derive_slipstream_liquidity(
    address: Address,
    layout: V3StorageLayout,
    event: DecodedLiquidity,
    state: &dyn StateView,
    context: &AdapterEventContext,
) -> Result<Vec<StateUpdate>, AdapterEventError> {
    validate_reviewed_slipstream_event(address, layout, context)?;
    if event.tick_lower < -887_272
        || event.tick_upper > 887_272
        || event.tick_lower >= event.tick_upper
        || event.tick_lower.rem_euclid(layout.tick_spacing) != 0
        || event.tick_upper.rem_euclid(layout.tick_spacing) != 0
    {
        return Err(contradiction(
            "Slipstream liquidity range is invalid or not spacing-aligned",
        ));
    }
    if event.amount == 0 {
        if event.is_mint {
            return Err(contradiction("Slipstream Mint liquidity must be positive"));
        }
        // A zero-amount Burn is a position fee poke. It cannot change the
        // adapter-owned pool search state.
        return Ok(Vec::new());
    }
    let signed_amount =
        i128::try_from(event.amount).map_err(|_| arithmetic("liquidity amount exceeds int128"))?;

    let slot0_raw = required(state, address, layout.slot0_slot)?;
    let slot0 = Slot0::decode(slot0_raw);
    if ((slot0.raw >> 232_usize) & U256::from(u8::MAX)).to::<u8>() != 1 {
        return Err(contradiction("parent Slipstream slot0 is locked"));
    }
    validate_parent_slot0(slot0)?;
    let liquidity_word = required(state, address, layout.liquidity_slot)?;
    let active_liquidity = liquidity_word & WORD_128_MASK;
    let max_liquidity_per_tick = (liquidity_word >> 128_usize) & WORD_128_MASK;

    let (observation_index, observation_cardinality, oracle_write, oracle_now) = advance_oracle(
        address,
        SLIPSTREAM_SURFACE,
        slot0,
        active_liquidity,
        state,
        context,
    )?;
    let fee_growth_0 = required(state, address, SLIPSTREAM_SURFACE.fee_growth_0_slot)?;
    let fee_growth_1 = required(state, address, SLIPSTREAM_SURFACE.fee_growth_1_slot)?;
    // The reviewed Optimism runtime initializes rewardGrowthOutside for a new
    // tick at/below the current tick. The reviewed Base runtime does not; this
    // is an observed deployed-bytecode semantic difference, not ABI parity.
    let initial_reward_growth = if context.chain_id == Some(10) {
        required(state, address, SLIPSTREAM_REWARD_GROWTH_SLOT)?
    } else {
        U256::ZERO
    };

    let lower = update_slipstream_liquidity_tick(
        address,
        layout,
        event.tick_lower,
        slot0.tick,
        signed_amount,
        event.is_mint,
        false,
        max_liquidity_per_tick,
        fee_growth_0,
        fee_growth_1,
        initial_reward_growth,
        oracle_now,
        context.block_timestamp.expect("validated") as u32,
        state,
    )?;
    let upper = update_slipstream_liquidity_tick(
        address,
        layout,
        event.tick_upper,
        slot0.tick,
        signed_amount,
        event.is_mint,
        true,
        max_liquidity_per_tick,
        fee_growth_0,
        fee_growth_1,
        initial_reward_growth,
        oracle_now,
        context.block_timestamp.expect("validated") as u32,
        state,
    )?;

    let mut updates = Vec::with_capacity(16);
    updates.extend(
        lower
            .keys
            .into_iter()
            .zip(lower.words)
            .map(|(slot, value)| StateUpdate::slot(address, slot, value)),
    );
    updates.extend(
        upper
            .keys
            .into_iter()
            .zip(upper.words)
            .map(|(slot, value)| StateUpdate::slot(address, slot, value)),
    );

    let mut bitmap_values = std::collections::BTreeMap::<U256, U256>::new();
    let mut changed_bitmaps = std::collections::BTreeSet::<U256>::new();
    for tick in [lower, upper] {
        let word = tick.tick.div_euclid(layout.tick_spacing).div_euclid(256);
        let bit = tick.tick.div_euclid(layout.tick_spacing).rem_euclid(256) as usize;
        let slot = v3_tick_bitmap_storage_key_with_base(word as i16, layout.tick_bitmap_base_slot);
        let value = bitmap_values
            .entry(slot)
            .or_insert(required(state, address, slot)?);
        let mask = U256::from(1) << bit;
        let parent_is_set = !(*value & mask).is_zero();
        if parent_is_set != tick.was_initialized {
            return Err(AdapterEventError::V3Transition(
                V3TransitionError::InitializedTick {
                    tick: tick.tick,
                    reason: "bitmap disagrees with parent Tick.Info initialization",
                },
            ));
        }
        if tick.was_initialized != tick.is_initialized {
            *value ^= mask;
            changed_bitmaps.insert(slot);
        }
    }
    updates.extend(
        bitmap_values
            .into_iter()
            .filter(|(slot, _)| changed_bitmaps.contains(slot))
            .map(|(slot, value)| StateUpdate::slot(address, slot, value)),
    );

    if slot0.tick >= event.tick_lower && slot0.tick < event.tick_upper {
        let next_liquidity = if event.is_mint {
            active_liquidity
                .checked_add(U256::from(event.amount))
                .ok_or_else(|| arithmetic("active liquidity overflow"))?
        } else {
            active_liquidity
                .checked_sub(U256::from(event.amount))
                .ok_or_else(|| arithmetic("active liquidity underflow"))?
        };
        if next_liquidity > WORD_128_MASK {
            return Err(arithmetic("active liquidity exceeds uint128"));
        }
        if let Some((slot, value)) = oracle_write {
            updates.push(StateUpdate::slot(address, slot, value));
            updates.push(StateUpdate::slot(
                address,
                layout.slot0_slot,
                slot0.encode_final(
                    slot0.sqrt_price_x96,
                    slot0.tick,
                    observation_index,
                    observation_cardinality,
                ),
            ));
        }
        updates.push(StateUpdate::slot(
            address,
            layout.liquidity_slot,
            next_liquidity | (max_liquidity_per_tick << 128_usize),
        ));
    }
    Ok(updates)
}

#[derive(Clone, Copy, Debug)]
struct LiquidityTickUpdate {
    tick: i32,
    keys: [U256; 6],
    words: [U256; 6],
    was_initialized: bool,
    is_initialized: bool,
}

#[allow(clippy::too_many_arguments)]
fn update_slipstream_liquidity_tick(
    address: Address,
    layout: V3StorageLayout,
    tick: i32,
    current_tick: i32,
    amount: i128,
    is_mint: bool,
    upper: bool,
    max_liquidity_per_tick: U256,
    fee_growth_0: U256,
    fee_growth_1: U256,
    initial_reward_growth: U256,
    oracle_now: Observation,
    timestamp: u32,
    state: &dyn StateView,
) -> Result<LiquidityTickUpdate, AdapterEventError> {
    let keys = slipstream_tick_info_storage_keys_with_base(tick, layout.ticks_base_slot);
    let mut words = [U256::ZERO; 6];
    for (word, key) in words.iter_mut().zip(keys) {
        *word = required(state, address, key)?;
    }
    let old_gross = words[0] & WORD_128_MASK;
    let old_net = ((words[0] >> 128_usize) & WORD_128_MASK).to::<u128>() as i128;
    let was_initialized = !old_gross.is_zero();
    let initialized_flag = ((words[5] >> 248_usize) & U256::from(u8::MAX)).to::<u8>() == 1;
    if was_initialized != initialized_flag {
        return Err(AdapterEventError::V3Transition(
            V3TransitionError::InitializedTick {
                tick,
                reason: "Tick.Info initialized flag disagrees with liquidityGross",
            },
        ));
    }
    if !was_initialized && words.iter().any(|word| !word.is_zero()) {
        return Err(AdapterEventError::V3Transition(
            V3TransitionError::InitializedTick {
                tick,
                reason: "uninitialized Tick.Info contains nonzero state",
            },
        ));
    }
    let amount_u256 = U256::from(amount as u128);
    let new_gross = if is_mint {
        old_gross
            .checked_add(amount_u256)
            .ok_or_else(|| arithmetic("tick liquidityGross overflow"))?
    } else {
        old_gross
            .checked_sub(amount_u256)
            .ok_or_else(|| arithmetic("tick liquidityGross underflow"))?
    };
    if new_gross > max_liquidity_per_tick {
        return Err(arithmetic(
            "tick liquidityGross exceeds maxLiquidityPerTick",
        ));
    }
    let delta = if is_mint { amount } else { -amount };
    let net_delta = if upper { -delta } else { delta };
    let new_net = old_net
        .checked_add(net_delta)
        .ok_or_else(|| arithmetic("tick liquidityNet overflow"))?;
    let is_initialized = !new_gross.is_zero();

    if !was_initialized && is_initialized {
        if tick <= current_tick {
            words[2] = fee_growth_0;
            words[3] = fee_growth_1;
            words[4] = initial_reward_growth;
            words[5] = unsigned_bits(oracle_now.tick_cumulative, 56)
                | ((oracle_now.seconds_per_liquidity_cumulative_x128 & WORD_160_MASK) << 56_usize)
                | (U256::from(timestamp) << 216_usize)
                | (U256::from(1) << 248_usize);
        } else {
            words[5] = U256::from(1) << 248_usize;
        }
    }
    words[0] = new_gross | (U256::from(new_net as u128) << 128_usize);
    if !is_initialized {
        words = [U256::ZERO; 6];
    }
    Ok(LiquidityTickUpdate {
        tick,
        keys,
        words,
        was_initialized,
        is_initialized,
    })
}

pub(super) fn validate_reviewed_slipstream_event(
    address: Address,
    layout: V3StorageLayout,
    context: &AdapterEventContext,
) -> Result<(), AdapterEventError> {
    validate_context(context)?;
    if layout != V3StorageLayout::slipstream(layout.tick_spacing) {
        return Err(contradiction(
            "exact Slipstream event transition requires the deployed core storage layout",
        ));
    }
    if layout.tick_spacing <= 0 {
        return Err(contradiction("Slipstream tick spacing must be positive"));
    }
    let reviewed = match context.chain_id {
        Some(8_453) => reviewed_slipstream_runtime(SlipstreamRuntimeFamily::AerodromeBaseBifi),
        Some(10) => reviewed_slipstream_runtime(SlipstreamRuntimeFamily::VelodromeOptimismBifi),
        _ => return Err(contradiction("unreviewed Slipstream chain identity")),
    };
    if address != reviewed.pool {
        return Err(contradiction("unreviewed Slipstream pool identity"));
    }
    if layout.tick_spacing != 200 {
        return Err(contradiction(
            "reviewed Slipstream pool tick spacing does not match registration",
        ));
    }
    Ok(())
}

pub(super) fn infer_slipstream_swap_fee(
    address: Address,
    layout: V3StorageLayout,
    swap: DecodedSwap,
    state: &dyn StateView,
    context: &AdapterEventContext,
) -> Result<u32, AdapterEventError> {
    derive_slipstream_swap_inner(address, layout, swap, state, context).map(|(_, fee)| fee)
}

fn derive_slipstream_swap_inner(
    address: Address,
    layout: V3StorageLayout,
    swap: DecodedSwap,
    state: &dyn StateView,
    context: &AdapterEventContext,
) -> Result<(Vec<StateUpdate>, u32), AdapterEventError> {
    validate_reviewed_slipstream_event(address, layout, context)?;
    let evidence = context
        .slipstream_fee_evidence
        .map(|_| validate_slipstream_evidence(address, state, context))
        .transpose()?;
    let unstaked_fee = evidence.map_or(0, |evidence| evidence.effective_unstaked_fee);
    if swap.sqrt_price_x96 > SLOT0_SQRT_MASK || swap.liquidity > WORD_128_MASK {
        return Err(AdapterEventError::MalformedLog(
            "Slipstream Swap final value exceeds its ABI width",
        ));
    }

    let slot0_raw = required(state, address, layout.slot0_slot)?;
    let slot0 = Slot0::decode(slot0_raw);
    if ((slot0.raw >> 232_usize) & U256::from(u8::MAX)).to::<u8>() != 1 {
        return Err(contradiction("parent Slipstream slot0 is locked"));
    }
    validate_parent_slot0(slot0)?;
    let liquidity_word = required(state, address, layout.liquidity_slot)?;
    let start_liquidity = liquidity_word & WORD_128_MASK;
    let staked_word = required(state, address, SLIPSTREAM_STAKED_LAST_SPACING_SLOT)?;
    let start_staked_liquidity = staked_word & WORD_128_MASK;
    if start_staked_liquidity > start_liquidity {
        return Err(contradiction(
            "parent staked liquidity exceeds active liquidity",
        ));
    }
    let stored_spacing = signed_from_bits((staked_word >> 160_usize) & SLOT0_TICK_MASK, 24) as i32;
    if stored_spacing != layout.tick_spacing {
        return Err(AdapterEventError::V3Transition(
            V3TransitionError::SlipstreamFeeEvidence(
                "registration tick spacing does not match pool storage",
            ),
        ));
    }
    let mut last_updated = ((staked_word >> 128_usize) & WORD_32_MASK).to::<u32>();
    let mut fee_growth_0 = if evidence.is_some() {
        required(state, address, SLIPSTREAM_SURFACE.fee_growth_0_slot)?
    } else {
        U256::ZERO
    };
    let mut fee_growth_1 = if evidence.is_some() {
        required(state, address, SLIPSTREAM_SURFACE.fee_growth_1_slot)?
    } else {
        U256::ZERO
    };
    let mut reward_growth = if evidence.is_some() {
        required(state, address, SLIPSTREAM_REWARD_GROWTH_SLOT)?
    } else {
        U256::ZERO
    };
    let mut gauge_fees = if evidence.is_some() {
        required(state, address, SLIPSTREAM_GAUGE_FEES_SLOT)?
    } else {
        U256::ZERO
    };

    let zero_for_one = match (
        swap.amount0_negative,
        swap.amount1_negative,
        swap.amount0.is_zero(),
        swap.amount1.is_zero(),
    ) {
        (false, true, false, false) => true,
        (true, false, false, false) => false,
        (false, false, false, true) => true,
        (false, false, true, false) => false,
        _ => {
            return Err(contradiction(
                "swap must contain one positive input and one negative output",
            ));
        }
    };
    let actual_output_is_zero = if zero_for_one {
        swap.amount1.is_zero()
    } else {
        swap.amount0.is_zero()
    };
    let unchanged_tiny_swap = actual_output_is_zero && swap.sqrt_price_x96 == slot0.sqrt_price_x96;
    if !unchanged_tiny_swap
        && ((zero_for_one && swap.sqrt_price_x96 >= slot0.sqrt_price_x96)
            || (!zero_for_one && swap.sqrt_price_x96 <= slot0.sqrt_price_x96))
    {
        return Err(contradiction("swap direction contradicts its final price"));
    }
    validate_final_tick(swap.sqrt_price_x96, swap.tick, zero_for_one)?;

    let (oracle_index, oracle_cardinality, oracle_write, oracle_now) = if slot0.tick != swap.tick {
        advance_oracle(
            address,
            SLIPSTREAM_SURFACE,
            slot0,
            start_liquidity,
            state,
            context,
        )?
    } else {
        (
            slot0.observation_index,
            slot0.observation_cardinality,
            None,
            Observation {
                timestamp: context.block_timestamp.expect("validated") as u32,
                tick_cumulative: 0,
                seconds_per_liquidity_cumulative_x128: U256::ZERO,
                initialized: true,
            },
        )
    };

    let mut ticks = Vec::new();
    let mut segments = Vec::new();
    let mut current_sqrt = slot0.sqrt_price_x96;
    let mut current_tick = slot0.tick;
    let mut current_liquidity = start_liquidity;
    let mut current_staked_liquidity = start_staked_liquidity;
    let mut steps = 0_u32;
    if zero_for_one && current_sqrt == swap.sqrt_price_x96 {
        let boundary = next_slipstream_step_boundary(address, layout, current_tick, true, state)?;
        let boundary_sqrt = sqrt_ratio_at_tick(boundary.tick)?;
        if boundary_sqrt == current_sqrt {
            consume_step(&mut steps)?;
            let crossed_tick = boundary
                .initialized
                .then(|| {
                    let initialized =
                        load_slipstream_initialized_tick(address, layout, boundary.tick, state)?;
                    let index = ticks.len();
                    ticks.push(initialized);
                    Ok::<_, AdapterEventError>(index)
                })
                .transpose()?;
            segments.push(SlipstreamSegment {
                liquidity: current_liquidity,
                staked_liquidity: current_staked_liquidity,
                amount_in: U256::ZERO,
                amount_out: U256::ZERO,
                reached_boundary: true,
                crossed_tick,
            });
            if let Some(index) = crossed_tick {
                current_liquidity =
                    apply_liquidity_net(current_liquidity, ticks[index].liquidity_net, true)?;
                current_staked_liquidity = apply_liquidity_net(
                    current_staked_liquidity,
                    ticks[index].staked_liquidity_net,
                    true,
                )?;
                validate_slipstream_liquidity(current_liquidity, current_staked_liquidity)?;
            }
            current_tick = boundary.tick - 1;
        }
    }
    while current_sqrt != swap.sqrt_price_x96 {
        consume_step(&mut steps)?;
        let boundary =
            next_slipstream_step_boundary(address, layout, current_tick, zero_for_one, state)?;
        let boundary_sqrt = sqrt_ratio_at_tick(boundary.tick)?;
        let target = if zero_for_one {
            boundary_sqrt.max(swap.sqrt_price_x96)
        } else {
            boundary_sqrt.min(swap.sqrt_price_x96)
        };
        let reached_boundary = target == boundary_sqrt;
        let (amount_in, amount_out) =
            segment_amounts(current_sqrt, target, current_liquidity, zero_for_one)?;
        let crossed_tick = if reached_boundary && boundary.initialized {
            Some({
                let initialized =
                    load_slipstream_initialized_tick(address, layout, boundary.tick, state)?;
                let index = ticks.len();
                ticks.push(initialized);
                index
            })
        } else {
            None
        };
        segments.push(SlipstreamSegment {
            liquidity: current_liquidity,
            staked_liquidity: current_staked_liquidity,
            amount_in,
            amount_out,
            reached_boundary,
            crossed_tick,
        });
        current_sqrt = target;
        if reached_boundary {
            if let Some(index) = crossed_tick {
                current_liquidity = apply_liquidity_net(
                    current_liquidity,
                    ticks[index].liquidity_net,
                    zero_for_one,
                )?;
                current_staked_liquidity = apply_liquidity_net(
                    current_staked_liquidity,
                    ticks[index].staked_liquidity_net,
                    zero_for_one,
                )?;
                validate_slipstream_liquidity(current_liquidity, current_staked_liquidity)?;
            }
            current_tick = if zero_for_one {
                boundary.tick - 1
            } else {
                boundary.tick
            };
        } else {
            current_tick = swap.tick;
        }
    }
    if segments.is_empty() {
        if unchanged_tiny_swap {
            segments.push(SlipstreamSegment {
                liquidity: current_liquidity,
                staked_liquidity: current_staked_liquidity,
                amount_in: U256::ZERO,
                amount_out: U256::ZERO,
                reached_boundary: false,
                crossed_tick: None,
            });
        } else {
            return Err(contradiction("swap did not move the square-root price"));
        }
    }
    if current_liquidity != swap.liquidity {
        return Err(final_mismatch(
            "liquidity",
            current_liquidity,
            swap.liquidity,
        ));
    }
    if current_tick != swap.tick {
        return Err(final_mismatch(
            "tick",
            U256::from((current_tick as u32) & 0x00ff_ffff),
            U256::from((swap.tick as u32) & 0x00ff_ffff),
        ));
    }
    if evidence.is_some_and(|evidence| {
        evidence.unstaked_fee_proof.kind()
            == SlipstreamUnstakedFeeProofKind::UnusedAllLiquidityStaked
    }) && segments
        .iter()
        .any(|segment| segment.liquidity != segment.staked_liquidity)
    {
        return Err(AdapterEventError::V3Transition(
            V3TransitionError::SlipstreamFeeEvidence(
                "unstaked fee was declared unused for mixed-liquidity swap step",
            ),
        ));
    }

    let actual_input = if zero_for_one {
        swap.amount0
    } else {
        swap.amount1
    };
    let actual_output = if zero_for_one {
        swap.amount1
    } else {
        swap.amount0
    };
    let principal_input = checked_sum(segments.iter().map(|segment| segment.amount_in))?;
    let derived_output = checked_sum(segments.iter().map(|segment| segment.amount_out))?;
    if derived_output != actual_output {
        return Err(final_mismatch(
            "signed output",
            derived_output,
            actual_output,
        ));
    }
    let total_fee = actual_input
        .checked_sub(principal_input)
        .ok_or_else(|| final_mismatch("signed input principal", principal_input, actual_input))?;
    let fee = infer_slipstream_fee(&segments, total_fee)?;

    // Search needs the exact price/liquidity traversal, observation state, and
    // staked-liquidity bound used by subsequent pool execution. Fee growth,
    // gauge accrual, and reward bookkeeping do not influence executable swap
    // amounts. When runtime-bound fee evidence is unavailable, publish only
    // this quote-exact surface instead of forcing an RPC reconstruction.
    if evidence.is_none() {
        let final_slot0 = slot0.encode_final(
            swap.sqrt_price_x96,
            swap.tick,
            oracle_index,
            oracle_cardinality,
        );
        let final_staked_word = (staked_word & !WORD_128_MASK) | current_staked_liquidity;
        let final_liquidity_word =
            (liquidity_word & (WORD_128_MASK << 128_usize)) | current_liquidity;
        let mut updates = vec![
            StateUpdate::slot(address, layout.slot0_slot, final_slot0),
            StateUpdate::slot(
                address,
                SLIPSTREAM_STAKED_LAST_SPACING_SLOT,
                final_staked_word,
            ),
            StateUpdate::slot(address, layout.liquidity_slot, final_liquidity_word),
        ];
        if let Some((slot, value)) = oracle_write {
            updates.push(StateUpdate::slot(address, slot, value));
        }
        return Ok((updates, fee));
    }
    if segments
        .last()
        .is_some_and(|segment| segment.reached_boundary)
    {
        let full_step_fees = checked_sum(
            segments
                .iter()
                .map(|segment| fee_for_full_step(segment.amount_in, fee))
                .collect::<Result<Vec<_>, _>>()?,
        )?;
        if let Some(residual) = total_fee.checked_sub(full_step_fees)
            && !residual.is_zero()
            && valid_partial_fee(U256::ZERO, residual, fee)?
        {
            segments.push(SlipstreamSegment {
                liquidity: current_liquidity,
                staked_liquidity: current_staked_liquidity,
                amount_in: U256::ZERO,
                amount_out: U256::ZERO,
                reached_boundary: false,
                crossed_tick: None,
            });
        }
    }

    let mut remaining_fee = total_fee;
    let mut gauge_fee = U256::ZERO;
    let timestamp = context.block_timestamp.expect("validated") as u32;
    let mut tick_updates = Vec::with_capacity(ticks.len() * 4);
    let mut rewards_updated = false;
    let mut reward_reserve_update = None;
    let mut rollover_update = None;
    for (segment_index, segment) in segments.iter().enumerate() {
        let is_partial_final = segment_index + 1 == segments.len() && !segment.reached_boundary;
        let fee_amount = if is_partial_final {
            if !valid_partial_fee(segment.amount_in, remaining_fee, fee)? {
                return Err(contradiction(
                    "final-step fee does not match exact-input or exact-output semantics",
                ));
            }
            remaining_fee
        } else {
            fee_for_full_step(segment.amount_in, fee)?
        };
        remaining_fee = remaining_fee
            .checked_sub(fee_amount)
            .ok_or_else(|| contradiction("fee allocation contradicts event input"))?;
        if !segment.liquidity.is_zero() {
            let (growth, step_gauge_fee) = slipstream_fee_growth(
                fee_amount,
                segment.liquidity,
                segment.staked_liquidity,
                unstaked_fee,
            )?;
            if zero_for_one {
                fee_growth_0 = fee_growth_0.wrapping_add(growth);
            } else {
                fee_growth_1 = fee_growth_1.wrapping_add(growth);
            }
            gauge_fee = gauge_fee.wrapping_add(step_gauge_fee & WORD_128_MASK) & WORD_128_MASK;
        }

        if let Some(tick_index) = segment.crossed_tick {
            if !rewards_updated {
                let reward_update = update_slipstream_rewards(
                    address,
                    reward_growth,
                    segment.staked_liquidity,
                    last_updated,
                    timestamp,
                    state,
                )?;
                reward_growth = reward_update.reward_growth;
                last_updated = reward_update.last_updated;
                reward_reserve_update = reward_update.reward_reserve;
                rollover_update = reward_update.rollover;
                rewards_updated = true;
            }
            let initialized = &mut ticks[tick_index];
            initialized.words[2] = fee_growth_0.wrapping_sub(initialized.words[2]);
            initialized.words[3] = fee_growth_1.wrapping_sub(initialized.words[3]);
            initialized.words[4] = reward_growth.wrapping_sub(initialized.words[4]);
            initialized.words[5] = cross_tick_word(
                initialized.words[5],
                oracle_now.tick_cumulative,
                oracle_now.seconds_per_liquidity_cumulative_x128,
                timestamp,
            );
            tick_updates.extend(
                initialized.keys[2..]
                    .iter()
                    .zip(initialized.words[2..].iter().copied())
                    .map(|(slot, value)| StateUpdate::slot(address, *slot, value)),
            );
        }
    }
    if !remaining_fee.is_zero() {
        return Err(contradiction(
            "fee allocation left an unexplained remainder",
        ));
    }

    if zero_for_one {
        let token0 = (gauge_fees & WORD_128_MASK).wrapping_add(gauge_fee) & WORD_128_MASK;
        gauge_fees = (gauge_fees & (WORD_128_MASK << 128_usize)) | token0;
    } else {
        let token1 =
            ((gauge_fees >> 128_usize) & WORD_128_MASK).wrapping_add(gauge_fee) & WORD_128_MASK;
        gauge_fees = (gauge_fees & WORD_128_MASK) | (token1 << 128_usize);
    }

    let final_slot0 = slot0.encode_final(
        swap.sqrt_price_x96,
        swap.tick,
        oracle_index,
        oracle_cardinality,
    );
    let final_staked_word = (staked_word & !(WORD_128_MASK | (WORD_32_MASK << 128_usize)))
        | current_staked_liquidity
        | (U256::from(last_updated) << 128_usize);
    let final_liquidity_word = (liquidity_word & (WORD_128_MASK << 128_usize)) | current_liquidity;
    let mut updates = vec![
        StateUpdate::slot(address, layout.slot0_slot, final_slot0),
        StateUpdate::slot(
            address,
            if zero_for_one {
                SLIPSTREAM_SURFACE.fee_growth_0_slot
            } else {
                SLIPSTREAM_SURFACE.fee_growth_1_slot
            },
            if zero_for_one {
                fee_growth_0
            } else {
                fee_growth_1
            },
        ),
        StateUpdate::slot(address, SLIPSTREAM_GAUGE_FEES_SLOT, gauge_fees),
        StateUpdate::slot(
            address,
            SLIPSTREAM_STAKED_LAST_SPACING_SLOT,
            final_staked_word,
        ),
        StateUpdate::slot(address, layout.liquidity_slot, final_liquidity_word),
    ];
    if rewards_updated {
        updates.push(StateUpdate::slot(
            address,
            SLIPSTREAM_REWARD_GROWTH_SLOT,
            reward_growth,
        ));
    }
    if let Some(value) = reward_reserve_update {
        updates.push(StateUpdate::slot(
            address,
            SLIPSTREAM_REWARD_RESERVE_SLOT,
            value,
        ));
    }
    if let Some(value) = rollover_update {
        updates.push(StateUpdate::slot(address, SLIPSTREAM_ROLLOVER_SLOT, value));
    }
    if let Some((slot, value)) = oracle_write {
        updates.push(StateUpdate::slot(address, slot, value));
    }
    updates.extend(tick_updates);
    Ok((updates, fee))
}

#[derive(Clone, Copy, Debug)]
struct SlipstreamRewardUpdate {
    reward_growth: U256,
    last_updated: u32,
    reward_reserve: Option<U256>,
    rollover: Option<U256>,
}

fn reviewed_slipstream_runtime(family: SlipstreamRuntimeFamily) -> ReviewedSlipstreamRuntime {
    match family {
        SlipstreamRuntimeFamily::AerodromeBaseBifi => ReviewedSlipstreamRuntime {
            chain_id: 8_453,
            pool: address!("b378137c90444bbcecd44a1f766851fbf53d2a9e"),
            factory: address!("5e7bb104d84c7cb9b682aac2f3d509f5f406809a"),
            proxy_runtime_code_hash: b256!(
                "acd6710f7037ad095b1e4d5f8ee5b2681069cb4dd316e77e4e0cb8f85716a2a1"
            ),
            implementation: address!("ec8e5342b19977b4ef8892e02d8daecfa1315831"),
            implementation_runtime_code_hash: b256!(
                "772fb5c610b40a122036f544e5b9b5bce6becb19db9524331289d1aaed2d5888"
            ),
        },
        SlipstreamRuntimeFamily::VelodromeOptimismBifi => ReviewedSlipstreamRuntime {
            chain_id: 10,
            pool: address!("173cdc71e29d5cffa6d090ad99f555a24b8831f9"),
            factory: address!("cc0bddb707055e04e497ab22a59c2af4391cd12f"),
            proxy_runtime_code_hash: b256!(
                "063ca35333cb7f2463f087d40ff9485475550abf4858a2f63c387d4d102b0f4f"
            ),
            implementation: address!("c28ad28853a547556780bebf7847628501a3bcbb"),
            implementation_runtime_code_hash: b256!(
                "36c3da904ca0b58544254cd0d978fe4801c32dc1f9e3b3e644487ef541299794"
            ),
        },
    }
}

fn validate_slipstream_evidence(
    address: Address,
    state: &dyn StateView,
    context: &AdapterEventContext,
) -> Result<SlipstreamSwapFeeEvidence, AdapterEventError> {
    let evidence = context
        .slipstream_fee_evidence
        .ok_or(AdapterEventError::V3Transition(
            V3TransitionError::MissingSlipstreamFeeEvidence,
        ))?;
    let reviewed = reviewed_slipstream_runtime(evidence.runtime_family);
    let invalid =
        |reason| AdapterEventError::V3Transition(V3TransitionError::SlipstreamFeeEvidence(reason));
    if evidence.validate().is_err() {
        return Err(invalid("evidence constructor validation failed"));
    }
    if evidence.chain_id != reviewed.chain_id || context.chain_id != Some(evidence.chain_id) {
        return Err(invalid("chain identity mismatch"));
    }
    if address != reviewed.pool || evidence.pool != address {
        return Err(invalid("pool identity mismatch"));
    }
    if evidence.factory != reviewed.factory {
        return Err(invalid("factory identity mismatch"));
    }
    if evidence.proxy_runtime_code_hash != reviewed.proxy_runtime_code_hash
        || evidence.implementation != reviewed.implementation
        || evidence.implementation_runtime_code_hash != reviewed.implementation_runtime_code_hash
    {
        return Err(invalid("runtime identity mismatch"));
    }
    if context.block_number != Some(evidence.block_number)
        || context.block_hash != Some(evidence.block_hash)
        || context.parent_hash != Some(evidence.parent_hash)
        || context.block_timestamp != Some(evidence.block_timestamp)
        || context.transaction_hash != Some(evidence.transaction_hash)
        || context.transaction_index != Some(evidence.transaction_index)
        || context.log_index != Some(evidence.log_index)
    {
        return Err(invalid("event lineage mismatch"));
    }
    if evidence.effective_swap_fee > 100_000
        || evidence.effective_unstaked_fee > FEE_DENOMINATOR as u32
    {
        return Err(invalid("effective fee is outside deployed bounds"));
    }
    let factory_word = required(state, address, SLIPSTREAM_FACTORY_SLOT)?;
    let factory = Address::from_slice(&factory_word.to_be_bytes::<32>()[12..]);
    if factory != evidence.factory {
        return Err(invalid("parent factory storage mismatch"));
    }
    Ok(evidence)
}

fn validate_slipstream_liquidity(
    liquidity: U256,
    staked_liquidity: U256,
) -> Result<(), AdapterEventError> {
    if staked_liquidity > liquidity {
        Err(contradiction("staked liquidity exceeds active liquidity"))
    } else {
        Ok(())
    }
}

fn slipstream_fee_growth(
    fee_amount: U256,
    liquidity: U256,
    staked_liquidity: U256,
    unstaked_fee: u32,
) -> Result<(U256, U256), AdapterEventError> {
    validate_slipstream_liquidity(liquidity, staked_liquidity)?;
    if liquidity.is_zero() {
        return Ok((U256::ZERO, U256::ZERO));
    }
    if liquidity == staked_liquidity {
        return Ok((U256::ZERO, fee_amount));
    }
    let base_gauge_fee = if staked_liquidity.is_zero() {
        U256::ZERO
    } else {
        mul_div_round_up(fee_amount, staked_liquidity, liquidity)?
    };
    let unstaked_before_levy = fee_amount - base_gauge_fee;
    let levy = mul_div_round_up(
        unstaked_before_levy,
        U256::from(unstaked_fee),
        U256::from(FEE_DENOMINATOR),
    )?;
    let unstaked_fee_amount = unstaked_before_levy - levy;
    let growth = mul_div(unstaked_fee_amount, Q128, liquidity - staked_liquidity)?;
    Ok((growth, base_gauge_fee + levy))
}

fn infer_slipstream_fee(
    segments: &[SlipstreamSegment],
    total_fee: U256,
) -> Result<u32, AdapterEventError> {
    let first = binary_first_fee(segments, total_fee)?;
    let last = binary_last_fee(segments, total_fee)?;
    let (Some(first), Some(last)) = (first, last) else {
        return Err(AdapterEventError::V3Transition(
            V3TransitionError::SlipstreamFeeInferenceNoMatch,
        ));
    };
    if first > last {
        return Err(AdapterEventError::V3Transition(
            V3TransitionError::SlipstreamFeeInferenceNoMatch,
        ));
    }
    if first != last {
        return Err(AdapterEventError::V3Transition(
            V3TransitionError::SlipstreamFeeInferenceAmbiguous { first, last },
        ));
    }
    let (minimum, maximum) = slipstream_fee_interval(segments, first)?;
    if total_fee < minimum || total_fee > maximum {
        return Err(AdapterEventError::V3Transition(
            V3TransitionError::SlipstreamFeeInferenceNoMatch,
        ));
    }
    Ok(first)
}

fn binary_first_fee(
    segments: &[SlipstreamSegment],
    total_fee: U256,
) -> Result<Option<u32>, AdapterEventError> {
    let mut low = 0_u32;
    let mut high = 100_000_u32;
    while low < high {
        let middle = low + (high - low) / 2;
        let (_, maximum) = slipstream_fee_interval(segments, middle)?;
        if maximum >= total_fee {
            high = middle;
        } else {
            low = middle + 1;
        }
    }
    let (_, maximum) = slipstream_fee_interval(segments, low)?;
    Ok((maximum >= total_fee).then_some(low))
}

fn binary_last_fee(
    segments: &[SlipstreamSegment],
    total_fee: U256,
) -> Result<Option<u32>, AdapterEventError> {
    let mut low = 0_u32;
    let mut high = 100_000_u32;
    while low < high {
        let middle = low + (high - low).div_ceil(2);
        let (minimum, _) = slipstream_fee_interval(segments, middle)?;
        if minimum <= total_fee {
            low = middle;
        } else {
            high = middle - 1;
        }
    }
    let (minimum, _) = slipstream_fee_interval(segments, low)?;
    Ok((minimum <= total_fee).then_some(low))
}

fn slipstream_fee_interval(
    segments: &[SlipstreamSegment],
    fee: u32,
) -> Result<(U256, U256), AdapterEventError> {
    let Some((last, preceding)) = segments.split_last() else {
        return Err(contradiction(
            "fee inference requires at least one swap step",
        ));
    };
    let preceding_fee = checked_sum(
        preceding
            .iter()
            .map(|segment| fee_for_full_step(segment.amount_in, fee))
            .collect::<Result<Vec<_>, _>>()?,
    )?;
    let last_minimum = fee_for_full_step(last.amount_in, fee)?;
    let minimum = preceding_fee
        .checked_add(last_minimum)
        .ok_or_else(|| arithmetic("fee inference minimum overflow"))?;
    let last_maximum = if last.reached_boundary {
        last_minimum
            .checked_add(maximum_partial_fee(U256::ZERO, fee)?)
            .ok_or_else(|| arithmetic("fee inference boundary maximum overflow"))?
    } else {
        maximum_partial_fee(last.amount_in, fee)?
    };
    let maximum = preceding_fee
        .checked_add(last_maximum)
        .ok_or_else(|| arithmetic("fee inference maximum overflow"))?;
    Ok((minimum, maximum))
}

fn maximum_partial_fee(amount_in: U256, fee: u32) -> Result<U256, AdapterEventError> {
    let denominator = U256::from(FEE_DENOMINATOR - u64::from(fee));
    let next_after_fee = amount_in
        .checked_add(U256::from(1))
        .ok_or_else(|| arithmetic("partial-fee input overflow"))?;
    let first_invalid_total =
        mul_div_round_up(next_after_fee, U256::from(FEE_DENOMINATOR), denominator)?;
    first_invalid_total
        .checked_sub(U256::from(1))
        .and_then(|total| total.checked_sub(amount_in))
        .ok_or_else(|| arithmetic("partial-fee maximum underflow"))
}

fn update_slipstream_rewards(
    address: Address,
    reward_growth: U256,
    staked_liquidity: U256,
    last_updated: u32,
    timestamp: u32,
    state: &dyn StateView,
) -> Result<SlipstreamRewardUpdate, AdapterEventError> {
    let delta = timestamp.wrapping_sub(last_updated);
    if delta == 0 {
        return Ok(SlipstreamRewardUpdate {
            reward_growth,
            last_updated,
            reward_reserve: None,
            rollover: None,
        });
    }
    let reserve = required(state, address, SLIPSTREAM_REWARD_RESERVE_SLOT)?;
    if reserve.is_zero() {
        return Ok(SlipstreamRewardUpdate {
            reward_growth,
            last_updated: timestamp,
            reward_reserve: None,
            rollover: None,
        });
    }
    let rate = required(state, address, SLIPSTREAM_REWARD_RATE_SLOT)?;
    let reward = rate.wrapping_mul(U256::from(delta)).min(reserve);
    let remaining_reserve = reserve - reward;
    if staked_liquidity.is_zero() {
        let rollover = required(state, address, SLIPSTREAM_ROLLOVER_SLOT)?.wrapping_add(reward);
        Ok(SlipstreamRewardUpdate {
            reward_growth,
            last_updated: timestamp,
            reward_reserve: Some(remaining_reserve),
            rollover: Some(rollover),
        })
    } else {
        Ok(SlipstreamRewardUpdate {
            reward_growth: reward_growth.wrapping_add(mul_div(reward, Q128, staked_liquidity)?),
            last_updated: timestamp,
            reward_reserve: Some(remaining_reserve),
            rollover: None,
        })
    }
}

fn next_slipstream_step_boundary(
    address: Address,
    layout: V3StorageLayout,
    current_tick: i32,
    zero_for_one: bool,
    state: &dyn StateView,
) -> Result<SlipstreamStepBoundary, AdapterEventError> {
    let spacing = layout.tick_spacing;
    let compressed = current_tick.div_euclid(spacing);
    let (word_position, bit_position, search_compressed) = if zero_for_one {
        (
            compressed.div_euclid(256) as i16,
            compressed.rem_euclid(256) as usize,
            compressed,
        )
    } else {
        let search = compressed
            .checked_add(1)
            .ok_or_else(|| arithmetic("compressed tick overflow"))?;
        (
            search.div_euclid(256) as i16,
            search.rem_euclid(256) as usize,
            search,
        )
    };
    let bitmap_slot =
        v3_tick_bitmap_storage_key_with_base(word_position, layout.tick_bitmap_base_slot);
    let bitmap = required(state, address, bitmap_slot)?;
    let (next_compressed, initialized) = if zero_for_one {
        let mask = if bit_position == 255 {
            U256::MAX
        } else {
            (U256::from(1) << (bit_position + 1)) - U256::from(1)
        };
        let masked = bitmap & mask;
        if masked.is_zero() {
            (search_compressed - bit_position as i32, false)
        } else {
            let most_significant_bit = masked.bit_len() - 1;
            (
                search_compressed - (bit_position - most_significant_bit) as i32,
                true,
            )
        }
    } else {
        let lower_mask = if bit_position == 0 {
            U256::ZERO
        } else {
            (U256::from(1) << bit_position) - U256::from(1)
        };
        let masked = bitmap & !lower_mask;
        if masked.is_zero() {
            (search_compressed + (255 - bit_position) as i32, false)
        } else {
            let least_significant_bit = masked.trailing_zeros();
            (
                search_compressed + (least_significant_bit - bit_position) as i32,
                true,
            )
        }
    };
    let tick = next_compressed
        .checked_mul(spacing)
        .ok_or_else(|| arithmetic("step-boundary tick overflow"))?
        .clamp(-887_272, 887_272);
    Ok(SlipstreamStepBoundary { tick, initialized })
}

fn load_slipstream_initialized_tick(
    address: Address,
    layout: V3StorageLayout,
    tick: i32,
    state: &dyn StateView,
) -> Result<SlipstreamInitializedTick, AdapterEventError> {
    let keys = slipstream_tick_info_storage_keys_with_base(tick, layout.ticks_base_slot);
    let words = [
        required(state, address, keys[0])?,
        required(state, address, keys[1])?,
        required(state, address, keys[2])?,
        required(state, address, keys[3])?,
        required(state, address, keys[4])?,
        required(state, address, keys[5])?,
    ];
    if ((words[5] >> 248_usize) & U256::from(u8::MAX)) != U256::from(1) {
        return Err(AdapterEventError::V3Transition(
            V3TransitionError::InitializedTick {
                tick,
                reason: "bitmap is set but Slipstream Tick.Info.initialized is false",
            },
        ));
    }
    if (words[0] & WORD_128_MASK).is_zero() {
        return Err(AdapterEventError::V3Transition(
            V3TransitionError::InitializedTick {
                tick,
                reason: "bitmap is set but Slipstream Tick.Info.liquidityGross is zero",
            },
        ));
    }
    Ok(SlipstreamInitializedTick {
        keys,
        words,
        liquidity_net: ((words[0] >> 128_usize) & WORD_128_MASK).to::<u128>() as i128,
        staked_liquidity_net: (words[1] & WORD_128_MASK).to::<u128>() as i128,
    })
}

fn validate_context(context: &AdapterEventContext) -> Result<(), AdapterEventError> {
    for (present, name) in [
        (context.chain_id.is_some(), "chain_id"),
        (context.block_number.is_some(), "block_number"),
        (context.block_hash.is_some(), "block_hash"),
        (context.parent_hash.is_some(), "parent_hash"),
        (context.block_timestamp.is_some(), "block_timestamp"),
        (context.transaction_hash.is_some(), "transaction_hash"),
        (context.transaction_index.is_some(), "transaction_index"),
        (context.log_index.is_some(), "log_index"),
    ] {
        if !present {
            return Err(AdapterEventError::V3Transition(
                V3TransitionError::MissingContext(name),
            ));
        }
    }
    let timestamp = context.block_timestamp.expect("checked above");
    if timestamp > u64::from(u32::MAX) {
        return Err(AdapterEventError::V3Transition(
            V3TransitionError::Observation("block timestamp exceeds uint32"),
        ));
    }
    Ok(())
}

fn consume_step(steps: &mut u32) -> Result<(), AdapterEventError> {
    *steps = steps.saturating_add(1);
    if *steps > MAX_SWAP_STEPS {
        Err(AdapterEventError::V3Transition(
            V3TransitionError::WorkLimitExceeded {
                limit: MAX_SWAP_STEPS,
            },
        ))
    } else {
        Ok(())
    }
}

fn required(
    state: &dyn StateView,
    address: Address,
    slot: U256,
) -> Result<U256, AdapterEventError> {
    state
        .storage(address, slot)
        .ok_or(AdapterEventError::MissingState { address, slot })
}

fn next_step_boundary(
    address: Address,
    layout: V3StorageLayout,
    current_tick: i32,
    zero_for_one: bool,
    state: &dyn StateView,
) -> Result<StepBoundary, AdapterEventError> {
    let spacing = layout.tick_spacing;
    let compressed = current_tick.div_euclid(spacing);
    let (word_position, bit_position, search_compressed) = if zero_for_one {
        (
            compressed.div_euclid(256) as i16,
            compressed.rem_euclid(256) as usize,
            compressed,
        )
    } else {
        let search = compressed
            .checked_add(1)
            .ok_or_else(|| arithmetic("compressed tick overflow"))?;
        (
            search.div_euclid(256) as i16,
            search.rem_euclid(256) as usize,
            search,
        )
    };
    let bitmap_slot =
        v3_tick_bitmap_storage_key_with_base(word_position, layout.tick_bitmap_base_slot);
    let bitmap = required(state, address, bitmap_slot)?;
    let (next_compressed, initialized) = if zero_for_one {
        let mask = if bit_position == 255 {
            U256::MAX
        } else {
            (U256::from(1) << (bit_position + 1)) - U256::from(1)
        };
        let masked = bitmap & mask;
        if masked.is_zero() {
            (search_compressed - bit_position as i32, false)
        } else {
            let most_significant_bit = masked.bit_len() - 1;
            (
                search_compressed - (bit_position - most_significant_bit) as i32,
                true,
            )
        }
    } else {
        let lower_mask = if bit_position == 0 {
            U256::ZERO
        } else {
            (U256::from(1) << bit_position) - U256::from(1)
        };
        let masked = bitmap & !lower_mask;
        if masked.is_zero() {
            (search_compressed + (255 - bit_position) as i32, false)
        } else {
            let least_significant_bit = masked.trailing_zeros();
            (
                search_compressed + (least_significant_bit - bit_position) as i32,
                true,
            )
        }
    };
    let tick = next_compressed
        .checked_mul(spacing)
        .ok_or_else(|| arithmetic("step-boundary tick overflow"))?
        .clamp(-887_272, 887_272);
    Ok(StepBoundary { tick, initialized })
}

fn load_initialized_tick(
    address: Address,
    layout: V3StorageLayout,
    tick: i32,
    state: &dyn StateView,
) -> Result<InitializedTick, AdapterEventError> {
    let keys = v3_tick_info_storage_keys_with_base(tick, layout.ticks_base_slot);
    let words = [
        required(state, address, keys[0])?,
        required(state, address, keys[1])?,
        required(state, address, keys[2])?,
        required(state, address, keys[3])?,
    ];
    if ((words[3] >> 248_usize) & U256::from(u8::MAX)) != U256::from(1) {
        return Err(AdapterEventError::V3Transition(
            V3TransitionError::InitializedTick {
                tick,
                reason: "bitmap is set but Tick.Info.initialized is false",
            },
        ));
    }
    if (words[0] & WORD_128_MASK).is_zero() {
        return Err(AdapterEventError::V3Transition(
            V3TransitionError::InitializedTick {
                tick,
                reason: "bitmap is set but Tick.Info.liquidityGross is zero",
            },
        ));
    }
    Ok(InitializedTick {
        keys,
        words,
        liquidity_net: ((words[0] >> 128_usize) & WORD_128_MASK).to::<u128>() as i128,
    })
}

fn advance_oracle(
    address: Address,
    surface: SwapStorageSurface,
    slot0: Slot0,
    liquidity: U256,
    state: &dyn StateView,
    context: &AdapterEventContext,
) -> Result<OracleAdvance, AdapterEventError> {
    if slot0.observation_cardinality == 0 {
        return Err(AdapterEventError::V3Transition(
            V3TransitionError::Observation("observation cardinality is zero"),
        ));
    }
    if slot0.observation_index >= slot0.observation_cardinality {
        return Err(AdapterEventError::V3Transition(
            V3TransitionError::Observation("observation index exceeds cardinality"),
        ));
    }
    if slot0.observation_cardinality_next < slot0.observation_cardinality {
        return Err(AdapterEventError::V3Transition(
            V3TransitionError::Observation(
                "observation cardinalityNext is smaller than cardinality",
            ),
        ));
    }
    let current_slot = surface.observations_base_slot + U256::from(slot0.observation_index);
    let current = Observation::decode(required(state, address, current_slot)?);
    if !current.initialized {
        return Err(AdapterEventError::V3Transition(
            V3TransitionError::Observation("current observation is not initialized"),
        ));
    }
    let timestamp = context.block_timestamp.expect("validated") as u32;
    let now = current.transform(timestamp, slot0.tick, liquidity);
    if current.timestamp == timestamp {
        return Ok((
            slot0.observation_index,
            slot0.observation_cardinality,
            None,
            now,
        ));
    }
    let cardinality = if slot0.observation_cardinality_next > slot0.observation_cardinality
        && slot0.observation_index == slot0.observation_cardinality - 1
    {
        slot0.observation_cardinality_next
    } else {
        slot0.observation_cardinality
    };
    let next_index = (slot0.observation_index + 1) % cardinality;
    let slot = surface.observations_base_slot + U256::from(next_index);
    Ok((next_index, cardinality, Some((slot, now.encode())), now))
}

fn cross_tick_word(
    raw: U256,
    tick_cumulative: i64,
    seconds_per_liquidity_cumulative_x128: U256,
    timestamp: u32,
) -> U256 {
    let old_tick_cumulative = signed_from_bits(raw & WORD_56_MASK, 56);
    let old_spl = (raw >> 56_usize) & WORD_160_MASK;
    let old_seconds = ((raw >> 216_usize) & WORD_32_MASK).to::<u32>();
    let new_tick_cumulative = wrapping_signed_sub(tick_cumulative, old_tick_cumulative, 56);
    let new_spl = seconds_per_liquidity_cumulative_x128.wrapping_sub(old_spl) & WORD_160_MASK;
    let new_seconds = timestamp.wrapping_sub(old_seconds);
    unsigned_bits(new_tick_cumulative, 56)
        | (new_spl << 56_usize)
        | (U256::from(new_seconds) << 216_usize)
        | (raw & (U256::from(u8::MAX) << 248_usize))
}

fn apply_liquidity_net(
    liquidity: U256,
    liquidity_net: i128,
    zero_for_one: bool,
) -> Result<U256, AdapterEventError> {
    let signed_delta = if zero_for_one {
        liquidity_net
            .checked_neg()
            .ok_or_else(|| arithmetic("liquidity net cannot be negated"))?
    } else {
        liquidity_net
    };
    let result = if signed_delta < 0 {
        liquidity.checked_sub(U256::from(signed_delta.unsigned_abs()))
    } else {
        liquidity.checked_add(U256::from(signed_delta as u128))
    }
    .ok_or_else(|| arithmetic("active liquidity overflow/underflow"))?;
    if result > WORD_128_MASK {
        return Err(arithmetic("active liquidity exceeds uint128"));
    }
    Ok(result)
}

fn segment_amounts(
    sqrt_a: U256,
    sqrt_b: U256,
    liquidity: U256,
    zero_for_one: bool,
) -> Result<(U256, U256), AdapterEventError> {
    if sqrt_a.is_zero() || sqrt_b.is_zero() {
        return Err(contradiction("segment has zero price"));
    }
    let (lower, upper) = if sqrt_a < sqrt_b {
        (sqrt_a, sqrt_b)
    } else {
        (sqrt_b, sqrt_a)
    };
    if zero_for_one {
        Ok((
            amount0_delta(lower, upper, liquidity, true)?,
            amount1_delta(lower, upper, liquidity, false)?,
        ))
    } else {
        Ok((
            amount1_delta(lower, upper, liquidity, true)?,
            amount0_delta(lower, upper, liquidity, false)?,
        ))
    }
}

fn amount0_delta(
    lower: U256,
    upper: U256,
    liquidity: U256,
    round_up: bool,
) -> Result<U256, AdapterEventError> {
    let numerator = U512::from(liquidity) * U512::from(upper - lower) * U512::from(Q96);
    let denominator = U512::from(upper) * U512::from(lower);
    div_512(numerator, denominator, round_up)
}

fn amount1_delta(
    lower: U256,
    upper: U256,
    liquidity: U256,
    round_up: bool,
) -> Result<U256, AdapterEventError> {
    let numerator = U512::from(liquidity) * U512::from(upper - lower);
    div_512(numerator, U512::from(Q96), round_up)
}

fn fee_for_full_step(amount_in: U256, fee: u32) -> Result<U256, AdapterEventError> {
    let denominator = U256::from(FEE_DENOMINATOR - u64::from(fee));
    mul_div_round_up(amount_in, U256::from(fee), denominator)
}

fn valid_partial_fee(
    amount_in: U256,
    fee_amount: U256,
    fee: u32,
) -> Result<bool, AdapterEventError> {
    let exact_output_fee = fee_for_full_step(amount_in, fee)?;
    if fee_amount == exact_output_fee {
        return Ok(true);
    }
    let total = amount_in
        .checked_add(fee_amount)
        .ok_or_else(|| arithmetic("final-step input overflow"))?;
    let after_fee = mul_div(
        total,
        U256::from(FEE_DENOMINATOR - u64::from(fee)),
        U256::from(FEE_DENOMINATOR),
    )?;
    Ok(after_fee == amount_in)
}

fn mul_div(a: U256, b: U256, denominator: U256) -> Result<U256, AdapterEventError> {
    div_512(
        U512::from(a) * U512::from(b),
        U512::from(denominator),
        false,
    )
}

fn mul_div_round_up(a: U256, b: U256, denominator: U256) -> Result<U256, AdapterEventError> {
    div_512(U512::from(a) * U512::from(b), U512::from(denominator), true)
}

fn div_512(numerator: U512, denominator: U512, round_up: bool) -> Result<U256, AdapterEventError> {
    if denominator.is_zero() {
        return Err(arithmetic("division by zero"));
    }
    let quotient = numerator / denominator;
    let remainder = numerator % denominator;
    let rounded = if round_up && !remainder.is_zero() {
        quotient + U512::from(1)
    } else {
        quotient
    };
    if rounded.bit_len() > 256 {
        Err(arithmetic("value exceeds uint256"))
    } else {
        Ok(rounded.wrapping_to::<U256>())
    }
}

fn checked_sum(values: impl IntoIterator<Item = U256>) -> Result<U256, AdapterEventError> {
    values.into_iter().try_fold(U256::ZERO, |sum, value| {
        sum.checked_add(value)
            .ok_or_else(|| arithmetic("amount sum overflow"))
    })
}

fn sqrt_ratio_at_tick(tick: i32) -> Result<U256, AdapterEventError> {
    if !(-887_272..=887_272).contains(&tick) {
        return Err(contradiction("tick is outside TickMath bounds"));
    }
    let abs_tick = tick.unsigned_abs();
    let mut ratio = if abs_tick & 1 != 0 {
        hex_u256("fffcb933bd6fad37aa2d162d1a594001")
    } else {
        Q128
    };
    for (bit, constant) in [
        (0x2, "fff97272373d413259a46990580e213a"),
        (0x4, "fff2e50f5f656932ef12357cf3c7fdcc"),
        (0x8, "ffe5caca7e10e4e61c3624eaa0941cd0"),
        (0x10, "ffcb9843d60f6159c9db58835c926644"),
        (0x20, "ff973b41fa98c081472e6896dfb254c0"),
        (0x40, "ff2ea16466c96a3843ec78b326b52861"),
        (0x80, "fe5dee046a99a2a811c461f1969c3053"),
        (0x100, "fcbe86c7900a88aedcffc83b479aa3a4"),
        (0x200, "f987a7253ac413176f2b074cf7815e54"),
        (0x400, "f3392b0822b70005940c7a398e4b70f3"),
        (0x800, "e7159475a2c29b7443b29c7fa6e889d9"),
        (0x1000, "d097f3bdfd2022b8845ad8f792aa5825"),
        (0x2000, "a9f746462d870fdf8a65dc1f90e061e5"),
        (0x4000, "70d869a156d2a1b890bb3df62baf32f7"),
        (0x8000, "31be135f97d08fd981231505542fcfa6"),
        (0x10000, "9aa508b5b7a84e1c677de54f3e99bc9"),
        (0x20000, "5d6af8dedb81196699c329225ee604"),
        (0x40000, "2216e584f5fa1ea926041bedfe98"),
        (0x80000, "48a170391f7dc42444e8fa2"),
    ] {
        if abs_tick & bit != 0 {
            ratio = mul_shift_128(ratio, hex_u256(constant))?;
        }
    }
    if tick > 0 {
        ratio = U256::MAX / ratio;
    }
    let remainder_mask = U256::from(u32::MAX);
    Ok((ratio >> 32) + U256::from(((ratio & remainder_mask) != U256::ZERO) as u8))
}

fn validate_parent_slot0(slot0: Slot0) -> Result<(), AdapterEventError> {
    let min_sqrt = sqrt_ratio_at_tick(-887_272)?;
    let max_sqrt = sqrt_ratio_at_tick(887_272)?;
    if slot0.sqrt_price_x96 < min_sqrt || slot0.sqrt_price_x96 >= max_sqrt {
        return Err(contradiction(
            "parent sqrtPriceX96 is outside canonical TickMath bounds",
        ));
    }
    let lower = sqrt_ratio_at_tick(slot0.tick)?;
    let upper = sqrt_ratio_at_tick(slot0.tick + 1)?;
    if slot0.sqrt_price_x96 < lower || slot0.sqrt_price_x96 > upper {
        return Err(contradiction(
            "parent sqrtPriceX96 is inconsistent with parent tick",
        ));
    }
    Ok(())
}

fn validate_final_tick(
    sqrt_price_x96: U256,
    tick: i32,
    zero_for_one: bool,
) -> Result<(), AdapterEventError> {
    let min_sqrt = sqrt_ratio_at_tick(-887_272)?;
    let max_sqrt = sqrt_ratio_at_tick(887_272)?;
    if sqrt_price_x96 <= min_sqrt || sqrt_price_x96 >= max_sqrt {
        return Err(contradiction(
            "final sqrtPriceX96 is outside canonical swap bounds",
        ));
    }
    let lower = sqrt_ratio_at_tick(tick)?;
    let upper = if tick == 887_272 {
        None
    } else {
        Some(sqrt_ratio_at_tick(tick + 1)?)
    };
    let valid = sqrt_price_x96 >= lower
        && upper.is_none_or(|upper| {
            sqrt_price_x96 < upper || (zero_for_one && sqrt_price_x96 == upper)
        });
    if valid {
        Ok(())
    } else {
        Err(contradiction(
            "final sqrtPriceX96 is inconsistent with the event tick",
        ))
    }
}

fn mul_shift_128(a: U256, b: U256) -> Result<U256, AdapterEventError> {
    let result = (U512::from(a) * U512::from(b)) >> 128_usize;
    if result.bit_len() > 256 {
        Err(arithmetic("TickMath overflow"))
    } else {
        Ok(result.wrapping_to::<U256>())
    }
}

fn hex_u256(value: &str) -> U256 {
    U256::from_str_radix(value, 16).expect("checked TickMath constant")
}

fn low_mask(bits: usize) -> U256 {
    U256::MAX >> (256 - bits)
}

fn signed_from_bits(word: U256, bits: usize) -> i64 {
    debug_assert!(bits <= 64);
    let raw = (word & low_mask(bits)).to::<u64>();
    let sign = 1_u64 << (bits - 1);
    if raw & sign == 0 {
        raw as i64
    } else {
        (raw as i128 - (1_i128 << bits)) as i64
    }
}

fn unsigned_bits(value: i64, bits: usize) -> U256 {
    U256::from((value as u64) & ((1_u64 << bits) - 1))
}

fn wrapping_signed_add(a: i64, b: i64, bits: usize) -> i64 {
    signed_from_bits(unsigned_bits(a.wrapping_add(b), bits), bits)
}

fn wrapping_signed_sub(a: i64, b: i64, bits: usize) -> i64 {
    signed_from_bits(unsigned_bits(a.wrapping_sub(b), bits), bits)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    const POOL: Address = Address::new([0x11; 20]);

    #[derive(Default)]
    struct TestState(BTreeMap<(Address, U256), U256>);

    impl TestState {
        fn insert(&mut self, slot: U256, value: U256) {
            self.0.insert((POOL, slot), value);
        }
    }

    impl StateView for TestState {
        fn storage(&self, address: Address, slot: U256) -> Option<U256> {
            self.0.get(&(address, slot)).copied()
        }
    }

    fn context(timestamp: u64) -> AdapterEventContext {
        AdapterEventContext::for_block(100, alloy_primitives::B256::repeat_byte(0xaa), timestamp)
            .with_chain_id(1)
            .with_parent_hash(alloy_primitives::B256::repeat_byte(0xbb))
            .with_transaction_hash(alloy_primitives::B256::repeat_byte(0xcc))
            .with_event_order(1, 2)
    }

    #[test]
    fn slipstream_fee_inference_accepts_unique_zero_fee() {
        let segments = [SlipstreamSegment {
            liquidity: U256::from(1_000_000),
            staked_liquidity: U256::ZERO,
            amount_in: U256::from(1_000_000),
            amount_out: U256::from(1),
            reached_boundary: false,
            crossed_tick: None,
        }];
        assert_eq!(infer_slipstream_fee(&segments, U256::ZERO), Ok(0));
    }

    #[test]
    fn slipstream_fee_inference_rejects_tiny_rounding_ambiguity() {
        let segments = [SlipstreamSegment {
            liquidity: U256::from(1),
            staked_liquidity: U256::ZERO,
            amount_in: U256::ZERO,
            amount_out: U256::ZERO,
            reached_boundary: false,
            crossed_tick: None,
        }];
        assert_eq!(
            infer_slipstream_fee(&segments, U256::from(1)),
            Err(AdapterEventError::V3Transition(
                V3TransitionError::SlipstreamFeeInferenceAmbiguous {
                    first: 1,
                    last: 100_000,
                }
            ))
        );
    }

    fn slot0(
        sqrt: U256,
        tick: i32,
        index: u16,
        cardinality: u16,
        cardinality_next: u16,
        fee_protocol: u8,
    ) -> U256 {
        sqrt | (U256::from((tick as u32) & 0x00ff_ffff) << 160_usize)
            | (U256::from(index) << 184_usize)
            | (U256::from(cardinality) << 200_usize)
            | (U256::from(cardinality_next) << 216_usize)
            | (U256::from(fee_protocol) << 232_usize)
            | (U256::from(1) << 240_usize)
    }

    fn observation(timestamp: u32, tick_cumulative: i64, spl: U256) -> U256 {
        Observation {
            timestamp,
            tick_cumulative,
            seconds_per_liquidity_cumulative_x128: spl,
            initialized: true,
        }
        .encode()
    }

    fn update_value(updates: &[StateUpdate], slot: U256) -> Option<U256> {
        updates.iter().find_map(|update| match update {
            StateUpdate::Slot {
                address,
                slot: actual,
                value,
            } if *address == POOL && *actual == slot => Some(*value),
            _ => None,
        })
    }

    #[test]
    fn tick_math_matches_canonical_boundaries() {
        assert_eq!(sqrt_ratio_at_tick(0).unwrap(), Q96);
        assert_eq!(
            sqrt_ratio_at_tick(-887_272).unwrap(),
            U256::from(4_295_128_739_u64)
        );
        assert_eq!(
            sqrt_ratio_at_tick(887_272).unwrap(),
            U256::from_str_radix("fffd8963efd1fc6a506488495d951d5263988d26", 16).unwrap()
        );
    }

    #[test]
    fn transition_work_budget_fails_closed_with_typed_error() {
        let mut steps = MAX_SWAP_STEPS;
        assert_eq!(
            consume_step(&mut steps),
            Err(AdapterEventError::V3Transition(
                V3TransitionError::WorkLimitExceeded {
                    limit: MAX_SWAP_STEPS,
                },
            ))
        );
    }

    #[test]
    fn initialized_crossing_updates_fee_oracle_liquidity_and_all_tick_words() {
        let layout = V3StorageLayout::uniswap(10);
        let liquidity = U256::from(1_000_000_000_000_000_000_u128);
        let liquidity_net = 100_000_000_000_000_000_i128;
        let final_sqrt = sqrt_ratio_at_tick(10).unwrap();
        let (principal_in, amount_out) =
            segment_amounts(Q96, final_sqrt, liquidity, false).unwrap();
        let fee = fee_for_full_step(principal_in, 3_000).unwrap();

        let mut state = TestState::default();
        state.insert(layout.slot0_slot, slot0(Q96, 0, 0, 2, 2, 0x40));
        state.insert(FEE_GROWTH_0_SLOT, U256::from(11));
        state.insert(FEE_GROWTH_1_SLOT, U256::from(22));
        state.insert(PROTOCOL_FEES_SLOT, U256::ZERO);
        state.insert(layout.liquidity_slot, liquidity);
        state.insert(OBSERVATIONS_BASE_SLOT, observation(100, 5, U256::from(7)));
        let bitmap_slot = v3_tick_bitmap_storage_key_with_base(0, layout.tick_bitmap_base_slot);
        state.insert(bitmap_slot, U256::from(1) << 1_usize);
        let tick_keys = v3_tick_info_storage_keys_with_base(10, layout.ticks_base_slot);
        state.insert(
            tick_keys[0],
            U256::from(liquidity_net as u128) | (U256::from(liquidity_net as u128) << 128_usize),
        );
        state.insert(tick_keys[1], U256::from(3));
        state.insert(tick_keys[2], U256::from(4));
        state.insert(tick_keys[3], U256::from(1) << 248_usize);

        let updates = derive_uniswap_v3_swap(
            POOL,
            layout,
            3_000,
            DecodedSwap {
                amount0_negative: true,
                amount0: amount_out,
                amount1_negative: false,
                amount1: principal_in + fee,
                sqrt_price_x96: final_sqrt,
                liquidity: liquidity + U256::from(liquidity_net as u128),
                tick: 10,
            },
            &state,
            &context(110),
        )
        .unwrap();

        let protocol_fee = fee / U256::from(4);
        let lp_growth = mul_div(fee - protocol_fee, Q128, liquidity).unwrap();
        assert_eq!(
            update_value(&updates, FEE_GROWTH_0_SLOT),
            Some(U256::from(11))
        );
        assert_eq!(
            update_value(&updates, FEE_GROWTH_1_SLOT),
            Some(U256::from(22).wrapping_add(lp_growth))
        );
        assert_eq!(
            update_value(&updates, PROTOCOL_FEES_SLOT),
            Some(protocol_fee << 128_usize)
        );
        assert_eq!(
            update_value(&updates, layout.liquidity_slot),
            Some(liquidity + U256::from(liquidity_net as u128))
        );
        assert_eq!(
            update_value(&updates, tick_keys[0]),
            state.storage(POOL, tick_keys[0])
        );
        assert_eq!(
            update_value(&updates, tick_keys[1]),
            Some(U256::from(11).wrapping_sub(U256::from(3)))
        );
        assert_eq!(
            update_value(&updates, tick_keys[2]),
            Some(
                U256::from(22)
                    .wrapping_add(lp_growth)
                    .wrapping_sub(U256::from(4))
            )
        );
        let now_spl = U256::from(7) + (U256::from(10) << 128_usize) / liquidity;
        let expected_tick_word = (U256::from(5) & WORD_56_MASK)
            | ((now_spl & WORD_160_MASK) << 56_usize)
            | (U256::from(110) << 216_usize)
            | (U256::from(1) << 248_usize);
        assert_eq!(
            update_value(&updates, tick_keys[3]),
            Some(expected_tick_word)
        );
        assert_eq!(
            update_value(&updates, OBSERVATIONS_BASE_SLOT + U256::from(1)),
            Some(observation(110, 5, now_spl))
        );
    }

    #[test]
    fn zero_liquidity_gap_crosses_without_fee_growth_before_liquidity_activates() {
        let layout = V3StorageLayout::uniswap(1);
        let active_liquidity = U256::from(1_000_000_000_000_000_000_u128);
        let crossing_sqrt = sqrt_ratio_at_tick(10).unwrap();
        let final_sqrt = sqrt_ratio_at_tick(20).unwrap();
        let (principal, output) =
            segment_amounts(crossing_sqrt, final_sqrt, active_liquidity, false).unwrap();
        let fee = fee_for_full_step(principal, 3_000).unwrap();

        let mut state = TestState::default();
        state.insert(layout.slot0_slot, slot0(Q96, 0, 0, 1, 1, 0));
        state.insert(FEE_GROWTH_0_SLOT, U256::from(11));
        state.insert(FEE_GROWTH_1_SLOT, U256::from(22));
        state.insert(PROTOCOL_FEES_SLOT, U256::ZERO);
        state.insert(layout.liquidity_slot, U256::ZERO);
        state.insert(OBSERVATIONS_BASE_SLOT, observation(100, 5, U256::from(7)));
        state.insert(
            v3_tick_bitmap_storage_key_with_base(0, layout.tick_bitmap_base_slot),
            U256::from(1) << 10_usize,
        );
        let tick_keys = v3_tick_info_storage_keys_with_base(10, layout.ticks_base_slot);
        state.insert(
            tick_keys[0],
            active_liquidity | (active_liquidity << 128_usize),
        );
        state.insert(tick_keys[1], U256::from(3));
        state.insert(tick_keys[2], U256::from(4));
        state.insert(tick_keys[3], U256::from(1) << 248_usize);

        let updates = derive_uniswap_v3_swap(
            POOL,
            layout,
            3_000,
            DecodedSwap {
                amount0_negative: true,
                amount0: output,
                amount1_negative: false,
                amount1: principal + fee,
                sqrt_price_x96: final_sqrt,
                liquidity: active_liquidity,
                tick: 20,
            },
            &state,
            &context(110),
        )
        .unwrap();

        assert_eq!(
            update_value(&updates, FEE_GROWTH_1_SLOT),
            Some(U256::from(22).wrapping_add(mul_div(fee, Q128, active_liquidity).unwrap()))
        );
        assert_eq!(
            update_value(&updates, layout.liquidity_slot),
            Some(active_liquidity)
        );
    }

    #[test]
    fn oracle_grows_cardinality_wraps_and_does_not_write_twice_at_same_timestamp() {
        let mut state = TestState::default();
        state.insert(
            OBSERVATIONS_BASE_SLOT + U256::from(1),
            observation(100, -10, U256::from(9)),
        );
        let parent = Slot0::decode(slot0(Q96, -2, 1, 2, 4, 0));
        let (index, cardinality, write, now) = advance_oracle(
            POOL,
            UNISWAP_V3_SURFACE,
            parent,
            U256::from(100),
            &state,
            &context(105),
        )
        .unwrap();
        assert_eq!((index, cardinality), (2, 4));
        assert_eq!(write.unwrap().0, OBSERVATIONS_BASE_SLOT + U256::from(2));
        assert_eq!(now.tick_cumulative, -20);

        state.insert(
            OBSERVATIONS_BASE_SLOT + U256::from(3),
            observation(105, 20, U256::from(30)),
        );
        let wrap = Slot0::decode(slot0(Q96, 3, 3, 4, 4, 0));
        let (index, cardinality, write, _) = advance_oracle(
            POOL,
            UNISWAP_V3_SURFACE,
            wrap,
            U256::from(100),
            &state,
            &context(110),
        )
        .unwrap();
        assert_eq!((index, cardinality), (0, 4));
        assert_eq!(write.unwrap().0, OBSERVATIONS_BASE_SLOT);

        state.insert(OBSERVATIONS_BASE_SLOT, observation(110, 35, U256::from(40)));
        let same_timestamp = Slot0::decode(slot0(Q96, 4, 0, 4, 4, 0));
        let (index, cardinality, write, now) = advance_oracle(
            POOL,
            UNISWAP_V3_SURFACE,
            same_timestamp,
            U256::from(100),
            &state,
            &context(110),
        )
        .unwrap();
        assert_eq!((index, cardinality), (0, 4));
        assert_eq!(write, None);
        assert_eq!(now.tick_cumulative, 35);
    }

    #[test]
    fn missing_bitmap_and_tick_info_fail_closed_with_typed_errors() {
        let layout = V3StorageLayout::uniswap(10);
        let liquidity = U256::from(1_000_000_u64);
        let final_sqrt = sqrt_ratio_at_tick(10).unwrap();
        let (principal, output) = segment_amounts(Q96, final_sqrt, liquidity, false).unwrap();
        let swap = DecodedSwap {
            amount0_negative: true,
            amount0: output,
            amount1_negative: false,
            amount1: principal + fee_for_full_step(principal, 3_000).unwrap(),
            sqrt_price_x96: final_sqrt,
            liquidity,
            tick: 10,
        };
        let mut state = TestState::default();
        state.insert(layout.slot0_slot, slot0(Q96, 0, 0, 1, 1, 0));
        state.insert(FEE_GROWTH_0_SLOT, U256::ZERO);
        state.insert(FEE_GROWTH_1_SLOT, U256::ZERO);
        state.insert(PROTOCOL_FEES_SLOT, U256::ZERO);
        state.insert(layout.liquidity_slot, liquidity);
        state.insert(OBSERVATIONS_BASE_SLOT, observation(100, 0, U256::ZERO));

        let bitmap_slot = v3_tick_bitmap_storage_key_with_base(0, layout.tick_bitmap_base_slot);
        assert_eq!(
            derive_uniswap_v3_swap(POOL, layout, 3_000, swap, &state, &context(110)),
            Err(AdapterEventError::MissingState {
                address: POOL,
                slot: bitmap_slot,
            })
        );

        state.insert(bitmap_slot, U256::from(1) << 1_usize);
        let first_tick_word = v3_tick_info_storage_keys_with_base(10, layout.ticks_base_slot)[0];
        assert_eq!(
            derive_uniswap_v3_swap(POOL, layout, 3_000, swap, &state, &context(110)),
            Err(AdapterEventError::MissingState {
                address: POOL,
                slot: first_tick_word,
            })
        );
    }

    #[test]
    fn partial_fee_accepts_exact_input_and_output_rounding_only() {
        for fee in [100_u32, 500, 3_000, 10_000] {
            for amount in [U256::from(1), U256::from(17), U256::from(1_000_003)] {
                let exact_output = fee_for_full_step(amount, fee).unwrap();
                assert!(valid_partial_fee(amount, exact_output, fee).unwrap());
                let bad = exact_output + U256::from(2);
                assert!(!valid_partial_fee(amount, bad, fee).unwrap());
            }
        }
    }

    #[test]
    fn partial_price_steps_accept_exact_input_and_exact_output_in_both_directions() {
        let layout = V3StorageLayout::uniswap(1);
        let start_tick = 100;
        let start = sqrt_ratio_at_tick(start_tick).unwrap();
        for (zero_for_one, final_tick, liquidity) in [
            (true, 90, U256::from(1_000_000_295_037_u64)),
            (false, 110, U256::from(1_000_000_652_421_u64)),
        ] {
            let final_sqrt = sqrt_ratio_at_tick(final_tick).unwrap();
            let (principal, output) =
                segment_amounts(start, final_sqrt, liquidity, zero_for_one).unwrap();
            let exact_output_fee = fee_for_full_step(principal, 3_000).unwrap();
            let exact_input_fee = exact_output_fee + U256::from(1);
            assert!(valid_partial_fee(principal, exact_input_fee, 3_000).unwrap());

            let mut state = TestState::default();
            state.insert(layout.slot0_slot, slot0(start, start_tick, 0, 1, 1, 0));
            state.insert(FEE_GROWTH_0_SLOT, U256::ZERO);
            state.insert(FEE_GROWTH_1_SLOT, U256::ZERO);
            state.insert(PROTOCOL_FEES_SLOT, U256::ZERO);
            state.insert(layout.liquidity_slot, liquidity);
            state.insert(OBSERVATIONS_BASE_SLOT, observation(100, 0, U256::ZERO));
            state.insert(
                v3_tick_bitmap_storage_key_with_base(0, layout.tick_bitmap_base_slot),
                U256::ZERO,
            );

            for fee_amount in [exact_output_fee, exact_input_fee] {
                let (amount0_negative, amount0, amount1_negative, amount1) = if zero_for_one {
                    (false, principal + fee_amount, true, output)
                } else {
                    (true, output, false, principal + fee_amount)
                };
                let updates = derive_uniswap_v3_swap(
                    POOL,
                    layout,
                    3_000,
                    DecodedSwap {
                        amount0_negative,
                        amount0,
                        amount1_negative,
                        amount1,
                        sqrt_price_x96: final_sqrt,
                        liquidity,
                        tick: final_tick,
                    },
                    &state,
                    &context(110),
                )
                .unwrap();
                let fee_growth_slot = if zero_for_one {
                    FEE_GROWTH_0_SLOT
                } else {
                    FEE_GROWTH_1_SLOT
                };
                assert_eq!(
                    update_value(&updates, fee_growth_slot),
                    Some(mul_div(fee_amount, Q128, liquidity).unwrap())
                );
            }
        }
    }

    #[test]
    fn one_for_zero_preserves_empty_word_step_rounding() {
        let layout = V3StorageLayout::uniswap(1);
        let start = sqrt_ratio_at_tick(0).unwrap();
        let boundary = sqrt_ratio_at_tick(255).unwrap();
        let final_sqrt = sqrt_ratio_at_tick(300).unwrap();
        let liquidity = (1_000_000_000_000_u64..1_000_000_100_000)
            .map(U256::from)
            .find(|liquidity| {
                let (input0, _) = segment_amounts(start, boundary, *liquidity, false).unwrap();
                let (input1, _) = segment_amounts(boundary, final_sqrt, *liquidity, false).unwrap();
                let fee0 = fee_for_full_step(input0, 3_000).unwrap();
                let fee1 = fee_for_full_step(input1, 3_000).unwrap();
                mul_div(fee0, Q128, *liquidity)
                    .unwrap()
                    .wrapping_add(mul_div(fee1, Q128, *liquidity).unwrap())
                    != mul_div(fee0 + fee1, Q128, *liquidity).unwrap()
            })
            .expect("deterministic fixture with a visible per-step rounding difference");
        let (input0, output0) = segment_amounts(start, boundary, liquidity, false).unwrap();
        let (input1, output1) = segment_amounts(boundary, final_sqrt, liquidity, false).unwrap();
        let fee0 = fee_for_full_step(input0, 3_000).unwrap();
        let fee1 = fee_for_full_step(input1, 3_000).unwrap();

        let mut state = TestState::default();
        state.insert(layout.slot0_slot, slot0(start, 0, 0, 1, 1, 0));
        state.insert(FEE_GROWTH_0_SLOT, U256::ZERO);
        state.insert(FEE_GROWTH_1_SLOT, U256::ZERO);
        state.insert(PROTOCOL_FEES_SLOT, U256::ZERO);
        state.insert(layout.liquidity_slot, liquidity);
        state.insert(OBSERVATIONS_BASE_SLOT, observation(100, 0, U256::ZERO));
        state.insert(
            v3_tick_bitmap_storage_key_with_base(0, layout.tick_bitmap_base_slot),
            U256::ZERO,
        );
        state.insert(
            v3_tick_bitmap_storage_key_with_base(1, layout.tick_bitmap_base_slot),
            U256::ZERO,
        );

        let updates = derive_uniswap_v3_swap(
            POOL,
            layout,
            3_000,
            DecodedSwap {
                amount0_negative: true,
                amount0: output0 + output1,
                amount1_negative: false,
                amount1: input0 + input1 + fee0 + fee1,
                sqrt_price_x96: final_sqrt,
                liquidity,
                tick: 300,
            },
            &state,
            &context(110),
        )
        .unwrap();
        let expected_growth = mul_div(fee0, Q128, liquidity)
            .unwrap()
            .wrapping_add(mul_div(fee1, Q128, liquidity).unwrap());
        assert_eq!(
            update_value(&updates, FEE_GROWTH_1_SLOT),
            Some(expected_growth)
        );
        assert_ne!(
            expected_growth,
            mul_div(fee0 + fee1, Q128, liquidity).unwrap(),
            "fixture must detect collapsing two canonical steps into one"
        );
    }

    #[test]
    fn zero_for_one_preserves_empty_word_step_rounding() {
        let layout = V3StorageLayout::uniswap(1);
        let liquidity = U256::from(1_000_000_000_003_u64);
        let start = sqrt_ratio_at_tick(300).unwrap();
        let boundary = sqrt_ratio_at_tick(256).unwrap();
        let final_sqrt = sqrt_ratio_at_tick(10).unwrap();
        let (input0, output0) = segment_amounts(start, boundary, liquidity, true).unwrap();
        let (input1, output1) = segment_amounts(boundary, final_sqrt, liquidity, true).unwrap();
        let fee0 = fee_for_full_step(input0, 3_000).unwrap();
        let fee1 = fee_for_full_step(input1, 3_000).unwrap();

        let mut state = TestState::default();
        state.insert(layout.slot0_slot, slot0(start, 300, 0, 1, 1, 0));
        state.insert(FEE_GROWTH_0_SLOT, U256::ZERO);
        state.insert(FEE_GROWTH_1_SLOT, U256::ZERO);
        state.insert(PROTOCOL_FEES_SLOT, U256::ZERO);
        state.insert(layout.liquidity_slot, liquidity);
        state.insert(OBSERVATIONS_BASE_SLOT, observation(100, 0, U256::ZERO));
        state.insert(
            v3_tick_bitmap_storage_key_with_base(1, layout.tick_bitmap_base_slot),
            U256::ZERO,
        );
        state.insert(
            v3_tick_bitmap_storage_key_with_base(0, layout.tick_bitmap_base_slot),
            U256::ZERO,
        );

        let updates = derive_uniswap_v3_swap(
            POOL,
            layout,
            3_000,
            DecodedSwap {
                amount0_negative: false,
                amount0: input0 + input1 + fee0 + fee1,
                amount1_negative: true,
                amount1: output0 + output1,
                sqrt_price_x96: final_sqrt,
                liquidity,
                tick: 10,
            },
            &state,
            &context(110),
        )
        .unwrap();
        let expected_growth = mul_div(fee0, Q128, liquidity)
            .unwrap()
            .wrapping_add(mul_div(fee1, Q128, liquidity).unwrap());
        assert_eq!(
            update_value(&updates, FEE_GROWTH_0_SLOT),
            Some(expected_growth)
        );
        assert_ne!(
            expected_growth,
            mul_div(fee0 + fee1, Q128, liquidity).unwrap(),
            "fixture must detect collapsing two canonical steps into one"
        );
    }

    #[test]
    fn tiny_exact_input_can_be_entirely_consumed_as_fee_after_zero_distance_boundary() {
        let layout = V3StorageLayout::uniswap(1);
        let liquidity = U256::from(1_000_000_000_000_000_000_u128);
        let mut state = TestState::default();
        state.insert(layout.slot0_slot, slot0(Q96, 0, 0, 1, 1, 0));
        state.insert(FEE_GROWTH_0_SLOT, U256::from(5));
        state.insert(FEE_GROWTH_1_SLOT, U256::from(7));
        state.insert(PROTOCOL_FEES_SLOT, U256::ZERO);
        state.insert(layout.liquidity_slot, liquidity);
        state.insert(OBSERVATIONS_BASE_SLOT, observation(100, 0, U256::ZERO));
        state.insert(
            v3_tick_bitmap_storage_key_with_base(0, layout.tick_bitmap_base_slot),
            U256::ZERO,
        );

        // Canonical SwapMath exact-input handling first floors
        // `amountRemaining * (1e6-fee) / 1e6`. For amount=1 and fee=3000
        // that is zero, so sqrt/tick/output remain unchanged and the entire
        // input becomes feeAmount.
        let updates = derive_uniswap_v3_swap(
            POOL,
            layout,
            3_000,
            DecodedSwap {
                amount0_negative: false,
                amount0: U256::from(1),
                amount1_negative: false,
                amount1: U256::ZERO,
                sqrt_price_x96: Q96,
                liquidity,
                tick: -1,
            },
            &state,
            &context(110),
        )
        .unwrap();

        assert_eq!(
            update_value(&updates, FEE_GROWTH_0_SLOT),
            Some(U256::from(5).wrapping_add(mul_div(U256::from(1), Q128, liquidity).unwrap()))
        );
        assert_eq!(
            update_value(&updates, FEE_GROWTH_1_SLOT),
            Some(U256::from(7))
        );
        assert_eq!(
            (update_value(&updates, layout.slot0_slot).unwrap() >> 160_usize)
                & U256::from(0x00ff_ffff_u32),
            U256::from(0x00ff_ffff_u32),
            "canonical zero-distance boundary changes tick from 0 to -1"
        );
        assert!(update_value(&updates, OBSERVATIONS_BASE_SLOT).is_some());
    }

    #[test]
    fn exact_stop_at_empty_word_boundary_uses_full_step_fee() {
        let layout = V3StorageLayout::uniswap(1);
        let liquidity = U256::from(1_000_000_000_003_u64);
        let final_sqrt = sqrt_ratio_at_tick(255).unwrap();
        let (principal, output) = segment_amounts(Q96, final_sqrt, liquidity, false).unwrap();
        let fee = fee_for_full_step(principal, 3_000).unwrap();
        let mut state = TestState::default();
        state.insert(layout.slot0_slot, slot0(Q96, 0, 0, 1, 1, 0));
        state.insert(FEE_GROWTH_0_SLOT, U256::ZERO);
        state.insert(FEE_GROWTH_1_SLOT, U256::ZERO);
        state.insert(PROTOCOL_FEES_SLOT, U256::ZERO);
        state.insert(layout.liquidity_slot, liquidity);
        state.insert(OBSERVATIONS_BASE_SLOT, observation(100, 0, U256::ZERO));
        state.insert(
            v3_tick_bitmap_storage_key_with_base(0, layout.tick_bitmap_base_slot),
            U256::ZERO,
        );
        let updates = derive_uniswap_v3_swap(
            POOL,
            layout,
            3_000,
            DecodedSwap {
                amount0_negative: true,
                amount0: output,
                amount1_negative: false,
                amount1: principal + fee,
                sqrt_price_x96: final_sqrt,
                liquidity,
                tick: 255,
            },
            &state,
            &context(110),
        )
        .unwrap();
        assert_eq!(
            update_value(&updates, FEE_GROWTH_1_SLOT),
            Some(mul_div(fee, Q128, liquidity).unwrap())
        );
    }
}
