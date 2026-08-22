use super::bytecode::{
    AdapterCodeSeed, BytecodeTemplateError, uniswap_v3_max_liquidity_per_tick,
    v3_code_seed_from_metadata,
};
use super::cold_start::{
    AdapterColdStartPlanner, ColdStartPlan, ColdStartResults, ColdStartRunReport, ColdStartStep,
    SlotFetch,
};
use super::factory::{ConcentratedLiquidityFactory, FactoryConfig, PoolFactory};
use super::sim::{
    ISlipstreamQuoter, QuoteExactInputSingleParams, SimConfig, SimError,
    SlipstreamQuoteExactInputSingleParams, SwapQuote, quote_via_call_with_code_overrides_from,
    quoteExactInputSingleCall,
};
use super::v3_transition::{
    DecodedCollectProtocol, DecodedFeeProtocol, DecodedFlash, DecodedLiquidity,
    DecodedObservationGrowth, DecodedSwap, derive_slipstream_liquidity, derive_slipstream_swap,
    derive_uniswap_v3_collect_protocol, derive_uniswap_v3_flash, derive_uniswap_v3_liquidity,
    derive_uniswap_v3_observation_growth, derive_uniswap_v3_set_fee_protocol,
    derive_uniswap_v3_swap, infer_slipstream_swap_fee, uniswap_v3_position_slots,
    validate_reviewed_slipstream_event,
};
use super::{
    AdapterCache, AdapterEvent, AdapterEventContext, AdapterEventError, AdapterEventKind,
    AdapterEventResult, AmmAdapter, ColdStartOutcome, ColdStartPolicy, ColdStartReport,
    DeferredWork, EventSource, PoolRegistration, PoolStateDependencies, PoolStatus, ProtocolId,
    ProtocolMetadata, PurgeScope, RepairAction, SlipstreamRuntimeFamily,
    SlipstreamSnapshotIdentity, SlipstreamUnstakedFeeEvaluation,
    SlipstreamUnstakedFeeEvaluationError, SlipstreamUnstakedFeeProof, SlotChange, StateDiff,
    StateSlot, StateUpdate, StateView, UnsupportedReason, UpdateQuality,
    V3LiquidityTransitionCapability, V3Metadata, V3SwapTransitionCapability,
};
use crate::adapters::storage::{
    V3StorageLayout, layout_for, slipstream_tick_info_storage_keys_with_base,
    v3_tick_bitmap_storage_key_with_base, v3_tick_info_storage_keys_with_base, v3_word_position,
};
use alloy_primitives::{
    Address, B256, Bytes, Log, U256,
    aliases::{I24, U24},
    keccak256,
};
use alloy_sol_types::{SolCall, SolEvent};
use evm_fork_cache::cache::{EvmOverlay, EvmSnapshot};
use revm::context::result::ExecutionResult;
use std::{collections::BTreeSet, sync::Arc};

/// `sol!`-generated pool event bindings (crate-internal, not public API).
mod abi {
    alloy_sol_types::sol! {
        event Initialize(uint160 sqrtPriceX96, int24 tick);
        event Swap(address indexed sender, address indexed recipient, int256 amount0, int256 amount1, uint160 sqrtPriceX96, uint128 liquidity, int24 tick);
        event Mint(address sender, address indexed owner, int24 indexed tickLower, int24 indexed tickUpper, uint128 amount, uint256 amount0, uint256 amount1);
        event Collect(address indexed owner, address recipient, int24 indexed tickLower, int24 indexed tickUpper, uint128 amount0, uint128 amount1);
        event Burn(address indexed owner, int24 indexed tickLower, int24 indexed tickUpper, uint128 amount, uint256 amount0, uint256 amount1);
        event Flash(address indexed sender, address indexed recipient, uint256 amount0, uint256 amount1, uint256 paid0, uint256 paid1);
        event IncreaseObservationCardinalityNext(uint16 observationCardinalityNextOld, uint16 observationCardinalityNextNew);
        event SetFeeProtocol(uint8 feeProtocol0Old, uint8 feeProtocol1Old, uint8 feeProtocol0New, uint8 feeProtocol1New);
        event CollectProtocol(address indexed sender, address indexed recipient, uint128 amount0, uint128 amount1);
    }
}
use abi::{
    Burn, Collect, CollectProtocol, Flash, IncreaseObservationCardinalityNext, Initialize, Mint,
    SetFeeProtocol, Swap,
};

const SLOT0_TICK_SHIFT: usize = 160;

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

/// Minimal runtime that ABI-returns `true` for any call.
///
/// QuoterV2 obtains its answer by executing `pool.swap`; the pool transfers the
/// output token, then the quoter callback deliberately reverts with quote data.
/// An immutable AMM snapshot intentionally does not carry arbitrary ERC20
/// balance mappings, so snapshot-backed quote execution temporarily substitutes
/// this runtime for both path tokens. Pool math and tick traversal remain real;
/// only the transfer side effect that precedes the intentional revert is
/// neutralized. Live-backed caches ignore the override and execute real token
/// code/state through their lazy backend.
const ERC20_TRANSFER_SUCCESS_RUNTIME: &[u8] = &[
    0x60, 0x01, // PUSH1 1
    0x60, 0x00, // PUSH1 0
    0x52, // MSTORE
    0x60, 0x20, // PUSH1 32
    0x60, 0x00, // PUSH1 0
    0xf3, // RETURN
];

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

/// Adapter for the Uniswap V3 concentrated-liquidity family.
///
/// A single instance routes Uniswap V3, Pancake V3, and Slipstream and resolves
/// their configured storage layouts. Layout similarity is not semantic parity:
/// canonical Uniswap V3 has registration-scoped exact `Swap` transitions, while
/// the reviewed Base/Optimism Slipstream deployments have event-scoped exact
/// quote/search transitions for `Swap`, `Mint`, `Burn`, and `Collect`.
/// Pancake and unreviewed Slipstream pools invalidate stale state and request
/// repair until their own deployed-runtime semantics are proven.
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
            .map(|address| EventSource::direct(address, v3_mutating_event_topics(pool.protocol())))
            .into_iter()
            .collect()
    }

    fn state_dependencies(&self, pool: &PoolRegistration) -> PoolStateDependencies {
        let Some(address) = pool.key.address() else {
            return PoolStateDependencies::default();
        };
        let mut slots = v3_metadata(pool)
            .map(|metadata| metadata.warmed_slots.clone())
            .unwrap_or_default();
        if slots.is_empty()
            && let Some(layout) = layout_for(pool)
        {
            slots.extend([layout.slot0_slot, layout.liquidity_slot]);
        }
        let external_slots = self
            .verified_storage_targets(pool)
            .into_iter()
            .map(|(address, slot)| StateSlot::new(address, slot));
        PoolStateDependencies::default()
            .with_associated_addresses([address])
            .with_slots(
                slots
                    .into_iter()
                    .map(|slot| StateSlot::new(address, slot))
                    .chain(external_slots),
            )
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

        let exact_surface = if pool.protocol() == ProtocolId::UniswapV3
            && layout == V3StorageLayout::uniswap(layout.tick_spacing)
        {
            V3ColdStartExactSurface::CanonicalUniswap
        } else if pool.protocol() == ProtocolId::Slipstream
            && layout == V3StorageLayout::slipstream(layout.tick_spacing)
        {
            V3ColdStartExactSurface::Slipstream
        } else {
            V3ColdStartExactSurface::None
        };
        Ok(Box::new(UniswapV3ColdStartPlanner::new(
            address,
            layout,
            policy,
            radius,
            exact_surface,
            self.verified_storage_targets(pool),
        )))
    }

    fn code_seeds(
        &self,
        pool: &PoolRegistration,
    ) -> Result<Vec<AdapterCodeSeed>, BytecodeTemplateError> {
        if matches!(
            pool.protocol(),
            ProtocolId::PancakeV3 | ProtocolId::Slipstream
        ) {
            return Ok(Vec::new());
        }
        let Some(address) = pool.key.address() else {
            return Ok(Vec::new());
        };
        let Some(metadata) = v3_metadata(pool) else {
            return Ok(Vec::new());
        };
        v3_code_seed_from_metadata(address, metadata).map(|opt| opt.into_iter().collect())
    }

    fn verified_code_targets(&self, pool: &PoolRegistration) -> Vec<Address> {
        let Some(address) = pool.key.address() else {
            return Vec::new();
        };
        match pool.protocol() {
            ProtocolId::PancakeV3 => vec![address],
            ProtocolId::Slipstream => {
                let Some(reviewed) = reviewed_slipstream_fee_runtime_for_pool(address) else {
                    return vec![address];
                };
                vec![
                    address,
                    reviewed.implementation,
                    reviewed.factory,
                    reviewed.voter,
                    reviewed.module,
                ]
            }
            _ => Vec::new(),
        }
    }

    fn verified_storage_targets(&self, pool: &PoolRegistration) -> Vec<(Address, U256)> {
        if pool.protocol() != ProtocolId::Slipstream {
            return Vec::new();
        }
        let Some(address) = pool.key.address() else {
            return Vec::new();
        };
        reviewed_slipstream_fee_runtime_for_pool(address)
            .map(reviewed_slipstream_fee_storage_targets)
            .unwrap_or_default()
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

        if topic0 == Swap::SIGNATURE_HASH || topic0 == PancakeV3Swap::SIGNATURE_HASH {
            self.decode_swap_without_context(pool, log, topic0)
        } else if topic0 == Mint::SIGNATURE_HASH {
            self.decode_liquidity_event(pool, log, view, true)
        } else if topic0 == Burn::SIGNATURE_HASH {
            self.decode_liquidity_event(pool, log, view, false)
        } else if canonical_v3_non_swap_mutation_topics().contains(&topic0) {
            self.decode_non_swap_mutation(pool, log, topic0)
        } else {
            AdapterEventResult::ignored()
        }
    }

    fn decode_event_with_context(
        &self,
        pool: &PoolRegistration,
        log: &Log,
        view: &dyn StateView,
        context: &AdapterEventContext,
    ) -> AdapterEventResult {
        let Some(topic0) = log.topics().first().copied() else {
            return AdapterEventResult::ignored();
        };

        if topic0 == Swap::SIGNATURE_HASH {
            if Self::swap_transition_capability_with_context(pool, context)
                == V3SwapTransitionCapability::Exact
            {
                self.decode_swap_exact(pool, log, topic0, view, context)
            } else {
                self.decode_swap_without_context(pool, log, topic0)
            }
        } else if topic0 == PancakeV3Swap::SIGNATURE_HASH {
            self.decode_swap_without_context(pool, log, topic0)
        } else if topic0 == Mint::SIGNATURE_HASH {
            self.decode_liquidity_event_with_context(pool, log, view, true, context)
        } else if topic0 == Burn::SIGNATURE_HASH {
            self.decode_liquidity_event_with_context(pool, log, view, false, context)
        } else if topic0 == Collect::SIGNATURE_HASH {
            self.decode_collect_with_context(pool, log, context)
        } else if canonical_v3_accounting_topics().contains(&topic0) {
            self.decode_canonical_accounting_event(pool, log, view, topic0, context)
        } else {
            self.decode_event(pool, log, view)
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
    /// Fee-keyed pools take `fee` from metadata; Slipstream instead takes the
    /// signed `tick_spacing` used by its native quoter ABI. Tick-crossing swaps
    /// stay correct because the cache lazily fetches any cold tick/bitmap slot
    /// from the backend.
    ///
    /// The quote target is the pool's own [`V3Metadata::quoter`] when set (a
    /// fork's QuoterV2, e.g. PancakeSwap's, filled in by factory discovery),
    /// falling back to the caller's [`SimConfig::v3_quoter`] otherwise. The
    /// adapter selects the fee-keyed struct for Uniswap/Pancake and the native
    /// signed-tick-spacing struct for Slipstream. A discovered Slipstream pool
    /// must therefore be paired with a compatible Slipstream quote target.
    fn simulate_swap(
        &self,
        pool: &PoolRegistration,
        cache: &mut dyn AdapterCache,
        token_in: Address,
        token_out: Address,
        amount_in: U256,
        config: &SimConfig,
    ) -> Result<SwapQuote, SimError> {
        // Fail closed before encoding: the two lines below read the venue from
        // *different* places — `v3_metadata` accepts any V3-family metadata
        // variant, while the calldata shape is chosen from the pool key. A
        // registration naming two venues would therefore pair one family's
        // metadata with another family's ABI (Uniswap's `uint24 fee` struct
        // against a Slipstream quoter, or the reverse), and its storage was
        // already warmed through `layout_for`'s metadata-derived layout. A
        // registry rejects such a pool up front; this guard covers a caller
        // holding the adapter directly.
        pool.check_protocol_agreement()
            .map_err(|mismatch| SimError::Custom(mismatch.to_string()))?;
        // Same resolver `ProtocolMetadata::quote_code_targets` uses, so the quoter
        // we call is always the one an eager cold-start pre-warmed.
        let quoter = v3_metadata(pool).map_or(config.v3_quoter, |m| m.quote_target(config));
        let slipstream = pool.protocol() == ProtocolId::Slipstream;
        let calldata = if slipstream {
            let tick_spacing = v3_metadata(pool)
                .and_then(|metadata| metadata.tick_spacing)
                .ok_or(SimError::MissingMetadata("Slipstream tick spacing"))?;
            let tick_spacing = I24::try_from(tick_spacing).map_err(|_| {
                SimError::Custom("Slipstream tick spacing exceeds signed int24".to_owned())
            })?;
            Bytes::from(
                ISlipstreamQuoter::quoteExactInputSingleCall {
                    params: SlipstreamQuoteExactInputSingleParams {
                        tokenIn: token_in,
                        tokenOut: token_out,
                        amountIn: amount_in,
                        tickSpacing: tick_spacing,
                        sqrtPriceLimitX96: U256::ZERO.to(),
                    },
                }
                .abi_encode(),
            )
        } else {
            let fee = v3_fee(pool).ok_or(SimError::MissingMetadata("V3 fee"))?;
            Bytes::from(
                quoteExactInputSingleCall {
                    params: QuoteExactInputSingleParams {
                        tokenIn: token_in,
                        tokenOut: token_out,
                        amountIn: amount_in,
                        fee: U24::from(fee),
                        sqrtPriceLimitX96: U256::ZERO.to(),
                    },
                }
                .abi_encode(),
            )
        };

        let transfer_success = Bytes::from_static(ERC20_TRANSFER_SUCCESS_RUNTIME);
        let code_overrides = [
            (token_in, transfer_success.clone()),
            (token_out, transfer_success),
        ];
        let output = quote_via_call_with_code_overrides_from(
            cache,
            config.from,
            quoter,
            calldata,
            &code_overrides,
        )?;
        let amount_out = if slipstream {
            ISlipstreamQuoter::quoteExactInputSingleCall::abi_decode_returns_validate(&output)
                .map_err(|_| SimError::MalformedOutput("quoteExactInputSingle return"))?
                .amountOut
        } else {
            quoteExactInputSingleCall::abi_decode_returns_validate(&output)
                .map_err(|_| SimError::MalformedOutput("quoteExactInputSingle return"))?
                .amountOut
        };

        Ok(SwapQuote::new(amount_out))
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

/// Every standard V3 pool event which can mutate pool storage.
///
/// Swap has a family-specific topic on Pancake V3. The remaining canonical
/// topics are routed for every V3-family registration so a matching mutation
/// can never silently bypass invalidation merely because exact family parity is
/// unavailable.
fn v3_mutating_event_topics(protocol: ProtocolId) -> Vec<B256> {
    let mut topics = Vec::with_capacity(9);
    topics.push(swap_topic_for(protocol));
    topics.extend(canonical_v3_non_swap_mutation_topics());
    topics
}

fn canonical_v3_non_swap_mutation_topics() -> [B256; 8] {
    [
        Initialize::SIGNATURE_HASH,
        Mint::SIGNATURE_HASH,
        Collect::SIGNATURE_HASH,
        Burn::SIGNATURE_HASH,
        Flash::SIGNATURE_HASH,
        IncreaseObservationCardinalityNext::SIGNATURE_HASH,
        SetFeeProtocol::SIGNATURE_HASH,
        CollectProtocol::SIGNATURE_HASH,
    ]
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

fn swap_invalidation(
    pool: &PoolRegistration,
    emitter: Address,
    topic0: B256,
    address: Address,
) -> AdapterEvent {
    AdapterEvent {
        pool: pool.key.clone(),
        emitter,
        topic0,
        kind: AdapterEventKind::Swap,
        updates: vec![StateUpdate::purge(address, PurgeScope::AllStorage)],
        quality: UpdateQuality::RequiresRepair,
        repair: RepairAction::PurgeStorage(address),
    }
}

fn non_swap_mutation_invalidation(
    pool: &PoolRegistration,
    emitter: Address,
    topic0: B256,
    address: Address,
) -> AdapterEvent {
    let kind = if topic0 == Mint::SIGNATURE_HASH {
        AdapterEventKind::LiquidityAdded
    } else if topic0 == Burn::SIGNATURE_HASH {
        AdapterEventKind::LiquidityRemoved
    } else {
        AdapterEventKind::Unknown
    };
    AdapterEvent {
        pool: pool.key.clone(),
        emitter,
        topic0,
        kind,
        updates: vec![StateUpdate::purge(address, PurgeScope::AllStorage)],
        quality: UpdateQuality::RequiresRepair,
        repair: RepairAction::PurgeStorage(address),
    }
}

fn offline_call_word(
    overlay: &mut EvmOverlay,
    from: Address,
    to: Address,
    signature: &'static str,
    argument: Option<Address>,
    observed_code_hashes: &mut BTreeSet<B256>,
) -> Result<U256, SlipstreamUnstakedFeeEvaluationError> {
    let mut calldata = Vec::with_capacity(if argument.is_some() { 36 } else { 4 });
    calldata.extend_from_slice(&keccak256(signature)[..4]);
    if let Some(argument) = argument {
        calldata.extend_from_slice(&[0_u8; 12]);
        calldata.extend_from_slice(argument.as_slice());
    }
    let result = overlay.call_raw_with_access_list(from, to, Bytes::from(calldata));
    if !overlay.missing_state().is_empty() {
        return Err(SlipstreamUnstakedFeeEvaluationError::MissingState);
    }
    let output = match result {
        Ok((ExecutionResult::Success { output, .. }, access)) => {
            observed_code_hashes.extend(access.code_hashes);
            output.into_data()
        }
        Ok((ExecutionResult::Revert { .. } | ExecutionResult::Halt { .. }, _)) | Err(_) => {
            return Err(SlipstreamUnstakedFeeEvaluationError::ExecutionFailed);
        }
    };
    if output.len() < 32 {
        return Err(SlipstreamUnstakedFeeEvaluationError::MalformedOutput);
    }
    Ok(U256::from_be_slice(&output[..32]))
}

#[derive(Clone, Copy)]
struct ReviewedSlipstreamFeeRuntime {
    chain_id: u64,
    pool: Address,
    factory: Address,
    proxy_runtime_code_hash: B256,
    implementation: Address,
    implementation_runtime_code_hash: B256,
    factory_runtime_code_hash: B256,
    voter: Address,
    voter_runtime_code_hash: B256,
    gauge: Address,
    module: Address,
    module_runtime_code_hash: B256,
}

const SLIPSTREAM_VOTER_GAUGES_SLOT: u64 = 8;
const SLIPSTREAM_VOTER_IS_ALIVE_SLOT: u64 = 20;
const SLIPSTREAM_FACTORY_UNSTAKED_FEE_MODULE_SLOT: u64 = 4;
const SLIPSTREAM_FEE_MODULE_OVERRIDES_SLOT: u64 = 1;

fn solidity_address_mapping_key(key: Address, mapping_slot: u64) -> U256 {
    let mut preimage = [0_u8; 64];
    preimage[12..32].copy_from_slice(key.as_slice());
    preimage[32..].copy_from_slice(&U256::from(mapping_slot).to_be_bytes::<32>());
    U256::from_be_slice(keccak256(preimage).as_slice())
}

fn reviewed_slipstream_fee_storage_targets(
    reviewed: ReviewedSlipstreamFeeRuntime,
) -> Vec<(Address, U256)> {
    vec![
        (
            reviewed.factory,
            U256::from(SLIPSTREAM_FACTORY_UNSTAKED_FEE_MODULE_SLOT),
        ),
        (
            reviewed.voter,
            solidity_address_mapping_key(reviewed.pool, SLIPSTREAM_VOTER_GAUGES_SLOT),
        ),
        (
            reviewed.voter,
            solidity_address_mapping_key(reviewed.gauge, SLIPSTREAM_VOTER_IS_ALIVE_SLOT),
        ),
        (
            reviewed.module,
            solidity_address_mapping_key(reviewed.pool, SLIPSTREAM_FEE_MODULE_OVERRIDES_SLOT),
        ),
    ]
}

fn reviewed_slipstream_fee_runtime(
    family: SlipstreamRuntimeFamily,
) -> ReviewedSlipstreamFeeRuntime {
    match family {
        SlipstreamRuntimeFamily::AerodromeBaseBifi => ReviewedSlipstreamFeeRuntime {
            chain_id: 8_453,
            pool: alloy_primitives::address!("b378137c90444bbcecd44a1f766851fbf53d2a9e"),
            factory: alloy_primitives::address!("5e7bb104d84c7cb9b682aac2f3d509f5f406809a"),
            proxy_runtime_code_hash: alloy_primitives::b256!(
                "acd6710f7037ad095b1e4d5f8ee5b2681069cb4dd316e77e4e0cb8f85716a2a1"
            ),
            implementation: alloy_primitives::address!("ec8e5342b19977b4ef8892e02d8daecfa1315831"),
            implementation_runtime_code_hash: alloy_primitives::b256!(
                "772fb5c610b40a122036f544e5b9b5bce6becb19db9524331289d1aaed2d5888"
            ),
            factory_runtime_code_hash: alloy_primitives::b256!(
                "7340cf80843bd721bcaefbfc050e38304cb4174c239e6e914e3056f27f39b11c"
            ),
            voter: alloy_primitives::address!("16613524e02ad97edfeF371bC883F2F5d6C480A5"),
            voter_runtime_code_hash: alloy_primitives::b256!(
                "465dc52dbb30fca5cca06c57fb266ec0e36c10530cdc6738dc4f035c81a0ae96"
            ),
            gauge: alloy_primitives::address!("6e415053aacdddc8b678a806a5279a8dcdd4f6f1"),
            module: alloy_primitives::address!("0ad08370c76ff426f534bb2affd9b5555338ee68"),
            module_runtime_code_hash: alloy_primitives::b256!(
                "ab88b0a965d9f221253c1affc473f1326156c89c15ebae6dc257a2654b063fdd"
            ),
        },
        SlipstreamRuntimeFamily::VelodromeOptimismBifi => ReviewedSlipstreamFeeRuntime {
            chain_id: 10,
            pool: alloy_primitives::address!("173cdc71e29d5cffa6d090ad99f555a24b8831f9"),
            factory: alloy_primitives::address!("cc0bddb707055e04e497ab22a59c2af4391cd12f"),
            proxy_runtime_code_hash: alloy_primitives::b256!(
                "063ca35333cb7f2463f087d40ff9485475550abf4858a2f63c387d4d102b0f4f"
            ),
            implementation: alloy_primitives::address!("c28ad28853a547556780bebf7847628501a3bcbb"),
            implementation_runtime_code_hash: alloy_primitives::b256!(
                "36c3da904ca0b58544254cd0d978fe4801c32dc1f9e3b3e644487ef541299794"
            ),
            factory_runtime_code_hash: alloy_primitives::b256!(
                "54e70bfdb89910349654db2f06c4a5cdbb9f4c74c781fe59276c1fa7b3f7f95e"
            ),
            voter: alloy_primitives::address!("41c914ee0c7e1a5edcd0295623e6dc557b5abf3c"),
            voter_runtime_code_hash: alloy_primitives::b256!(
                "6b54418007a7361638cff3c94032cb17f2728a6cda864d4ac65753c5445f1062"
            ),
            gauge: alloy_primitives::address!("41160e66fcaa10cbb148ace60bc2a22d609ec519"),
            module: alloy_primitives::address!("c565f7ba9c56b157da983c4db30e13f5f06c59d9"),
            module_runtime_code_hash: alloy_primitives::b256!(
                "23425577e3d433f535309680b44b53e5cbd1ad581b1d2248e015030fd9816e37"
            ),
        },
    }
}

fn reviewed_slipstream_fee_runtime_for_pool(pool: Address) -> Option<ReviewedSlipstreamFeeRuntime> {
    [
        SlipstreamRuntimeFamily::AerodromeBaseBifi,
        SlipstreamRuntimeFamily::VelodromeOptimismBifi,
    ]
    .into_iter()
    .map(reviewed_slipstream_fee_runtime)
    .find(|reviewed| reviewed.pool == pool)
}

fn validate_snapshot_identity(
    snapshot: &EvmSnapshot,
    reviewed: ReviewedSlipstreamFeeRuntime,
    identity: SlipstreamSnapshotIdentity,
    context: &AdapterEventContext,
) -> Result<(), SlipstreamUnstakedFeeEvaluationError> {
    if !identity.matches_context(context)
        || snapshot.chain_id() != reviewed.chain_id
        || identity.chain_id() != reviewed.chain_id
        || snapshot.block_number() != Some(identity.block_number())
        || snapshot.block_context_hash() != Some(identity.block_hash())
        || snapshot.timestamp() != Some(identity.block_timestamp())
        || identity
            .block_number()
            .checked_sub(1)
            .and_then(|number| snapshot.block_hash(number))
            != Some(identity.parent_hash())
    {
        return Err(SlipstreamUnstakedFeeEvaluationError::SnapshotIdentity);
    }
    Ok(())
}

fn validate_reviewed_runtime_addresses(
    snapshot: &EvmSnapshot,
    reviewed: ReviewedSlipstreamFeeRuntime,
    include_fee_path: bool,
) -> Result<(), SlipstreamUnstakedFeeEvaluationError> {
    let pool_runtimes = [
        (reviewed.pool, reviewed.proxy_runtime_code_hash),
        (
            reviewed.implementation,
            reviewed.implementation_runtime_code_hash,
        ),
    ];
    let fee_runtimes = [
        (reviewed.factory, reviewed.factory_runtime_code_hash),
        (reviewed.voter, reviewed.voter_runtime_code_hash),
        (reviewed.module, reviewed.module_runtime_code_hash),
    ];
    for (address, expected) in pool_runtimes.into_iter().chain(
        include_fee_path
            .then_some(fee_runtimes)
            .into_iter()
            .flatten(),
    ) {
        if snapshot.account_code_hash(address) != Some(expected) {
            return Err(SlipstreamUnstakedFeeEvaluationError::RuntimeCodeIdentity {
                missing: expected,
            });
        }
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
struct ReviewedUnstakedFeeOutcome {
    effective_fee: u32,
    gauge_alive: bool,
    observed_code_hashes: BTreeSet<B256>,
}

fn evaluate_reviewed_unstaked_fee_path(
    snapshot: Arc<EvmSnapshot>,
    reviewed: ReviewedSlipstreamFeeRuntime,
    caller: Address,
) -> Result<ReviewedUnstakedFeeOutcome, SlipstreamUnstakedFeeEvaluationError> {
    let mut observed_code_hashes = BTreeSet::new();
    let mut overlay = EvmOverlay::new(snapshot, None);
    let actual_pool_factory = Address::from_slice(
        &offline_call_word(
            &mut overlay,
            caller,
            reviewed.pool,
            "factory()",
            None,
            &mut observed_code_hashes,
        )?
        .to_be_bytes::<32>()[12..],
    );
    let _ = offline_call_word(
        &mut overlay,
        caller,
        reviewed.implementation,
        "factory()",
        None,
        &mut observed_code_hashes,
    )?;
    let actual_voter = Address::from_slice(
        &offline_call_word(
            &mut overlay,
            caller,
            reviewed.factory,
            "voter()",
            None,
            &mut observed_code_hashes,
        )?
        .to_be_bytes::<32>()[12..],
    );
    let actual_module = Address::from_slice(
        &offline_call_word(
            &mut overlay,
            caller,
            reviewed.factory,
            "unstakedFeeModule()",
            None,
            &mut observed_code_hashes,
        )?
        .to_be_bytes::<32>()[12..],
    );
    let actual_gauge = Address::from_slice(
        &offline_call_word(
            &mut overlay,
            caller,
            reviewed.voter,
            "gauges(address)",
            Some(reviewed.pool),
            &mut observed_code_hashes,
        )?
        .to_be_bytes::<32>()[12..],
    );
    if actual_pool_factory != reviewed.factory
        || actual_voter != reviewed.voter
        || actual_module != reviewed.module
        || actual_gauge != reviewed.gauge
    {
        return Err(SlipstreamUnstakedFeeEvaluationError::RuntimeAddressIdentity);
    }
    let alive_word = offline_call_word(
        &mut overlay,
        caller,
        reviewed.voter,
        "isAlive(address)",
        Some(reviewed.gauge),
        &mut observed_code_hashes,
    )?;
    if alive_word > U256::from(1) {
        return Err(SlipstreamUnstakedFeeEvaluationError::MalformedOutput);
    }
    let gauge_alive = !alive_word.is_zero();
    let effective_word = offline_call_word(
        &mut overlay,
        caller,
        reviewed.factory,
        "getUnstakedFee(address)",
        Some(reviewed.pool),
        &mut observed_code_hashes,
    )?;
    if effective_word > U256::from(1_000_000) {
        return Err(SlipstreamUnstakedFeeEvaluationError::FeeRange);
    }
    let effective_fee = effective_word.to::<u32>();
    let module_fee = offline_call_word(
        &mut overlay,
        caller,
        reviewed.module,
        "getFee(address)",
        Some(reviewed.pool),
        &mut observed_code_hashes,
    )?;
    if (gauge_alive && module_fee != effective_word) || (!gauge_alive && effective_fee != 0) {
        return Err(SlipstreamUnstakedFeeEvaluationError::FeePathMismatch);
    }
    let expected_code_hashes = [
        reviewed.proxy_runtime_code_hash,
        reviewed.implementation_runtime_code_hash,
        reviewed.factory_runtime_code_hash,
        reviewed.voter_runtime_code_hash,
        reviewed.module_runtime_code_hash,
    ];
    if let Some(missing) = expected_code_hashes
        .iter()
        .find(|hash| !observed_code_hashes.contains(*hash))
    {
        return Err(SlipstreamUnstakedFeeEvaluationError::RuntimeCodeIdentity {
            missing: *missing,
        });
    }
    Ok(ReviewedUnstakedFeeOutcome {
        effective_fee,
        gauge_alive,
        observed_code_hashes,
    })
}

impl ConcentratedLiquidityAdapter {
    /// Return the independently proven event-only transition capability for a
    /// concrete V3-family registration.
    pub fn swap_transition_capability(pool: &PoolRegistration) -> V3SwapTransitionCapability {
        let Some(layout) = layout_for(pool) else {
            return V3SwapTransitionCapability::Unsupported;
        };
        if pool.protocol() == ProtocolId::UniswapV3
            && layout == V3StorageLayout::uniswap(layout.tick_spacing)
            && layout.tick_spacing > 0
            && v3_fee(pool).is_some_and(|fee| fee < 1_000_000)
        {
            V3SwapTransitionCapability::Exact
        } else {
            V3SwapTransitionCapability::Unsupported
        }
    }

    /// Return the event-scoped exact capability.
    ///
    /// Canonical Uniswap V3 inherits its registration-scoped capability.
    /// Reviewed Slipstream deployments can reproduce the quote/search surface
    /// from the event and exact parent alone. Optional runtime-bound fee
    /// evidence additionally enables byte-exact fee/reward accounting writes;
    /// invalid supplied evidence always fails closed.
    pub fn swap_transition_capability_with_context(
        pool: &PoolRegistration,
        context: &AdapterEventContext,
    ) -> V3SwapTransitionCapability {
        let registration_capability = Self::swap_transition_capability(pool);
        if registration_capability == V3SwapTransitionCapability::Exact {
            return registration_capability;
        }
        let Some(layout) = layout_for(pool) else {
            return V3SwapTransitionCapability::Unsupported;
        };
        let Some(address) = pool.key.address() else {
            return V3SwapTransitionCapability::Unsupported;
        };
        if pool.protocol() != ProtocolId::Slipstream
            || validate_reviewed_slipstream_event(address, layout, context).is_err()
        {
            return V3SwapTransitionCapability::Unsupported;
        }
        if let Some(evidence) = context.slipstream_fee_evidence
            && (evidence.validate().is_err()
                || evidence.pool != address
                || context.chain_id != Some(evidence.chain_id)
                || context.block_number != Some(evidence.block_number)
                || context.block_hash != Some(evidence.block_hash)
                || context.parent_hash != Some(evidence.parent_hash)
                || context.block_timestamp != Some(evidence.block_timestamp)
                || context.transaction_hash != Some(evidence.transaction_hash)
                || context.transaction_index != Some(evidence.transaction_index)
                || context.log_index != Some(evidence.log_index))
        {
            return V3SwapTransitionCapability::Unsupported;
        }
        V3SwapTransitionCapability::Exact
    }

    /// Return the independently proven event-only `Mint`/`Burn` capability for a
    /// concrete V3-family registration.
    ///
    /// Unlike [`swap_transition_capability`](Self::swap_transition_capability)
    /// this does not require a fee: a liquidity change replays
    /// `Tick.update`/`Tick.clear`, the bitmap, the oracle, and active
    /// liquidity, none of which consult the pool fee. It does require a tick
    /// spacing the canonical `maxLiquidityPerTick` immutable is defined for,
    /// because `Tick.update` bounds `liquidityGross` against it.
    pub fn liquidity_transition_capability(
        pool: &PoolRegistration,
    ) -> V3LiquidityTransitionCapability {
        let Some(layout) = layout_for(pool) else {
            return V3LiquidityTransitionCapability::Unsupported;
        };
        if pool.protocol() == ProtocolId::UniswapV3
            && layout == V3StorageLayout::uniswap(layout.tick_spacing)
            && layout.tick_spacing > 0
            && uniswap_v3_max_liquidity_per_tick(layout.tick_spacing).is_some()
        {
            V3LiquidityTransitionCapability::Exact
        } else {
            V3LiquidityTransitionCapability::Unsupported
        }
    }

    /// Return the event-scoped exact `Mint`/`Burn` capability.
    ///
    /// Canonical Uniswap V3 inherits its registration-scoped capability.
    /// Reviewed Slipstream deployments additionally qualify once the event
    /// context pins them to a proven chain and pool identity.
    pub fn liquidity_transition_capability_with_context(
        pool: &PoolRegistration,
        context: &AdapterEventContext,
    ) -> V3LiquidityTransitionCapability {
        let registration_capability = Self::liquidity_transition_capability(pool);
        if registration_capability == V3LiquidityTransitionCapability::Exact {
            return registration_capability;
        }
        let Some(layout) = layout_for(pool) else {
            return V3LiquidityTransitionCapability::Unsupported;
        };
        let Some(address) = pool.key.address() else {
            return V3LiquidityTransitionCapability::Unsupported;
        };
        if pool.protocol() != ProtocolId::Slipstream
            || validate_reviewed_slipstream_event(address, layout, context).is_err()
        {
            return V3LiquidityTransitionCapability::Unsupported;
        }
        V3LiquidityTransitionCapability::Exact
    }

    /// Evaluate the reviewed Slipstream unstaked-liquidity fee against an
    /// immutable, provider-free transaction-parent snapshot.
    ///
    /// This executes the factory, voter, liveness, and custom-module calls used
    /// by the deployed pool. Every runtime must already be resident in the
    /// snapshot and every storage read must resolve locally; otherwise the
    /// evaluation fails closed. The snapshot block number, timestamp, and chain
    /// must match `context`. The reviewed `factory -> voter/module` path is
    /// caller-independent for these exact deployed runtime hashes; calls use a
    /// fixed zero caller so no transaction-origin feed is needed on the event
    /// hot path.
    pub fn evaluate_slipstream_unstaked_fee(
        family: SlipstreamRuntimeFamily,
        snapshot: Arc<EvmSnapshot>,
        identity: SlipstreamSnapshotIdentity,
        context: &AdapterEventContext,
    ) -> Result<SlipstreamUnstakedFeeEvaluation, SlipstreamUnstakedFeeEvaluationError> {
        let reviewed = reviewed_slipstream_fee_runtime(family);
        validate_snapshot_identity(&snapshot, reviewed, identity, context)?;
        validate_reviewed_runtime_addresses(&snapshot, reviewed, true)?;
        let outcome =
            evaluate_reviewed_unstaked_fee_path(Arc::clone(&snapshot), reviewed, Address::ZERO)?;
        // The reviewed factory/voter/custom-module source reaches only mapping
        // getters on this path. Execute the exact deployed runtimes again with
        // a distinct caller in every build so caller independence remains a
        // release-enforced property of the opaque proof.
        let alternate = evaluate_reviewed_unstaked_fee_path(snapshot, reviewed, reviewed.pool)?;
        if alternate != outcome {
            return Err(SlipstreamUnstakedFeeEvaluationError::FeePathMismatch);
        }
        Ok(SlipstreamUnstakedFeeEvaluation::new(
            outcome.effective_fee,
            SlipstreamUnstakedFeeProof::reviewed_runtime_evaluation(
                family,
                outcome.gauge_alive,
                outcome.effective_fee,
                identity,
            ),
        ))
    }

    /// Produce an all-liquidity-staked research candidate from an exact,
    /// provider-free snapshot of a reviewed pool runtime.
    ///
    /// This attests the address-bound pool proxy and implementation hashes and
    /// exact block/event lineage. Candidate replay must still prove every step
    /// has `stakedLiquidity == liquidity`. Alpha.5 does not grant public
    /// Slipstream Exact capability from this result.
    pub fn evaluate_slipstream_all_staked_candidate(
        family: SlipstreamRuntimeFamily,
        snapshot: Arc<EvmSnapshot>,
        identity: SlipstreamSnapshotIdentity,
        context: &AdapterEventContext,
    ) -> Result<SlipstreamUnstakedFeeEvaluation, SlipstreamUnstakedFeeEvaluationError> {
        let reviewed = reviewed_slipstream_fee_runtime(family);
        validate_snapshot_identity(&snapshot, reviewed, identity, context)?;
        validate_reviewed_runtime_addresses(&snapshot, reviewed, false)?;
        Ok(SlipstreamUnstakedFeeEvaluation::new(
            0,
            SlipstreamUnstakedFeeProof::unused_all_liquidity_staked(family, identity),
        ))
    }

    /// Infer the unique effective swap fee from a reviewed Slipstream event and
    /// exact parent state without a provider read.
    ///
    /// The supplied context must carry reviewed runtime/lineage evidence and
    /// the effective unstaked fee. Its provisional `effective_swap_fee` is not
    /// trusted by this method; callers replace it with the returned value via
    /// [`super::SlipstreamSwapFeeEvidence::with_effective_swap_fee`] before
    /// injecting the final evidence into [`super::AmmSyncEngine`]. Ambiguous
    /// tiny/rounded events fail closed.
    pub fn infer_slipstream_swap_fee(
        pool: &PoolRegistration,
        log: &Log,
        view: &dyn StateView,
        context: &AdapterEventContext,
    ) -> Result<u32, AdapterEventError> {
        if pool.protocol() != ProtocolId::Slipstream {
            return Err(AdapterEventError::Unsupported(UnsupportedReason::Protocol(
                pool.protocol(),
            )));
        }
        let address = pool.key.address().ok_or(AdapterEventError::MalformedLog(
            "Slipstream pool key is not address-keyed",
        ))?;
        let layout = layout_for(pool).ok_or(AdapterEventError::Unsupported(
            UnsupportedReason::MissingMetadata("Slipstream storage layout"),
        ))?;
        if log.topics().first().copied() != Some(Swap::SIGNATURE_HASH)
            || Swap::decode_log_data_validate(&log.data).is_err()
        {
            return Err(AdapterEventError::MalformedLog(
                "malformed Slipstream Swap log",
            ));
        }
        let swap = decode_swap_body(log)?;
        infer_slipstream_swap_fee(address, layout, swap, view, context)
    }

    fn decode_swap_without_context(
        &self,
        pool: &PoolRegistration,
        log: &Log,
        topic0: B256,
    ) -> AdapterEventResult {
        let Some(address) = pool.key.address() else {
            return AdapterEventResult::error(AdapterEventError::MalformedLog(
                "V3 pool key is not address-keyed",
            ));
        };
        if layout_for(pool).is_none() {
            return AdapterEventResult::event_with_error(
                swap_invalidation(pool, log.address, topic0, address),
                AdapterEventError::Unsupported(UnsupportedReason::MissingMetadata(
                    "V3 storage layout",
                )),
            );
        }
        let valid = if topic0 == PancakeV3Swap::SIGNATURE_HASH {
            PancakeV3Swap::decode_log_data_validate(&log.data).is_ok()
        } else {
            Swap::decode_log_data_validate(&log.data).is_ok()
        };
        if !valid {
            return AdapterEventResult::event_with_error(
                swap_invalidation(pool, log.address, topic0, address),
                AdapterEventError::MalformedLog("malformed V3 Swap log"),
            );
        }

        let error = if Self::swap_transition_capability(pool) == V3SwapTransitionCapability::Exact {
            AdapterEventError::V3Transition(super::V3TransitionError::MissingContext(
                "complete event context",
            ))
        } else {
            AdapterEventError::Unsupported(UnsupportedReason::Protocol(pool.protocol()))
        };
        AdapterEventResult::event_with_error(
            swap_invalidation(pool, log.address, topic0, address),
            error,
        )
    }

    fn decode_swap_exact(
        &self,
        pool: &PoolRegistration,
        log: &Log,
        topic0: B256,
        view: &dyn StateView,
        context: &AdapterEventContext,
    ) -> AdapterEventResult {
        // Canonical Uniswap and reviewed Slipstream runtimes use independent
        // transition implementations. Fork-family events never enter replay
        // merely because their ABI or slot offsets look similar.
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
        let valid = Swap::decode_log_data_validate(&log.data).is_ok();
        if !valid {
            return AdapterEventResult::event_with_error(
                swap_invalidation(pool, log.address, topic0, address),
                AdapterEventError::MalformedLog("malformed V3 Swap log"),
            );
        }

        let swap = match decode_swap_body(log) {
            Ok(swap) => swap,
            Err(error) => {
                return AdapterEventResult::event_with_error(
                    swap_invalidation(pool, log.address, topic0, address),
                    error,
                );
            }
        };
        let updates = match pool.protocol() {
            ProtocolId::UniswapV3 => {
                let Some(fee) = v3_fee(pool) else {
                    return AdapterEventResult::error(AdapterEventError::Unsupported(
                        UnsupportedReason::MissingMetadata("V3 fee"),
                    ));
                };
                derive_uniswap_v3_swap(address, layout, fee, swap, view, context)
            }
            ProtocolId::Slipstream => derive_slipstream_swap(address, layout, swap, view, context),
            _ => Err(AdapterEventError::Unsupported(UnsupportedReason::Protocol(
                pool.protocol(),
            ))),
        };
        let updates = match updates {
            Ok(updates) => updates,
            Err(error) => {
                return AdapterEventResult::event_with_error(
                    AdapterEvent {
                        pool: pool.key.clone(),
                        emitter: log.address,
                        topic0,
                        kind: AdapterEventKind::Swap,
                        updates: vec![StateUpdate::purge(address, PurgeScope::AllStorage)],
                        quality: UpdateQuality::ConservativeInvalidation,
                        repair: RepairAction::PurgeStorage(address),
                    },
                    error,
                );
            }
        };

        AdapterEventResult::event(AdapterEvent {
            pool: pool.key.clone(),
            emitter: log.address,
            topic0,
            kind: AdapterEventKind::Swap,
            updates,
            quality: UpdateQuality::Exact,
            repair: RepairAction::None,
        })
    }

    fn decode_non_swap_mutation(
        &self,
        pool: &PoolRegistration,
        log: &Log,
        topic0: B256,
    ) -> AdapterEventResult {
        let Some(address) = pool.key.address() else {
            return AdapterEventResult::error(AdapterEventError::MalformedLog(
                "V3 pool key is not address-keyed",
            ));
        };
        AdapterEventResult::event_with_error(
            non_swap_mutation_invalidation(pool, log.address, topic0, address),
            AdapterEventError::Unsupported(UnsupportedReason::Custom(
                "exact event-only canonical V3 non-Swap transition is unsupported".to_owned(),
            )),
        )
    }

    /// Decode a Uniswap V3 `Mint`/`Burn` conservatively, for callers with no
    /// event context.
    ///
    /// `_modifyPosition` reads the oracle for any nonzero liquidity delta, and a
    /// newly initialized tick at or below the current one seeds its outside
    /// accumulators from that reading. Without the block timestamp an
    /// [`AdapterEventContext`] carries, that reading cannot be reproduced, so no
    /// exact transition exists — and a partial one would leave the pool
    /// quote-ready on state a later swap would diverge from. Even a fully warm
    /// cache is therefore purged after the log is validated; no partial state is
    /// computed or briefly applied.
    ///
    /// Callers on the context-aware path get the exact transition instead; see
    /// [`decode_canonical_liquidity_exact`](Self::decode_canonical_liquidity_exact).
    fn decode_liquidity_event(
        &self,
        pool: &PoolRegistration,
        log: &Log,
        _view: &dyn StateView,
        is_mint: bool,
    ) -> AdapterEventResult {
        let topic0 = if is_mint {
            Mint::SIGNATURE_HASH
        } else {
            Burn::SIGNATURE_HASH
        };
        let Some(address) = pool.key.address() else {
            return AdapterEventResult::error(AdapterEventError::MalformedLog(
                "V3 pool key is not address-keyed",
            ));
        };
        let decode_ok = if is_mint {
            Mint::decode_log_data_validate(&log.data).is_ok()
        } else {
            Burn::decode_log_data_validate(&log.data).is_ok()
        };
        if !decode_ok {
            return AdapterEventResult::event_with_error(
                non_swap_mutation_invalidation(pool, log.address, topic0, address),
                AdapterEventError::MalformedLog("malformed V3 liquidity log"),
            );
        }

        self.decode_non_swap_mutation(pool, log, topic0)
    }

    /// Replay one of the canonical Uniswap V3 accounting events — `Flash`,
    /// `SetFeeProtocol`, `CollectProtocol`, or
    /// `IncreaseObservationCardinalityNext` — against an exact parent.
    ///
    /// None of these move price, liquidity, or a tick, and every one of them is
    /// fully determined by the event plus warm parent cells. Treating them as
    /// unknown mutations meant a governance call or a single flash loan discarded
    /// a pool's entire storage.
    fn decode_canonical_accounting_event(
        &self,
        pool: &PoolRegistration,
        log: &Log,
        view: &dyn StateView,
        topic0: B256,
        context: &AdapterEventContext,
    ) -> AdapterEventResult {
        let Some((address, layout)) = canonical_accounting_layout(pool) else {
            return self.decode_non_swap_mutation(pool, log, topic0);
        };

        let derived = if topic0 == Flash::SIGNATURE_HASH {
            match v3_fee(pool) {
                Some(fee) => decode_flash_body(log).and_then(|flash| {
                    derive_uniswap_v3_flash(address, layout, fee, flash, view, context)
                }),
                None => Err(AdapterEventError::Unsupported(
                    UnsupportedReason::MissingMetadata("V3 fee"),
                )),
            }
        } else if topic0 == SetFeeProtocol::SIGNATURE_HASH {
            decode_fee_protocol_body(log).and_then(|event| {
                derive_uniswap_v3_set_fee_protocol(address, layout, event, view, context)
            })
        } else if topic0 == CollectProtocol::SIGNATURE_HASH {
            decode_collect_protocol_body(log).and_then(|event| {
                derive_uniswap_v3_collect_protocol(address, layout, event, view, context)
            })
        } else if topic0 == IncreaseObservationCardinalityNext::SIGNATURE_HASH {
            decode_observation_growth_body(log).and_then(|event| {
                derive_uniswap_v3_observation_growth(address, layout, event, view, context)
            })
        } else {
            return self.decode_non_swap_mutation(pool, log, topic0);
        };

        match derived {
            Ok(updates) => AdapterEventResult::event(AdapterEvent {
                pool: pool.key.clone(),
                emitter: log.address,
                topic0,
                kind: AdapterEventKind::Unknown,
                updates,
                quality: UpdateQuality::Exact,
                repair: RepairAction::None,
            }),
            Err(error) => AdapterEventResult::event_with_error(
                non_swap_mutation_invalidation(pool, log.address, topic0, address),
                error,
            ),
        }
    }

    /// A canonical `Collect` moves position `tokensOwed` and ERC-20 balances and
    /// nothing else: it never touches slot0, liquidity, a tick, the bitmap, or
    /// the oracle. It is therefore an exact transition over the pool's pricing
    /// surface with no writes at all — only the collecting position is dropped.
    ///
    /// This matters out of proportion to its simplicity. Nearly every `Burn`
    /// through the position manager is followed by a `Collect`, so treating it
    /// as an unknown mutation would re-purge the pool immediately after a
    /// `Burn` was replayed exactly.
    fn decode_canonical_collect(&self, pool: &PoolRegistration, log: &Log) -> AdapterEventResult {
        let Some(address) = pool.key.address() else {
            return AdapterEventResult::error(AdapterEventError::MalformedLog(
                "V3 pool key is not address-keyed",
            ));
        };
        if Collect::decode_log_data_validate(&log.data).is_err() {
            return AdapterEventResult::event_with_error(
                non_swap_mutation_invalidation(pool, log.address, Collect::SIGNATURE_HASH, address),
                AdapterEventError::MalformedLog("malformed V3 Collect log"),
            );
        }
        let (Some(owner), Some(tick_lower), Some(tick_upper)) = (
            topic_address(log, 1),
            log.topics()
                .get(2)
                .map(|topic| int24_from_word((*topic).into())),
            log.topics()
                .get(3)
                .map(|topic| int24_from_word((*topic).into())),
        ) else {
            return AdapterEventResult::event_with_error(
                non_swap_mutation_invalidation(pool, log.address, Collect::SIGNATURE_HASH, address),
                AdapterEventError::MalformedLog("V3 Collect is missing indexed position topics"),
            );
        };
        AdapterEventResult::event(AdapterEvent {
            pool: pool.key.clone(),
            emitter: log.address,
            topic0: Collect::SIGNATURE_HASH,
            kind: AdapterEventKind::Unknown,
            updates: vec![StateUpdate::purge(
                address,
                PurgeScope::Slots(
                    uniswap_v3_position_slots(owner, tick_lower, tick_upper).to_vec(),
                ),
            )],
            quality: UpdateQuality::Exact,
            repair: RepairAction::None,
        })
    }

    /// Replay a canonical Uniswap V3 `Mint`/`Burn` against an exact parent.
    ///
    /// Falls back to the conservative invalidation whenever the registration is
    /// not provably canonical, so a fork family can never reach replay merely
    /// because its event ABI matches.
    fn decode_canonical_liquidity_exact(
        &self,
        pool: &PoolRegistration,
        log: &Log,
        view: &dyn StateView,
        is_mint: bool,
        context: &AdapterEventContext,
    ) -> AdapterEventResult {
        if Self::liquidity_transition_capability(pool) != V3LiquidityTransitionCapability::Exact {
            return self.decode_liquidity_event(pool, log, view, is_mint);
        }
        let topic0 = if is_mint {
            Mint::SIGNATURE_HASH
        } else {
            Burn::SIGNATURE_HASH
        };
        let Some(address) = pool.key.address() else {
            return AdapterEventResult::error(AdapterEventError::MalformedLog(
                "V3 pool key is not address-keyed",
            ));
        };
        let Some(layout) = layout_for(pool) else {
            return AdapterEventResult::event_with_error(
                non_swap_mutation_invalidation(pool, log.address, topic0, address),
                AdapterEventError::Unsupported(UnsupportedReason::MissingMetadata(
                    "V3 storage layout",
                )),
            );
        };
        let decode_ok = if is_mint {
            Mint::decode_log_data_validate(&log.data).is_ok()
        } else {
            Burn::decode_log_data_validate(&log.data).is_ok()
        };
        if !decode_ok {
            return AdapterEventResult::event_with_error(
                non_swap_mutation_invalidation(pool, log.address, topic0, address),
                AdapterEventError::MalformedLog("malformed V3 liquidity log"),
            );
        }
        let decoded = match decode_liquidity_body(log, is_mint) {
            Ok(decoded) => decoded,
            Err(error) => {
                return AdapterEventResult::event_with_error(
                    non_swap_mutation_invalidation(pool, log.address, topic0, address),
                    error,
                );
            }
        };
        let transition = match derive_uniswap_v3_liquidity(address, layout, decoded, view, context)
        {
            Ok(transition) => transition,
            Err(error) => {
                return AdapterEventResult::event_with_error(
                    non_swap_mutation_invalidation(pool, log.address, topic0, address),
                    error,
                );
            }
        };
        let kind = if is_mint {
            AdapterEventKind::LiquidityAdded
        } else {
            AdapterEventKind::LiquidityRemoved
        };
        let mut updates = transition.updates;
        // The transition reconstructs the pricing surface, not the position the
        // event belongs to. Dropping that position's cells keeps a warm one from
        // going stale and costs nothing on a pool that never warmed them.
        if let Some(owner) = topic_address(log, 1) {
            updates.push(StateUpdate::purge(
                address,
                PurgeScope::Slots(
                    uniswap_v3_position_slots(owner, decoded.tick_lower, decoded.tick_upper)
                        .to_vec(),
                ),
            ));
        }
        if transition.cold_slots.is_empty() {
            return AdapterEventResult::event(AdapterEvent {
                pool: pool.key.clone(),
                emitter: log.address,
                topic0,
                kind,
                updates,
                quality: UpdateQuality::Exact,
                repair: RepairAction::None,
            });
        }
        // A boundary tick outside the warmed bitmap radius leaves only that
        // boundary unknown. Drop exactly those cells and re-read them, rather
        // than discarding a pool whose price, liquidity, and oracle this
        // transition just established exactly.
        updates.push(StateUpdate::purge(
            address,
            PurgeScope::Slots(transition.cold_slots.clone()),
        ));
        AdapterEventResult::event(AdapterEvent {
            pool: pool.key.clone(),
            emitter: log.address,
            topic0,
            kind,
            updates,
            quality: UpdateQuality::RequiresRepair,
            repair: RepairAction::VerifySlots(
                transition
                    .cold_slots
                    .into_iter()
                    .map(|slot| (address, slot))
                    .collect(),
            ),
        })
    }

    fn decode_liquidity_event_with_context(
        &self,
        pool: &PoolRegistration,
        log: &Log,
        view: &dyn StateView,
        is_mint: bool,
        context: &AdapterEventContext,
    ) -> AdapterEventResult {
        match pool.protocol() {
            ProtocolId::UniswapV3 => {
                return self.decode_canonical_liquidity_exact(pool, log, view, is_mint, context);
            }
            ProtocolId::Slipstream => {}
            _ => return self.decode_liquidity_event(pool, log, view, is_mint),
        }
        let topic0 = if is_mint {
            Mint::SIGNATURE_HASH
        } else {
            Burn::SIGNATURE_HASH
        };
        let Some(address) = pool.key.address() else {
            return AdapterEventResult::error(AdapterEventError::MalformedLog(
                "Slipstream pool key is not address-keyed",
            ));
        };
        let Some(layout) = layout_for(pool) else {
            return AdapterEventResult::event_with_error(
                non_swap_mutation_invalidation(pool, log.address, topic0, address),
                AdapterEventError::Unsupported(UnsupportedReason::MissingMetadata(
                    "Slipstream storage layout",
                )),
            );
        };
        let decode_ok = if is_mint {
            Mint::decode_log_data_validate(&log.data).is_ok()
        } else {
            Burn::decode_log_data_validate(&log.data).is_ok()
        };
        if !decode_ok {
            return AdapterEventResult::event_with_error(
                non_swap_mutation_invalidation(pool, log.address, topic0, address),
                AdapterEventError::MalformedLog("malformed Slipstream liquidity log"),
            );
        }
        let decoded = match decode_liquidity_body(log, is_mint) {
            Ok(decoded) => decoded,
            Err(error) => {
                return AdapterEventResult::event_with_error(
                    non_swap_mutation_invalidation(pool, log.address, topic0, address),
                    error,
                );
            }
        };
        let updates = match derive_slipstream_liquidity(address, layout, decoded, view, context) {
            Ok(updates) => updates,
            Err(error) => {
                return AdapterEventResult::event_with_error(
                    non_swap_mutation_invalidation(pool, log.address, topic0, address),
                    error,
                );
            }
        };
        AdapterEventResult::event(AdapterEvent {
            pool: pool.key.clone(),
            emitter: log.address,
            topic0,
            kind: if is_mint {
                AdapterEventKind::LiquidityAdded
            } else {
                AdapterEventKind::LiquidityRemoved
            },
            updates,
            quality: UpdateQuality::Exact,
            repair: RepairAction::None,
        })
    }

    fn decode_collect_with_context(
        &self,
        pool: &PoolRegistration,
        log: &Log,
        context: &AdapterEventContext,
    ) -> AdapterEventResult {
        if pool.protocol() == ProtocolId::UniswapV3
            && Self::liquidity_transition_capability(pool) == V3LiquidityTransitionCapability::Exact
        {
            return self.decode_canonical_collect(pool, log);
        }
        if pool.protocol() != ProtocolId::Slipstream {
            return self.decode_non_swap_mutation(pool, log, Collect::SIGNATURE_HASH);
        }
        let Some(address) = pool.key.address() else {
            return AdapterEventResult::error(AdapterEventError::MalformedLog(
                "Slipstream pool key is not address-keyed",
            ));
        };
        if Collect::decode_log_data_validate(&log.data).is_err() {
            return AdapterEventResult::event_with_error(
                non_swap_mutation_invalidation(pool, log.address, Collect::SIGNATURE_HASH, address),
                AdapterEventError::MalformedLog("malformed Slipstream Collect log"),
            );
        }
        let Some(layout) = layout_for(pool) else {
            return AdapterEventResult::event_with_error(
                non_swap_mutation_invalidation(pool, log.address, Collect::SIGNATURE_HASH, address),
                AdapterEventError::Unsupported(UnsupportedReason::MissingMetadata(
                    "Slipstream storage layout",
                )),
            );
        };
        if let Err(error) = validate_reviewed_slipstream_event(address, layout, context) {
            return AdapterEventResult::event_with_error(
                non_swap_mutation_invalidation(pool, log.address, Collect::SIGNATURE_HASH, address),
                error,
            );
        }
        // Collect mutates the caller's Position.Info and ERC-20 balances only.
        // Neither is part of the pool quote/search surface, so the exact
        // adapter-owned transition is empty and must not trigger reconstruction.
        AdapterEventResult::event(AdapterEvent {
            pool: pool.key.clone(),
            emitter: log.address,
            topic0: Collect::SIGNATURE_HASH,
            kind: AdapterEventKind::Unknown,
            updates: Vec::new(),
            quality: UpdateQuality::Exact,
            repair: RepairAction::None,
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
/// - Round 3 (`Strict`/`Eager` only) verifies every required `Tick.Info` word of
///   each initialized tick across the whole window in one round: four for
///   canonical Uniswap, six for reviewed Slipstream.
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
    /// Independently proven state surface that must be complete before the
    /// planner can leave a pool quote-ready for event-only swaps.
    exact_surface: V3ColdStartExactSurface,
    /// Address-bound external cells required by the reviewed quote runtime.
    external_exact_slots: Vec<(Address, U256)>,
    phase: V3Phase,
    /// The cold-start window: each `(word, bitmap_key)` pair in
    /// `[W0 - R, W0 + R]` clamped to the valid V3 word range, resolved from the
    /// warmed slot0. Empty until round 1 decodes the current tick.
    window: Vec<(i16, U256)>,
    verified_slots: Vec<(Address, U256)>,
    changed_slots: Vec<SlotChange>,
    /// Exact canonical cells fetched as genuine zero during this run. They are
    /// materialized explicitly so `StateView::storage(None)` remains unknown,
    /// never an implicit zero assumption.
    proven_zero_slots: Vec<(Address, U256)>,
    /// Exact canonical cells that could not be fetched. A planner carrying any
    /// such cell cannot mark the pool ready for event-only transitions.
    failed_exact_slots: Vec<(Address, U256)>,
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum V3ColdStartExactSurface {
    None,
    CanonicalUniswap,
    Slipstream,
}

impl UniswapV3ColdStartPlanner {
    fn new(
        address: Address,
        layout: V3StorageLayout,
        policy: ColdStartPolicy,
        radius: i16,
        exact_surface: V3ColdStartExactSurface,
        external_exact_slots: Vec<(Address, U256)>,
    ) -> Self {
        Self {
            address,
            layout,
            policy,
            radius,
            exact_surface,
            external_exact_slots,
            phase: V3Phase::Slot0Liquidity,
            window: Vec::new(),
            verified_slots: Vec::new(),
            changed_slots: Vec::new(),
            proven_zero_slots: Vec::new(),
            failed_exact_slots: Vec::new(),
            deferred: Vec::new(),
            slot0_cold: false,
        }
    }

    fn is_exact_slot(&self, address: Address, slot: U256) -> bool {
        self.exact_surface != V3ColdStartExactSurface::None
            && (address == self.address || self.external_exact_slots.contains(&(address, slot)))
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
        let mut verify = vec![
            (self.address, self.layout.slot0_slot),
            (self.address, self.layout.liquidity_slot),
        ];
        match self.exact_surface {
            V3ColdStartExactSurface::CanonicalUniswap => verify.extend([
                (self.address, U256::from(1)),
                (self.address, U256::from(2)),
                (self.address, U256::from(3)),
            ]),
            V3ColdStartExactSurface::Slipstream => verify.extend([
                // Factory identity plus every non-mapping parent cell the
                // reviewed swap transition can read or mutate.
                (self.address, U256::ZERO),
                (self.address, U256::from(7)),
                (self.address, U256::from(8)),
                (self.address, U256::from(9)),
                (self.address, U256::from(10)),
                (self.address, U256::from(11)),
                (self.address, U256::from(12)),
                (self.address, U256::from(14)),
                (self.address, U256::from(15)),
            ]),
            V3ColdStartExactSurface::None => {}
        }
        verify.extend(self.external_exact_slots.iter().copied());
        self.verified_slots.extend_from_slice(&verify);
        ColdStartPlan {
            verify,
            ..Default::default()
        }
    }

    fn on_results(&mut self, results: &ColdStartResults, state: &dyn StateView) -> ColdStartStep {
        self.changed_slots.extend(results.verified.iter().cloned());
        if self.exact_surface != V3ColdStartExactSurface::None {
            let proven_zero_slots: Vec<_> = results
                .fetched
                .iter()
                .filter_map(|outcome| {
                    (self.is_exact_slot(outcome.address, outcome.slot)
                        && matches!(outcome.fetch, SlotFetch::Zero))
                    .then_some((outcome.address, outcome.slot))
                })
                .collect();
            self.proven_zero_slots.extend(proven_zero_slots);
            self.proven_zero_slots.sort_unstable();
            self.proven_zero_slots.dedup();
            let failed_exact_slots: Vec<_> = results
                .fetched
                .iter()
                .filter_map(|outcome| {
                    (self.is_exact_slot(outcome.address, outcome.slot)
                        && matches!(
                            outcome.fetch,
                            SlotFetch::FetchFailed { .. } | SlotFetch::NotAttempted
                        ))
                    .then_some((outcome.address, outcome.slot))
                })
                .collect();
            self.failed_exact_slots.extend(failed_exact_slots);
            self.failed_exact_slots.sort_unstable();
            self.failed_exact_slots.dedup();
        }

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
                if !self.failed_exact_slots.is_empty() {
                    return ColdStartStep::Done;
                }

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
                        let mut verify: Vec<(Address, U256)> = self
                            .window
                            .iter()
                            .map(|(_, key)| (self.address, *key))
                            .collect();
                        if self.exact_surface != V3ColdStartExactSurface::None {
                            let observation_index =
                                ((slot0 >> 184_usize) & U256::from(u16::MAX)).to::<u16>();
                            match self.exact_surface {
                                V3ColdStartExactSurface::CanonicalUniswap => verify.push((
                                    self.address,
                                    U256::from(8) + U256::from(observation_index),
                                )),
                                V3ColdStartExactSurface::Slipstream => {
                                    let observation_cardinality =
                                        ((slot0 >> 200_usize) & U256::from(u16::MAX)).to::<u16>();
                                    verify.extend((0..observation_cardinality).map(|index| {
                                        (self.address, U256::from(20) + U256::from(index))
                                    }));
                                }
                                V3ColdStartExactSurface::None => unreachable!(),
                            }
                        }
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
                if !self.failed_exact_slots.is_empty() {
                    return ColdStartStep::Done;
                }
                // Round 3: warm every `Tick.Info` word of each initialized tick
                // across the whole window. Canonical Uniswap reads four; the
                // reviewed Slipstream layout reads all six. This matches the
                // respective one-shot full-sync program. Each window word's bitmap is extracted
                // adapter-locally: bit `i` set => tick `(word * 256 + i) *
                // tick_spacing`, skipping any tick outside [MIN_TICK, MAX_TICK].
                let mut tick_slots: Vec<(Address, U256)> = Vec::new();
                for (word, bitmap_key) in &self.window {
                    let bitmap = results
                        .fetched
                        .iter()
                        .find(|outcome| {
                            outcome.address == self.address && outcome.slot == *bitmap_key
                        })
                        .and_then(|outcome| match &outcome.fetch {
                            SlotFetch::Value(value) => Some(*value),
                            SlotFetch::Zero => Some(U256::ZERO),
                            SlotFetch::FetchFailed { .. } | SlotFetch::NotAttempted => None,
                        })
                        .or_else(|| state.storage(self.address, *bitmap_key));
                    let Some(bitmap) = bitmap else {
                        self.failed_exact_slots.push((self.address, *bitmap_key));
                        continue;
                    };
                    for bit in 0..256u32 {
                        if (bitmap >> bit) & U256::from(1) == U256::from(1) {
                            // Compute the tick index in i32; word/bit/spacing are
                            // all bounded so this cannot overflow.
                            let tick_i =
                                (*word as i32 * 256 + bit as i32) * self.layout.tick_spacing;
                            if !(V3_MIN_TICK..=V3_MAX_TICK).contains(&tick_i) {
                                continue;
                            }
                            if self.exact_surface == V3ColdStartExactSurface::Slipstream {
                                let keys = slipstream_tick_info_storage_keys_with_base(
                                    tick_i,
                                    self.layout.ticks_base_slot,
                                );
                                tick_slots.extend(keys.iter().map(|key| (self.address, *key)));
                            } else {
                                let keys = v3_tick_info_storage_keys_with_base(
                                    tick_i,
                                    self.layout.ticks_base_slot,
                                );
                                tick_slots.extend(keys.iter().map(|key| (self.address, *key)));
                            }
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

    fn materialization_updates(&self) -> Vec<StateUpdate> {
        if self.slot0_cold {
            return Vec::new();
        }
        self.proven_zero_slots
            .iter()
            .map(|(address, slot)| StateUpdate::slot(*address, *slot, U256::ZERO))
            .collect()
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
            pool.status = PoolStatus::Degraded;
            return ColdStartOutcome::NeedsRepair(
                report,
                RepairAction::VerifySlots(vec![(self.address, self.layout.slot0_slot)]),
            );
        }

        if !self.failed_exact_slots.is_empty() {
            report.status = PoolStatus::Degraded;
            pool.status = PoolStatus::Degraded;
            let slots = self.failed_exact_slots.clone();
            return ColdStartOutcome::NeedsRepair(report, RepairAction::VerifySlots(slots));
        }

        // Preserve the config-supplied V3 metadata (token0/token1/fee/
        // tick_spacing/layout are not at predictable storage slots and are not
        // re-fetched here).
        record_v3_warmed_slots(pool, self.address, &self.verified_slots);
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

fn record_v3_warmed_slots(
    pool: &mut PoolRegistration,
    address: Address,
    verified_slots: &[(Address, U256)],
) {
    let mut slots = verified_slots
        .iter()
        .filter_map(|(slot_address, slot)| (*slot_address == address).then_some(*slot))
        .collect::<Vec<_>>();
    slots.sort_unstable();
    slots.dedup();
    match &mut pool.metadata {
        ProtocolMetadata::UniswapV3(metadata)
        | ProtocolMetadata::PancakeV3(metadata)
        | ProtocolMetadata::Slipstream(metadata) => {
            metadata.warmed_slots = slots;
        }
        _ => {}
    }
}

/// The shared gate for every canonical accounting transition: this pool must be
/// a provably canonical Uniswap V3 deployment, not merely a V3-shaped one.
fn canonical_accounting_layout(pool: &PoolRegistration) -> Option<(Address, V3StorageLayout)> {
    if pool.protocol() != ProtocolId::UniswapV3 {
        return None;
    }
    let layout = layout_for(pool)?;
    if layout != V3StorageLayout::uniswap(layout.tick_spacing) || layout.tick_spacing <= 0 {
        return None;
    }
    Some((pool.key.address()?, layout))
}

/// The canonical accounting events: pool mutations that move fee, protocol-fee,
/// or oracle-reservation state without touching price, liquidity, or any tick.
fn canonical_v3_accounting_topics() -> [B256; 4] {
    [
        Flash::SIGNATURE_HASH,
        SetFeeProtocol::SIGNATURE_HASH,
        CollectProtocol::SIGNATURE_HASH,
        IncreaseObservationCardinalityNext::SIGNATURE_HASH,
    ]
}

/// Read a non-indexed word that the ABI narrows to `u8`.
fn narrow_u8(log: &Log, index: usize, what: &'static str) -> Result<u8, AdapterEventError> {
    let word = data_word(log, index).ok_or(AdapterEventError::MalformedLog(what))?;
    if word > U256::from(u8::MAX) {
        return Err(AdapterEventError::MalformedLog(what));
    }
    Ok(word.to::<u8>())
}

/// Read a non-indexed word that the ABI narrows to `u16`.
fn narrow_u16(log: &Log, index: usize, what: &'static str) -> Result<u16, AdapterEventError> {
    let word = data_word(log, index).ok_or(AdapterEventError::MalformedLog(what))?;
    if word > U256::from(u16::MAX) {
        return Err(AdapterEventError::MalformedLog(what));
    }
    Ok(word.to::<u16>())
}

fn decode_flash_body(log: &Log) -> Result<DecodedFlash, AdapterEventError> {
    if Flash::decode_log_data_validate(&log.data).is_err() {
        return Err(AdapterEventError::MalformedLog("malformed V3 Flash log"));
    }
    const WHAT: &str = "missing V3 Flash data word";
    let (Some(amount0), Some(amount1), Some(paid0), Some(paid1)) = (
        data_word(log, 0),
        data_word(log, 1),
        data_word(log, 2),
        data_word(log, 3),
    ) else {
        return Err(AdapterEventError::MalformedLog(WHAT));
    };
    Ok(DecodedFlash {
        amount0,
        amount1,
        paid0,
        paid1,
    })
}

fn decode_fee_protocol_body(log: &Log) -> Result<DecodedFeeProtocol, AdapterEventError> {
    if SetFeeProtocol::decode_log_data_validate(&log.data).is_err() {
        return Err(AdapterEventError::MalformedLog(
            "malformed V3 SetFeeProtocol log",
        ));
    }
    const WHAT: &str = "missing V3 SetFeeProtocol data word";
    Ok(DecodedFeeProtocol {
        old0: narrow_u8(log, 0, WHAT)?,
        old1: narrow_u8(log, 1, WHAT)?,
        new0: narrow_u8(log, 2, WHAT)?,
        new1: narrow_u8(log, 3, WHAT)?,
    })
}

fn decode_collect_protocol_body(log: &Log) -> Result<DecodedCollectProtocol, AdapterEventError> {
    if CollectProtocol::decode_log_data_validate(&log.data).is_err() {
        return Err(AdapterEventError::MalformedLog(
            "malformed V3 CollectProtocol log",
        ));
    }
    const WHAT: &str = "missing V3 CollectProtocol data word";
    let (Some(amount0), Some(amount1)) = (data_word(log, 0), data_word(log, 1)) else {
        return Err(AdapterEventError::MalformedLog(WHAT));
    };
    Ok(DecodedCollectProtocol { amount0, amount1 })
}

fn decode_observation_growth_body(
    log: &Log,
) -> Result<DecodedObservationGrowth, AdapterEventError> {
    if IncreaseObservationCardinalityNext::decode_log_data_validate(&log.data).is_err() {
        return Err(AdapterEventError::MalformedLog(
            "malformed V3 IncreaseObservationCardinalityNext log",
        ));
    }
    const WHAT: &str = "missing V3 IncreaseObservationCardinalityNext data word";
    Ok(DecodedObservationGrowth {
        old: narrow_u16(log, 0, WHAT)?,
        new: narrow_u16(log, 1, WHAT)?,
    })
}

/// Read an indexed `address` topic (right-aligned in its 32-byte word).
fn topic_address(log: &Log, index: usize) -> Option<Address> {
    log.topics()
        .get(index)
        .map(|topic| Address::from_slice(&topic.as_slice()[12..]))
}

fn data_word(log: &Log, index: usize) -> Option<U256> {
    let start = index.checked_mul(32)?;
    log.data
        .data
        .get(start..start + 32)
        .map(U256::from_be_slice)
}

fn decode_swap_body(log: &Log) -> Result<DecodedSwap, AdapterEventError> {
    let amount0 = data_word(log, 0).ok_or(AdapterEventError::MalformedLog("missing V3 amount0"))?;
    let amount1 = data_word(log, 1).ok_or(AdapterEventError::MalformedLog("missing V3 amount1"))?;
    let sqrt_price_x96 =
        data_word(log, 2).ok_or(AdapterEventError::MalformedLog("missing V3 sqrtPriceX96"))?;
    let liquidity =
        data_word(log, 3).ok_or(AdapterEventError::MalformedLog("missing V3 liquidity"))?;
    let tick = int24_from_word(
        data_word(log, 4).ok_or(AdapterEventError::MalformedLog("missing V3 tick"))?,
    );
    let (amount0_negative, amount0) = signed_word_magnitude(amount0);
    let (amount1_negative, amount1) = signed_word_magnitude(amount1);
    Ok(DecodedSwap {
        amount0_negative,
        amount0,
        amount1_negative,
        amount1,
        sqrt_price_x96,
        liquidity,
        tick,
    })
}

fn decode_liquidity_body(log: &Log, is_mint: bool) -> Result<DecodedLiquidity, AdapterEventError> {
    let tick_lower = log
        .topics()
        .get(2)
        .copied()
        .map(|word| int24_from_word(U256::from_be_slice(word.as_slice())))
        .ok_or(AdapterEventError::MalformedLog(
            "missing Slipstream lower tick",
        ))?;
    let tick_upper = log
        .topics()
        .get(3)
        .copied()
        .map(|word| int24_from_word(U256::from_be_slice(word.as_slice())))
        .ok_or(AdapterEventError::MalformedLog(
            "missing Slipstream upper tick",
        ))?;
    let amount_word = data_word(log, usize::from(is_mint)).ok_or(
        AdapterEventError::MalformedLog("missing Slipstream liquidity amount"),
    )?;
    if amount_word > U256::from(u128::MAX) {
        return Err(AdapterEventError::MalformedLog(
            "Slipstream liquidity amount exceeds uint128",
        ));
    }
    Ok(DecodedLiquidity {
        tick_lower,
        tick_upper,
        amount: amount_word.to::<u128>(),
        is_mint,
    })
}

fn signed_word_magnitude(word: U256) -> (bool, U256) {
    if (word >> 255_usize).is_zero() {
        (false, word)
    } else {
        (true, (!word).wrapping_add(U256::from(1)))
    }
}

fn int24_from_word(word: U256) -> i32 {
    let raw = (word & U256::from(0x00FF_FFFFu32)).to::<u32>();
    if raw & 0x0080_0000 != 0 {
        (raw | 0xFF00_0000) as i32
    } else {
        raw as i32
    }
}

/// The low 128 bits of a 256-bit word (a `Tick.Info` word 0's `liquidityGross`).
#[cfg(test)]
fn u128_low(word: U256) -> u128 {
    let limbs = word.as_limbs();
    (limbs[0] as u128) | ((limbs[1] as u128) << 64)
}

/// The high 128 bits of a 256-bit word, as raw bits (word 0's `liquidityNet`,
/// two's-complement `int128`).
#[cfg(test)]
fn u128_high(word: U256) -> u128 {
    let limbs = word.as_limbs();
    (limbs[2] as u128) | ((limbs[3] as u128) << 64)
}

/// Pack `liquidityGross` (low 128) and `liquidityNet` (high 128, two's complement)
/// back into a `Tick.Info` word-0 value.
#[cfg(test)]
fn pack_gross_net(gross: u128, net: i128) -> U256 {
    U256::from(gross) | (U256::from(net as u128) << 128)
}

/// The `tickBitmap` bit index (0..256) for `tick`, matching the V3
/// `TickBitmap.position` low byte (`compressed % 256`, floor-toward-negative).
/// `tick_spacing` must be positive (guaranteed by [`layout_for`]).
#[cfg(test)]
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
#[cfg(test)]
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
    use super::super::PoolKey;
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
    fn record_warmed_slots_keeps_pool_slots_only() {
        let address = Address::repeat_byte(0x42);
        let other = Address::repeat_byte(0x43);
        let mut pool = PoolRegistration::new(PoolKey::UniswapV3(address)).with_metadata(
            ProtocolMetadata::UniswapV3(V3Metadata::default().with_tick_spacing(60)),
        );

        record_v3_warmed_slots(
            &mut pool,
            address,
            &[
                (address, U256::from(4)),
                (other, U256::from(9)),
                (address, U256::ZERO),
                (address, U256::from(4)),
            ],
        );

        let ProtocolMetadata::UniswapV3(metadata) = &pool.metadata else {
            panic!("metadata changed protocol");
        };
        assert_eq!(metadata.warmed_slots, vec![U256::ZERO, U256::from(4)]);
    }

    #[test]
    fn reviewed_slipstream_warms_the_complete_quote_runtime_path() {
        for family in [
            SlipstreamRuntimeFamily::AerodromeBaseBifi,
            SlipstreamRuntimeFamily::VelodromeOptimismBifi,
        ] {
            let reviewed = reviewed_slipstream_fee_runtime(family);
            let pool = PoolRegistration::new(PoolKey::Slipstream(reviewed.pool));
            let adapter = ConcentratedLiquidityAdapter::default();
            assert_eq!(
                adapter.verified_code_targets(&pool),
                vec![
                    reviewed.pool,
                    reviewed.implementation,
                    reviewed.factory,
                    reviewed.voter,
                    reviewed.module,
                ]
            );
            assert_eq!(
                adapter.verified_storage_targets(&pool),
                vec![
                    (reviewed.factory, U256::from(4)),
                    (
                        reviewed.voter,
                        solidity_address_mapping_key(reviewed.pool, 8),
                    ),
                    (
                        reviewed.voter,
                        solidity_address_mapping_key(reviewed.gauge, 20),
                    ),
                    (
                        reviewed.module,
                        solidity_address_mapping_key(reviewed.pool, 1),
                    ),
                ]
            );
            let dependencies = adapter.state_dependencies(&pool);
            for (address, slot) in adapter.verified_storage_targets(&pool) {
                assert!(
                    dependencies
                        .slots()
                        .contains(&StateSlot::new(address, slot)),
                    "reviewed external fee-runtime cell must be owned by the pool generation",
                );
            }
            let wrong_family = PoolRegistration::new(PoolKey::UniswapV3(reviewed.pool));
            assert!(
                adapter.verified_storage_targets(&wrong_family).is_empty(),
                "an address match cannot grant Slipstream state authority to another family",
            );
        }
    }

    #[test]
    fn v3_dependencies_use_warmed_slots_not_whole_account() {
        let address = Address::repeat_byte(0x44);
        let pool = PoolRegistration::new(PoolKey::UniswapV3(address)).with_metadata(
            ProtocolMetadata::UniswapV3(
                V3Metadata::default()
                    .with_tick_spacing(60)
                    .with_warmed_slots([U256::from(4), U256::ZERO, U256::from(4)]),
            ),
        );

        let dependencies = ConcentratedLiquidityAdapter::default().state_dependencies(&pool);

        assert_eq!(dependencies.associated_addresses(), &[address]);
        assert!(dependencies.whole_accounts().is_empty());
        assert_eq!(
            dependencies.slots(),
            &[
                StateSlot::new(address, U256::ZERO),
                StateSlot::new(address, U256::from(4)),
            ]
        );
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
