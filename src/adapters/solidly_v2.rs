use super::cold_start::{
    AdapterColdStartPlanner, ColdStartCall, ColdStartPlan, ColdStartResults, ColdStartRunReport,
    ColdStartStep, SlotFetch,
};
use super::factory::{FactoryConfig, PoolFactory, SolidlyFactory};
use super::sim::{
    ExactOutputQuote, POOL_TYPECall, SimConfig, SimError, SwapQuote, getAmountInWithTotalFeeCall,
    getAmountOutCall, getAmountOutWithTotalFeeCall, quote_via_call_from,
    quote_via_call_with_config,
};
use super::storage::{SolidlyStorageLayout, decode_address_slot};
use super::{
    AdapterCache, AdapterEvent, AdapterEventError, AdapterEventKind, AdapterEventResult,
    AeroVolatileRuntimeFamily, AmmAdapter, CallOutcome, ColdStartOutcome, ColdStartPolicy,
    ColdStartReport, DeferredWork, EventSource, PoolRegistration, PoolStatus, ProtocolId,
    ProtocolMetadata, RepairAction, SlotChange, SolidlyPoolVariant, SolidlyV2Metadata, StateUpdate,
    StateView, UnsupportedReason, UpdateQuality,
};
use alloy_primitives::{Address, B256, Bytes, Log, U256, address, b256, keccak256};
use alloy_sol_types::{SolCall, SolEvent};

/// `sol!`-generated pool event binding (crate-internal, not public API).
mod abi {
    alloy_sol_types::sol! {
        // Velodrome V2 / Aerodrome pools emit reserves as two separate uint256 values
        // (unlike Uniswap V2's packed uint112,uint112).
        event Sync(uint256 reserve0, uint256 reserve1);
    }
}
use abi::Sync;

/// `bytes32("V2_VOLATILE")`: the `POOL_TYPE()` label every Aero basic volatile
/// pool returns.
pub const AERO_VOLATILE_POOL_TYPE: B256 =
    b256!("56325f564f4c4154494c45000000000000000000000000000000000000000000");

/// Adapter for Solidly V2 (Aerodrome / Velodrome V2) reserves pools.
///
/// Mirrors the Uniswap V2 adapter (reserves + `Sync`), but reserves live in TWO
/// separate `uint256` storage slots (not V2's single packed slot), so reactive
/// writes are two plain slot writes. Swap simulation calls the pool's own
/// `getAmountOut(amountIn, tokenIn)`, which applies the stable (x³y+y³x) or
/// volatile (xy=k) invariant in-EVM — no math is reimplemented here.
///
/// The storage layout is config-supplied via [`SolidlyV2Metadata::storage_layout`]
/// ([`SolidlyStorageLayout`]); slot indices are fork-specific, so there is no
/// derivable default (cold-start returns [`UnsupportedReason::MissingMetadata`]
/// without one). Validate a fork's layout with the gated RPC-parity test.
///
/// # Aero (Dromos) volatile pools
///
/// A registration carrying [`SolidlyPoolVariant::AeroVolatile`] quotes through
/// the pool's `getAmountOutWithTotalFee` / `getAmountInWithTotalFee`, which
/// charge `factory.getFee(pool, msg.sender, …)` — the base fee plus the MEV
/// tax the factory's module derives from `tx.gasprice` — exactly as the
/// pool's `swap` does for the same sender. Quotes therefore run under the
/// [`SimConfig`] transaction context (`from`, `gas_price`). Cold start verifies
/// `POOL_TYPE() == "V2_VOLATILE"` and never calls `stable()`, which these
/// pools do not implement. The reviewed Optimism deployment
/// ([`AeroVolatileRuntimeFamily::AeroOptimismMooBifi`]) additionally pins its
/// factory, fee-module, exact-output fee-quoter and MEV-module runtimes and
/// warms the factory/module cells the getters read, so the production quote
/// path never relies on the lazy first-quote factory fetch. An Aero volatile
/// pool outside the reviewed family still quotes correctly against a
/// live-backed cache, but its factory and modules are fetched lazily on the
/// first quote: pre-warm them yourself before relying on offline quotes.
#[derive(Clone, Debug, Default)]
pub struct SolidlyV2Adapter {
    _private: (),
}

fn solidly_layout(pool: &PoolRegistration) -> Option<SolidlyStorageLayout> {
    match &pool.metadata {
        ProtocolMetadata::SolidlyV2(metadata) => metadata.storage_layout,
        _ => None,
    }
}

fn solidly_variant(pool: &PoolRegistration) -> SolidlyPoolVariant {
    match &pool.metadata {
        ProtocolMetadata::SolidlyV2(metadata) => metadata.variant,
        _ => SolidlyPoolVariant::Legacy,
    }
}

/// Reviewed Aero basic-pool deployment: every runtime the total-fee quote
/// path executes, with the runtime keccak observed at the pinned block.
#[derive(Clone, Copy)]
struct ReviewedAeroVolatileRuntime {
    pool: Address,
    proxy_runtime_code_hash: B256,
    implementation: Address,
    implementation_runtime_code_hash: B256,
    factory: Address,
    factory_runtime_code_hash: B256,
    fee_module: Address,
    fee_module_runtime_code_hash: B256,
    exact_out_fee_quoter: Address,
    exact_out_fee_quoter_runtime_code_hash: B256,
    mev_tax_module: Address,
    mev_tax_module_runtime_code_hash: B256,
}

/// Aero `PoolFactory` storage: `defaultFee`, packed `feeModule` +
/// `feeModuleGasLimit`, packed `mevTaxModule` + `mevTaxModuleGasLimit`, and
/// `exactOutFeeQuoter`. Read at Optimism block 158800699 against the getters
/// and the `debug_traceCall` read set of `getAmountOutWithTotalFee` /
/// `getAmountInWithTotalFee`.
const AERO_VOLATILE_FACTORY_DEFAULT_FEE_SLOT: u64 = 5;
const AERO_VOLATILE_FACTORY_FEE_MODULE_SLOT: u64 = 7;
const AERO_VOLATILE_FACTORY_MEV_TAX_MODULE_SLOT: u64 = 13;
const AERO_VOLATILE_FACTORY_EXACT_OUT_FEE_QUOTER_SLOT: u64 = 14;
/// `CustomFeeModule.customFee[pool]` mapping base slot.
const AERO_VOLATILE_FEE_MODULE_CUSTOM_FEE_SLOT: u64 = 0;
/// `PriorityFeeMevTaxModule`: `pendingOwner` + `priorityFeeMultiplier` (slot 1)
/// and `minThreshold` + `baseFeeFactor` (slot 2).
const AERO_VOLATILE_MEV_MODULE_MULTIPLIER_SLOT: u64 = 1;
const AERO_VOLATILE_MEV_MODULE_THRESHOLD_SLOT: u64 = 2;
/// Aero basic pool `factory` cell (the getters read it to reach `getFee`).
const AERO_VOLATILE_POOL_FACTORY_SLOT: u64 = 10;

fn reviewed_aero_volatile_runtime(
    family: AeroVolatileRuntimeFamily,
) -> ReviewedAeroVolatileRuntime {
    match family {
        AeroVolatileRuntimeFamily::AeroOptimismMooBifi => ReviewedAeroVolatileRuntime {
            pool: address!("c71b571aa9f06bfa096c24e9df5c45fc4fab4151"),
            proxy_runtime_code_hash: b256!(
                "7604f8b17690bd85de7be0b616a0c999322a6d44ae6f6786c314e5644c78a06a"
            ),
            implementation: address!("0526cf035c1156b5c598b0d3c2445e38af2e8bde"),
            implementation_runtime_code_hash: b256!(
                "15269fbb3aca4305485be2669a8394ec6a14c75181e1eb4c2d46c5b1ee2a7782"
            ),
            factory: address!("8e6a9fcd1d638cc760ffba397605fe3355de76d2"),
            factory_runtime_code_hash: b256!(
                "bb6f945479608857a220f9643db76e877f1e52844d953d980c18f5ea38d9574b"
            ),
            fee_module: address!("7949de8d67e4f6b8509bdbb43ff0503af1247a7a"),
            fee_module_runtime_code_hash: b256!(
                "a56614fe447178e62c473ff1333d660c1dbb70965b4a059fae228bf52457c0f2"
            ),
            exact_out_fee_quoter: address!("95f6b665a1a97878353737ecb3d68aa978a5cb9f"),
            exact_out_fee_quoter_runtime_code_hash: b256!(
                "d102c04e1dded8e498df1f213a61af9a0cf59c4890d1ea103d4d0bb0f4a3f38d"
            ),
            mev_tax_module: address!("5de55ebe5b789bd309809c7969334c2c16e64eaf"),
            mev_tax_module_runtime_code_hash: b256!(
                "59de6c7ead685b6295ef7dccb7ee30cd2881c697267d09718790f998db3cf016"
            ),
        },
    }
}

fn reviewed_aero_volatile_runtime_for_pool(
    pool: &PoolRegistration,
) -> Option<(AeroVolatileRuntimeFamily, ReviewedAeroVolatileRuntime)> {
    if solidly_variant(pool) != SolidlyPoolVariant::AeroVolatile {
        return None;
    }
    let address = pool.key.address()?;
    [AeroVolatileRuntimeFamily::AeroOptimismMooBifi]
        .into_iter()
        .map(|family| (family, reviewed_aero_volatile_runtime(family)))
        .find(|(_, reviewed)| reviewed.pool == address)
}

fn solidity_address_mapping_key(key: Address, mapping_slot: u64) -> U256 {
    let mut preimage = [0_u8; 64];
    preimage[12..32].copy_from_slice(key.as_slice());
    preimage[32..].copy_from_slice(&U256::from(mapping_slot).to_be_bytes::<32>());
    U256::from_be_slice(keccak256(preimage).as_slice())
}

fn reviewed_aero_volatile_storage_targets(
    reviewed: ReviewedAeroVolatileRuntime,
) -> Vec<(Address, U256)> {
    vec![
        (reviewed.pool, U256::from(AERO_VOLATILE_POOL_FACTORY_SLOT)),
        (
            reviewed.factory,
            U256::from(AERO_VOLATILE_FACTORY_DEFAULT_FEE_SLOT),
        ),
        (
            reviewed.factory,
            U256::from(AERO_VOLATILE_FACTORY_FEE_MODULE_SLOT),
        ),
        (
            reviewed.factory,
            U256::from(AERO_VOLATILE_FACTORY_MEV_TAX_MODULE_SLOT),
        ),
        (
            reviewed.factory,
            U256::from(AERO_VOLATILE_FACTORY_EXACT_OUT_FEE_QUOTER_SLOT),
        ),
        (
            reviewed.fee_module,
            solidity_address_mapping_key(reviewed.pool, AERO_VOLATILE_FEE_MODULE_CUSTOM_FEE_SLOT),
        ),
        (
            reviewed.mev_tax_module,
            U256::from(AERO_VOLATILE_MEV_MODULE_MULTIPLIER_SLOT),
        ),
        (
            reviewed.mev_tax_module,
            U256::from(AERO_VOLATILE_MEV_MODULE_THRESHOLD_SLOT),
        ),
    ]
}

fn reviewed_aero_volatile_identities(
    reviewed: ReviewedAeroVolatileRuntime,
) -> Vec<(Address, B256)> {
    vec![
        (reviewed.pool, reviewed.proxy_runtime_code_hash),
        (
            reviewed.implementation,
            reviewed.implementation_runtime_code_hash,
        ),
        (reviewed.factory, reviewed.factory_runtime_code_hash),
        (reviewed.fee_module, reviewed.fee_module_runtime_code_hash),
        (
            reviewed.exact_out_fee_quoter,
            reviewed.exact_out_fee_quoter_runtime_code_hash,
        ),
        (
            reviewed.mev_tax_module,
            reviewed.mev_tax_module_runtime_code_hash,
        ),
    ]
}

impl SolidlyV2Adapter {
    /// The reviewed Aero volatile deployment family a registration belongs
    /// to, if it registered as [`SolidlyPoolVariant::AeroVolatile`] under a
    /// reviewed pool address.
    pub fn aero_volatile_runtime_family(
        pool: &PoolRegistration,
    ) -> Option<AeroVolatileRuntimeFamily> {
        reviewed_aero_volatile_runtime_for_pool(pool).map(|(family, _)| family)
    }

    /// The pinned runtime identities (`address → runtime keccak`) of a reviewed
    /// Aero volatile family: pool proxy, implementation, factory, fee module,
    /// exact-output fee quoter and MEV-tax module.
    pub fn aero_volatile_runtime_identities(
        family: AeroVolatileRuntimeFamily,
    ) -> Vec<(Address, B256)> {
        reviewed_aero_volatile_identities(reviewed_aero_volatile_runtime(family))
    }
}

impl AmmAdapter for SolidlyV2Adapter {
    fn protocol(&self) -> ProtocolId {
        ProtocolId::SolidlyV2
    }

    fn event_sources(&self, pool: &PoolRegistration) -> Vec<EventSource> {
        pool.key
            .address()
            .map(|address| EventSource::direct(address, vec![Sync::SIGNATURE_HASH]))
            .into_iter()
            .collect()
    }

    fn pool_factories(&self, config: &FactoryConfig) -> Vec<Box<dyn PoolFactory>> {
        config
            .solidly
            .iter()
            .map(|solidly| {
                // The config-level `verify_derivations` is a global off-switch: a
                // config's CREATE2 cross-check runs only when both it and the
                // global flag opt in.
                let mut solidly = solidly.clone();
                solidly.verify_derivations &= config.verify_derivations;
                Box::new(SolidlyFactory::new(solidly)) as Box<dyn PoolFactory>
            })
            .collect()
    }

    fn verified_code_targets(&self, pool: &PoolRegistration) -> Vec<Address> {
        reviewed_aero_volatile_runtime_for_pool(pool)
            .map(|(_, reviewed)| {
                vec![
                    reviewed.pool,
                    reviewed.implementation,
                    reviewed.factory,
                    reviewed.fee_module,
                    reviewed.exact_out_fee_quoter,
                    reviewed.mev_tax_module,
                ]
            })
            .unwrap_or_default()
    }

    fn verified_storage_targets(&self, pool: &PoolRegistration) -> Vec<(Address, U256)> {
        reviewed_aero_volatile_runtime_for_pool(pool)
            .map(|(_, reviewed)| reviewed_aero_volatile_storage_targets(reviewed))
            .unwrap_or_default()
    }

    fn reviewed_code_identities(&self, pool: &PoolRegistration) -> Vec<(Address, B256)> {
        reviewed_aero_volatile_runtime_for_pool(pool)
            .map(|(_, reviewed)| reviewed_aero_volatile_identities(reviewed))
            .unwrap_or_default()
    }

    fn cold_start_planner(
        &self,
        pool: &PoolRegistration,
        policy: ColdStartPolicy,
    ) -> Result<Box<dyn AdapterColdStartPlanner>, UnsupportedReason> {
        let Some(address) = pool.key.address() else {
            return Err(UnsupportedReason::Custom(
                "Solidly V2 pool key is not address-keyed".into(),
            ));
        };
        let Some(layout) = solidly_layout(pool) else {
            return Err(UnsupportedReason::MissingMetadata(
                "Solidly V2 storage layout",
            ));
        };
        // reserve0/reserve1/token0/token1 are four distinct storage variables on
        // a real pool; a colliding layout would silently corrupt the cold-start
        // verdict and token decode (and clobber one reserve write), so reject it.
        let slots = [
            layout.reserve0_slot,
            layout.reserve1_slot,
            layout.token0_slot,
            layout.token1_slot,
        ];
        for i in 0..slots.len() {
            for j in (i + 1)..slots.len() {
                if slots[i] == slots[j] {
                    return Err(UnsupportedReason::Custom(
                        "Solidly V2 storage layout slots must be pairwise distinct".into(),
                    ));
                }
            }
        }
        Ok(Box::new(SolidlyV2ColdStartPlanner::new(
            address,
            layout,
            policy,
            solidly_variant(pool),
            self.verified_storage_targets(pool),
        )))
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
                "malformed Solidly V2 Sync log",
            ));
        }

        let Some(address) = pool.key.address() else {
            return AdapterEventResult::error(AdapterEventError::MalformedLog(
                "Solidly V2 pool key is not address-keyed",
            ));
        };
        let Some(layout) = solidly_layout(pool) else {
            // A missing layout is a config issue (the pool was not cold-started),
            // not a malformed log. Skip the event rather than returning an error
            // that would fail the whole reactive batch — and without a layout
            // there are no slots to target anyway.
            return AdapterEventResult::ignored();
        };

        let Some(reserve0) = data_word(log, 0) else {
            return AdapterEventResult::error(AdapterEventError::MalformedLog(
                "missing Solidly V2 reserve0",
            ));
        };
        let Some(reserve1) = data_word(log, 1) else {
            return AdapterEventResult::error(AdapterEventError::MalformedLog(
                "missing Solidly V2 reserve1",
            ));
        };

        // Two exact full-slot writes from the event payload (no fetch) — Solidly
        // stores the reserves unpacked, one uint256 per slot.
        AdapterEventResult::event(AdapterEvent {
            pool: pool.key.clone(),
            emitter: log.address,
            topic0: Sync::SIGNATURE_HASH,
            kind: AdapterEventKind::Sync,
            updates: vec![
                StateUpdate::slot(address, layout.reserve0_slot, reserve0),
                StateUpdate::slot(address, layout.reserve1_slot, reserve1),
            ],
            quality: UpdateQuality::ExactIfApplied,
            repair: RepairAction::None,
        })
    }

    // No `after_apply` override (unlike V2): Solidly stores each reserve in its
    // own full uint256 slot, so decode_event's absolute `StateUpdate::slot`
    // writes are always exact — a full Slot write is never cold-skipped (unlike
    // V2's masked write into a packed slot), so `StateDiff::has_skipped()` can
    // never be true here and the default `RepairAction::None` is correct. No
    // cold-slot resync is ever needed.

    /// Quote via the pool's own `getAmountOut(amountIn, tokenIn)` (chain code, no
    /// reimplemented math). Beyond the warmed reserves the pool also reads its
    /// `stable` flag + `token0`/`decimals0`/`decimals1` and STATICCALLs the
    /// factory's `getFee`, so the quote is NOT reproducible from warmed reserves
    /// alone — those slots and the factory's bytecode must be reachable (lazily
    /// fetched from a live backend, or installed for offline tests). `token_out`
    /// is implied (the pool's other token), so it is not part of the call.
    ///
    /// An [`AeroVolatile`](SolidlyPoolVariant::AeroVolatile) registration
    /// quotes through `getAmountOutWithTotalFee(amountIn, tokenIn)` under the
    /// configured transaction context instead, so the MEV tax the factory's
    /// module derives from `tx.gasprice` and the `msg.sender`-keyed fee match
    /// what the pool's `swap` charges that sender.
    fn simulate_swap(
        &self,
        pool: &PoolRegistration,
        cache: &mut dyn AdapterCache,
        token_in: Address,
        _token_out: Address,
        amount_in: U256,
        config: &SimConfig,
    ) -> Result<SwapQuote, SimError> {
        let pool_address = pool
            .key
            .address()
            .ok_or(SimError::MissingMetadata("Solidly V2 pool address"))?;

        let amount_out = match solidly_variant(pool) {
            SolidlyPoolVariant::Legacy => {
                let calldata = Bytes::from(
                    getAmountOutCall {
                        amountIn: amount_in,
                        tokenIn: token_in,
                    }
                    .abi_encode(),
                );
                let output = quote_via_call_from(cache, config.from, pool_address, calldata)?;
                getAmountOutCall::abi_decode_returns_validate(&output)
                    .map_err(|_| SimError::MalformedOutput("getAmountOut return"))?
            }
            SolidlyPoolVariant::AeroVolatile => {
                let calldata = Bytes::from(
                    getAmountOutWithTotalFeeCall {
                        amountIn: amount_in,
                        tokenIn: token_in,
                    }
                    .abi_encode(),
                );
                let output = quote_via_call_with_config(cache, config, pool_address, calldata)?;
                getAmountOutWithTotalFeeCall::abi_decode_returns_validate(&output)
                    .map_err(|_| SimError::MalformedOutput("getAmountOutWithTotalFee return"))?
            }
        };
        Ok(SwapQuote::new(amount_out))
    }

    /// Exact-output quote for an [`AeroVolatile`](SolidlyPoolVariant::AeroVolatile)
    /// pool through `getAmountInWithTotalFee(amountOut, tokenOut)`: the curve
    /// input grossed up by `factory.getFeeForAmountIn(pool, msg.sender, …)`
    /// under the configured transaction context. `token_in` is implied (the
    /// pool's other token). Legacy pools expose no exact-output getter and
    /// return [`SimError::Unsupported`].
    fn simulate_swap_exact_output(
        &self,
        pool: &PoolRegistration,
        cache: &mut dyn AdapterCache,
        _token_in: Address,
        token_out: Address,
        amount_out: U256,
        config: &SimConfig,
    ) -> Result<ExactOutputQuote, SimError> {
        if solidly_variant(pool) != SolidlyPoolVariant::AeroVolatile {
            return Err(SimError::Unsupported(ProtocolId::SolidlyV2));
        }
        let pool_address = pool
            .key
            .address()
            .ok_or(SimError::MissingMetadata("Solidly V2 pool address"))?;
        let calldata = Bytes::from(
            getAmountInWithTotalFeeCall {
                amountOut: amount_out,
                tokenOut: token_out,
            }
            .abi_encode(),
        );
        let output = quote_via_call_with_config(cache, config, pool_address, calldata)?;
        let amount_in = getAmountInWithTotalFeeCall::abi_decode_returns_validate(&output)
            .map_err(|_| SimError::MalformedOutput("getAmountInWithTotalFee return"))?;
        Ok(ExactOutputQuote::new(amount_in))
    }
}

/// The classified verdict of the mandatory reserve slots.
#[derive(Clone, Copy)]
enum SolidlyVerdict {
    /// Both reserves warmed; pool is ready.
    Ready,
    /// A reserve read a genuine on-chain zero (degenerate pool).
    DegenerateZero,
    /// A reserve could not be fetched (archive / historical miss).
    FetchFailed,
}

/// Cold-start planner for a Solidly V2 pool: a single verify-only round.
///
/// Verifies `reserve0`/`reserve1` (always — both mandatory) plus the token slots
/// under `Strict`/`Eager`. The reserves are classified from their per-slot
/// [`SlotFetch`] verdict, so a genuine zero and an archive miss map to *distinct*
/// repairs. Token addresses are decoded from the warmed slots and merged into the
/// metadata; the config-supplied `stable`/`storage_layout`/`variant` are
/// preserved. Under `Lazy` the token slots are recorded as deferred work.
///
/// An [`AeroVolatile`](SolidlyPoolVariant::AeroVolatile) pool additionally
/// proves its identity with a `POOL_TYPE()` view call (it must return
/// `bytes32("V2_VOLATILE")`; a legacy pool reverts) and, when it belongs to a
/// reviewed family, verifies the external factory/module cells its total-fee
/// getters read under `Strict`/`Eager`. A failed external cell degrades the
/// pool with a targeted repair; a failed identity check fails closed as
/// `Unsupported`.
struct SolidlyV2ColdStartPlanner {
    address: Address,
    layout: SolidlyStorageLayout,
    policy: ColdStartPolicy,
    variant: SolidlyPoolVariant,
    external_exact_slots: Vec<(Address, U256)>,
    verified_slots: Vec<(Address, U256)>,
    changed_slots: Vec<SlotChange>,
    failed_external_slots: Vec<(Address, U256)>,
    /// Reviewed external cells fetched as genuine zero. They are materialized
    /// explicitly so an offline quote never treats them as unknown.
    proven_zero_external_slots: Vec<(Address, U256)>,
    decoded_token0: Option<Address>,
    decoded_token1: Option<Address>,
    verdict: Option<SolidlyVerdict>,
    /// `Some(false)` once the `POOL_TYPE()` identity call contradicted the
    /// registered variant.
    identity_verified: Option<bool>,
}

impl SolidlyV2ColdStartPlanner {
    fn new(
        address: Address,
        layout: SolidlyStorageLayout,
        policy: ColdStartPolicy,
        variant: SolidlyPoolVariant,
        external_exact_slots: Vec<(Address, U256)>,
    ) -> Self {
        Self {
            address,
            layout,
            policy,
            variant,
            external_exact_slots,
            verified_slots: Vec::new(),
            changed_slots: Vec::new(),
            failed_external_slots: Vec::new(),
            proven_zero_external_slots: Vec::new(),
            decoded_token0: None,
            decoded_token1: None,
            verdict: None,
            identity_verified: None,
        }
    }
}

/// Classify a single reserve slot from its per-slot [`SlotFetch`] outcome.
fn classify_slot(results: &ColdStartResults, address: Address, slot: U256) -> SolidlyVerdict {
    match results
        .fetched
        .iter()
        .find(|o| o.address == address && o.slot == slot)
        .map(|o| &o.fetch)
    {
        Some(SlotFetch::Value(_)) => SolidlyVerdict::Ready,
        Some(SlotFetch::Zero) => SolidlyVerdict::DegenerateZero,
        Some(SlotFetch::FetchFailed { .. }) | Some(SlotFetch::NotAttempted) | None => {
            SolidlyVerdict::FetchFailed
        }
    }
}

impl AdapterColdStartPlanner for SolidlyV2ColdStartPlanner {
    fn initial_plan(&mut self, _state: &dyn StateView) -> ColdStartPlan {
        // Both reserve slots are always warmed so subsequent reactive `Sync`
        // writes land exactly. The token slots are warmed up-front except under
        // `Lazy`/`HotSlotsOnly`.
        let mut verify: Vec<(Address, U256)> = match self.policy {
            ColdStartPolicy::Strict | ColdStartPolicy::Eager => vec![
                (self.address, self.layout.reserve0_slot),
                (self.address, self.layout.reserve1_slot),
                (self.address, self.layout.token0_slot),
                (self.address, self.layout.token1_slot),
            ],
            ColdStartPolicy::Lazy | ColdStartPolicy::HotSlotsOnly => vec![
                (self.address, self.layout.reserve0_slot),
                (self.address, self.layout.reserve1_slot),
            ],
        };
        if matches!(
            self.policy,
            ColdStartPolicy::Strict | ColdStartPolicy::Eager
        ) {
            verify.extend(self.external_exact_slots.iter().copied());
        }
        self.verified_slots = verify.clone();
        let discover = if self.variant == SolidlyPoolVariant::AeroVolatile {
            vec![ColdStartCall {
                from: Address::ZERO,
                to: self.address,
                calldata: Bytes::from(POOL_TYPECall {}.abi_encode()),
                restrict_to: Some(vec![self.address]),
            }]
        } else {
            Vec::new()
        };
        ColdStartPlan {
            verify,
            discover,
            ..Default::default()
        }
    }

    fn on_results(&mut self, results: &ColdStartResults, state: &dyn StateView) -> ColdStartStep {
        self.changed_slots.extend(results.verified.iter().cloned());

        // Both reserves are mandatory: combine their per-slot verdicts —
        // FetchFailed dominates (archive miss), then DegenerateZero.
        let r0 = classify_slot(results, self.address, self.layout.reserve0_slot);
        let r1 = classify_slot(results, self.address, self.layout.reserve1_slot);
        self.verdict = Some(match (r0, r1) {
            (SolidlyVerdict::FetchFailed, _) | (_, SolidlyVerdict::FetchFailed) => {
                SolidlyVerdict::FetchFailed
            }
            (SolidlyVerdict::DegenerateZero, _) | (_, SolidlyVerdict::DegenerateZero) => {
                SolidlyVerdict::DegenerateZero
            }
            (SolidlyVerdict::Ready, SolidlyVerdict::Ready) => SolidlyVerdict::Ready,
        });

        if matches!(
            self.policy,
            ColdStartPolicy::Strict | ColdStartPolicy::Eager
        ) {
            for (address, slot) in &self.external_exact_slots {
                let fetched = results
                    .fetched
                    .iter()
                    .find(|outcome| outcome.address == *address && outcome.slot == *slot);
                let resident = matches!(
                    fetched.map(|outcome| &outcome.fetch),
                    Some(SlotFetch::Value(_) | SlotFetch::Zero)
                ) || state.storage(*address, *slot).is_some();
                if !resident {
                    self.failed_external_slots.push((*address, *slot));
                } else if matches!(fetched.map(|outcome| &outcome.fetch), Some(SlotFetch::Zero)) {
                    self.proven_zero_external_slots.push((*address, *slot));
                }
            }
        }

        if self.variant == SolidlyPoolVariant::AeroVolatile {
            let pool_type = results
                .discovered
                .first()
                .and_then(|call| match &call.result {
                    CallOutcome::Success { output, .. } if output.len() >= 32 => {
                        Some(B256::from_slice(&output[..32]))
                    }
                    _ => None,
                });
            self.identity_verified = Some(pool_type == Some(AERO_VOLATILE_POOL_TYPE));
        }

        self.decoded_token0 = state
            .storage(self.address, self.layout.token0_slot)
            .map(decode_address_slot);
        self.decoded_token1 = state
            .storage(self.address, self.layout.token1_slot)
            .map(decode_address_slot);

        ColdStartStep::Done
    }

    fn materialization_updates(&self) -> Vec<StateUpdate> {
        self.proven_zero_external_slots
            .iter()
            .map(|(address, slot)| StateUpdate::slot(*address, *slot, U256::ZERO))
            .collect()
    }

    fn finish(
        &mut self,
        pool: &mut PoolRegistration,
        _report: &ColdStartRunReport,
    ) -> ColdStartOutcome {
        if self.identity_verified == Some(false) {
            // Fail closed: the registered variant promised an Aero basic pool
            // but the runtime did not report `POOL_TYPE() == "V2_VOLATILE"`.
            // Quoting it through the total-fee getters would revert or, worse,
            // decode a legacy pool's unrelated selector collision.
            pool.status = PoolStatus::Degraded;
            return ColdStartOutcome::Unsupported(UnsupportedReason::Custom(
                "Aero volatile registration: POOL_TYPE() did not return V2_VOLATILE".into(),
            ));
        }

        let mut cold_report = ColdStartReport::new(pool.key.clone(), self.policy);
        cold_report.verified_slots = self.verified_slots.clone();
        cold_report.changed_slots = self.changed_slots.clone();

        let reserve_slots = vec![
            (self.address, self.layout.reserve0_slot),
            (self.address, self.layout.reserve1_slot),
        ];

        match self.verdict {
            Some(SolidlyVerdict::DegenerateZero) => {
                cold_report.status = PoolStatus::Degraded;
                // Distinct from the archive-miss repair: a genuine zero is a
                // degenerate pool, so purge the stale slots rather than re-verify.
                ColdStartOutcome::NeedsRepair(
                    cold_report,
                    RepairAction::PurgeSlots {
                        address: self.address,
                        slots: vec![self.layout.reserve0_slot, self.layout.reserve1_slot],
                    },
                )
            }
            Some(SolidlyVerdict::FetchFailed) | None => {
                cold_report.status = PoolStatus::Degraded;
                ColdStartOutcome::NeedsRepair(cold_report, RepairAction::VerifySlots(reserve_slots))
            }
            Some(SolidlyVerdict::Ready) if !self.failed_external_slots.is_empty() => {
                // The pool's own state is exact but a reviewed external fee cell
                // could not be fetched: the total-fee quote would fall back to a
                // lazy provider read, so hold the pool until the cells arrive.
                cold_report.status = PoolStatus::Degraded;
                pool.status = PoolStatus::Degraded;
                ColdStartOutcome::NeedsRepair(
                    cold_report,
                    RepairAction::VerifySlots(self.failed_external_slots.clone()),
                )
            }
            Some(SolidlyVerdict::Ready) => {
                // Merge decoded tokens; preserve config `stable`/`storage_layout`/`variant`.
                let metadata = match &pool.metadata {
                    ProtocolMetadata::SolidlyV2(existing) => SolidlyV2Metadata {
                        token0: self.decoded_token0,
                        token1: self.decoded_token1,
                        stable: existing.stable,
                        storage_layout: existing.storage_layout,
                        variant: existing.variant,
                    },
                    _ => SolidlyV2Metadata {
                        token0: self.decoded_token0,
                        token1: self.decoded_token1,
                        stable: None,
                        storage_layout: Some(self.layout),
                        variant: self.variant,
                    },
                };
                pool.metadata = ProtocolMetadata::SolidlyV2(metadata);
                pool.status = PoolStatus::Ready;
                cold_report.status = PoolStatus::Ready;

                if self.policy == ColdStartPolicy::Lazy {
                    let deferred = vec![DeferredWork::VerifySlots(vec![
                        (self.address, self.layout.token0_slot),
                        (self.address, self.layout.token1_slot),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::PoolKey;

    fn aero_pool() -> PoolRegistration {
        PoolRegistration::new(PoolKey::SolidlyV2(address!(
            "c71b571aa9f06bfa096c24e9df5c45fc4fab4151"
        )))
        .with_metadata(ProtocolMetadata::SolidlyV2(
            SolidlyV2Metadata::default().with_aero_volatile(),
        ))
    }

    #[test]
    fn pool_type_label_is_v2_volatile() {
        let mut expected = [0_u8; 32];
        expected[..11].copy_from_slice(b"V2_VOLATILE");
        assert_eq!(AERO_VOLATILE_POOL_TYPE, B256::from(expected));
    }

    #[test]
    fn reviewed_aero_volatile_family_is_keyed_by_pool_and_variant() {
        let adapter = SolidlyV2Adapter::default();
        let pool = aero_pool();
        assert_eq!(
            SolidlyV2Adapter::aero_volatile_runtime_family(&pool),
            Some(AeroVolatileRuntimeFamily::AeroOptimismMooBifi)
        );
        let reviewed =
            reviewed_aero_volatile_runtime(AeroVolatileRuntimeFamily::AeroOptimismMooBifi);
        assert_eq!(
            adapter.verified_code_targets(&pool),
            vec![
                reviewed.pool,
                reviewed.implementation,
                reviewed.factory,
                reviewed.fee_module,
                reviewed.exact_out_fee_quoter,
                reviewed.mev_tax_module,
            ]
        );
        let identities = adapter.reviewed_code_identities(&pool);
        assert_eq!(identities.len(), 6);
        assert!(identities.contains(&(
            reviewed.implementation,
            b256!("15269fbb3aca4305485be2669a8394ec6a14c75181e1eb4c2d46c5b1ee2a7782")
        )));
        let targets = adapter.verified_storage_targets(&pool);
        assert_eq!(targets.len(), 8);
        assert!(targets.contains(&(reviewed.factory, U256::from(5))));
        assert!(targets.contains(&(reviewed.factory, U256::from(7))));
        assert!(targets.contains(&(reviewed.factory, U256::from(13))));
        assert!(targets.contains(&(reviewed.factory, U256::from(14))));
        assert!(targets.contains(&(reviewed.mev_tax_module, U256::from(1))));
        assert!(targets.contains(&(reviewed.mev_tax_module, U256::from(2))));
        assert!(targets.contains(&(reviewed.pool, U256::from(10))));
        // `customFee[pool]` at slot 0 of the fee module, confirmed by the
        // traced read set at Optimism block 158800699.
        assert!(targets.contains(&(
            reviewed.fee_module,
            U256::from_be_slice(&alloy_primitives::hex!(
                "cb21fa02f97b30809db1425d6adfdd69eb3e642f7297ea54983670aeb8dd5500"
            )),
        )));

        // The same address registered as a legacy pool gets no Aero authority.
        let legacy = PoolRegistration::new(PoolKey::SolidlyV2(reviewed.pool)).with_metadata(
            ProtocolMetadata::SolidlyV2(
                SolidlyV2Metadata::default()
                    .with_storage_layout(SolidlyStorageLayout::aero_volatile()),
            ),
        );
        assert_eq!(
            SolidlyV2Adapter::aero_volatile_runtime_family(&legacy),
            None
        );
        assert!(adapter.verified_code_targets(&legacy).is_empty());
        assert!(adapter.verified_storage_targets(&legacy).is_empty());
        assert!(adapter.reviewed_code_identities(&legacy).is_empty());

        // An unreviewed Aero volatile pool keeps the variant but pins nothing.
        let unreviewed = PoolRegistration::new(PoolKey::SolidlyV2(Address::repeat_byte(0x77)))
            .with_metadata(ProtocolMetadata::SolidlyV2(
                SolidlyV2Metadata::default().with_aero_volatile(),
            ));
        assert_eq!(
            SolidlyV2Adapter::aero_volatile_runtime_family(&unreviewed),
            None
        );
        assert!(adapter.verified_storage_targets(&unreviewed).is_empty());
    }

    #[test]
    fn aero_volatile_metadata_builder_sets_variant_and_layout() {
        let metadata = SolidlyV2Metadata::default().with_aero_volatile();
        assert_eq!(metadata.variant, SolidlyPoolVariant::AeroVolatile);
        assert_eq!(
            metadata.storage_layout,
            Some(SolidlyStorageLayout::aero_volatile())
        );
        assert_eq!(metadata.stable, Some(false));
        assert_eq!(
            SolidlyV2Metadata::default().variant,
            SolidlyPoolVariant::Legacy
        );
    }
}
