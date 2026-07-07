use super::cold_start::{
    AdapterColdStartPlanner, ColdStartCall, ColdStartPlan, ColdStartResults, ColdStartRunReport,
    ColdStartStep, SlotFetch,
};
use super::sim::{
    BatchSwapStep, FundManagement, SimConfig, SimError, SwapQuote, queryBatchSwapCall,
    quote_via_call_from,
};
use super::{
    AdapterCache, AdapterEvent, AdapterEventError, AdapterEventKind, AdapterEventResult,
    AmmAdapter, BalancerTokenBalance, BalancerV2Metadata, ColdStartOutcome, ColdStartPolicy,
    ColdStartReport, EventSource, PoolRegistration, PoolStatus, ProtocolId, ProtocolMetadata,
    RepairAction, SlotChange, StateUpdate, StateView, UnsupportedReason, UpdateQuality,
};
use alloy_primitives::{Address, B256, Bytes, Log, U256};
use alloy_sol_types::{SolCall, SolEvent};

/// `sol!`-generated vault ABI bindings (crate-internal, not public API):
/// the `Swap` / `PoolBalanceChanged` events and the `getPoolTokens`
/// cold-start discovery call.
mod abi {
    alloy_sol_types::sol! {
        event Swap(bytes32 indexed poolId, address indexed tokenIn, address indexed tokenOut, uint256 amountIn, uint256 amountOut);
        /// Emitted by the vault on a join or exit — it changes the pool's vault
        /// balances, so it is subscribed and resynced (event-sourcing its `deltas` /
        /// `protocolFeeAmounts` is a follow-up). `topic0 = 0xe5ce2490…`.
        event PoolBalanceChanged(bytes32 indexed poolId, address indexed liquidityProvider, address[] tokens, int256[] deltas, uint256[] protocolFeeAmounts);

        /// Balancer V2 vault `getPoolTokens` for cold-start discovery.
        function getPoolTokens(bytes32 poolId)
            returns (address[] tokens, uint256[] balances, uint256 lastChangeBlock);
    }
}
use abi::{PoolBalanceChanged, Swap, getPoolTokensCall};

/// Width of the vault `BalanceAllocation` `cash` field (bits): a packed balance is
/// `[lastChangeBlock : top 32][managed : bits 112–223][cash : bits 0–111]`.
const CASH_BITS: usize = 112;

/// Mask selecting a 112-bit `cash` field.
fn cash_mask() -> U256 {
    (U256::from(1) << CASH_BITS) - U256::from(1)
}

/// Balancer V2 pool specialization encoded in the poolId after the 20-byte pool
/// address. `2` is the only specialization where the high 112-bit balance field
/// is another token's `cash`; GENERAL/MINIMAL pools use it for `managed`.
const TWO_TOKEN_SPECIALIZATION: u16 = 2;

fn is_two_token_pool(pool_id: B256) -> bool {
    let bytes = pool_id.as_slice();
    u16::from_be_bytes([bytes[20], bytes[21]]) == TWO_TOKEN_SPECIALIZATION
}

/// Extract the 112-bit `cash` field at the low (bits 0–111) or high (bits
/// 112–223) position of a packed vault balance word.
fn cash_field(word: U256, high: bool) -> U256 {
    let shift = if high { CASH_BITS } else { 0 };
    (word >> shift) & cash_mask()
}

/// Set the 112-bit `cash` field of `word`, preserving the other bits (`managed` /
/// `lastChangeBlock`, or the co-tenant token's `cash` in a TWO_TOKEN slot).
fn set_cash_field(word: U256, high: bool, cash: U256) -> U256 {
    let shift = if high { CASH_BITS } else { 0 };
    let cleared = word & (U256::MAX ^ (cash_mask() << shift));
    cleared | ((cash & cash_mask()) << shift)
}

/// Apply a `cash` delta (`+amount` on add, `-amount` on sub) to `word`'s field,
/// returning the new word — or `None` on 112-bit overflow / underflow (invalid
/// for real vault balances) so the caller can fall back to a resync.
fn apply_cash_delta(word: U256, high: bool, add: bool, amount: U256) -> Option<U256> {
    let old = cash_field(word, high);
    let new = if add {
        old.checked_add(amount)?
    } else {
        old.checked_sub(amount)?
    };
    if new > cash_mask() {
        return None;
    }
    Some(set_cash_field(word, high, new))
}

/// Probe the warmed vault slots to locate each token's `cash` field by value:
/// match each token's balance (== `cash` for an unmanaged pool) to a discovered
/// slot's low (bits 0–111) or high (bits 112–223) 112-bit field. A token with no
/// **unique** match — a zero balance, a managed balance (`cash != balance`), or a
/// value collision with another field — is skipped, so the reactive `Swap` path
/// resyncs it rather than risk writing the wrong slot. Specialization-agnostic:
/// works for the TWO_TOKEN shared slot and per-token GENERAL/MINIMAL slots alike.
/// For GENERAL/MINIMAL pools, the high 112-bit field is `managed`, not `cash`, so
/// it is only considered when the poolId specialization is TWO_TOKEN. The probe
/// naturally ignores `EnumerableMap` overhead slots (no balance matches them).
fn probe_token_cash(
    tokens: &[Address],
    balances: &[U256],
    verified_slots: &[(Address, U256)],
    vault: Address,
    state: &dyn StateView,
    high_field_can_be_cash: bool,
) -> Vec<BalancerTokenBalance> {
    let mut located = Vec::new();
    for (token, balance) in tokens.iter().zip(balances.iter()) {
        if *balance == U256::ZERO {
            continue; // a zero balance matches any empty field — ambiguous.
        }
        let matches: Vec<(U256, bool)> = verified_slots
            .iter()
            .filter(|(address, _)| *address == vault)
            .filter_map(|(_, slot)| state.storage(vault, *slot).map(|word| (*slot, word)))
            .flat_map(|(slot, word)| {
                let mut found = Vec::new();
                if cash_field(word, false) == *balance {
                    found.push((slot, false));
                }
                if high_field_can_be_cash && cash_field(word, true) == *balance {
                    found.push((slot, true));
                }
                found
            })
            .collect();
        // Only a unique match is trustworthy; otherwise leave the token to resync.
        if let [(slot, high)] = matches.as_slice() {
            located.push(BalancerTokenBalance::new(*token, *slot, *high));
        }
    }
    located
}

/// Locate a token's `cash` field in the metadata's probed map.
fn token_cash_location(
    metadata: &BalancerV2Metadata,
    token: Address,
) -> Option<BalancerTokenBalance> {
    metadata
        .token_cash
        .iter()
        .find(|balance| balance.token == token)
        .copied()
}

#[derive(Clone, Copy)]
struct SwapCashDelta {
    token_in: Address,
    amount_in: U256,
    token_out: Address,
    amount_out: U256,
}

/// The resync repair + quality for the fallback path: re-verify the known
/// `balance_slots`, or a conservative no-op when none are known yet.
fn resync_repair(vault: Address, metadata: &BalancerV2Metadata) -> (RepairAction, UpdateQuality) {
    if metadata.balance_slots.is_empty() {
        (RepairAction::None, UpdateQuality::ConservativeInvalidation)
    } else {
        (
            RepairAction::VerifySlots(
                metadata
                    .balance_slots
                    .iter()
                    .map(|slot| (vault, *slot))
                    .collect(),
            ),
            UpdateQuality::RequiresRepair,
        )
    }
}

/// Event-source a swap: if both tokens' `cash` fields are located and warm,
/// return the exact vault-balance writes (`+amountIn` to `tokenIn`'s cash,
/// `-amountOut` from `tokenOut`'s). `None` on any gap (unknown token, cold slot,
/// or 112-bit overflow) so the caller falls back to a resync.
fn event_source_swap(
    view: &dyn StateView,
    vault: Address,
    pool_id: B256,
    metadata: &BalancerV2Metadata,
    swap: SwapCashDelta,
) -> Option<Vec<StateUpdate>> {
    let in_loc = token_cash_location(metadata, swap.token_in)?;
    let out_loc = token_cash_location(metadata, swap.token_out)?;

    if !is_two_token_pool(pool_id) && (in_loc.high_field || out_loc.high_field) {
        return None;
    }

    if in_loc.slot == out_loc.slot {
        if in_loc.high_field == out_loc.high_field {
            return None;
        }
        // TWO_TOKEN shared slot: both fields live in one word, so apply both
        // deltas to a single write (two separate writes would clobber each other).
        let word = view.storage(vault, in_loc.slot)?;
        let word = apply_cash_delta(word, in_loc.high_field, true, swap.amount_in)?;
        let word = apply_cash_delta(word, out_loc.high_field, false, swap.amount_out)?;
        Some(vec![StateUpdate::slot(vault, in_loc.slot, word)])
    } else {
        let word_in = view.storage(vault, in_loc.slot)?;
        let word_in = apply_cash_delta(word_in, in_loc.high_field, true, swap.amount_in)?;
        let word_out = view.storage(vault, out_loc.slot)?;
        let word_out = apply_cash_delta(word_out, out_loc.high_field, false, swap.amount_out)?;
        Some(vec![
            StateUpdate::slot(vault, in_loc.slot, word_in),
            StateUpdate::slot(vault, out_loc.slot, word_out),
        ])
    }
}

/// Decode a vault `Swap` and **event-source** the two token cash balances
/// directly (no RPC) when possible, falling back to a `balance_slots` resync.
fn decode_swap(pool: &PoolRegistration, log: &Log, view: &dyn StateView) -> AdapterEventResult {
    let decoded = match Swap::decode_log_data_validate(&log.data) {
        Ok(decoded) => decoded,
        Err(_) => {
            return AdapterEventResult::error(AdapterEventError::MalformedLog(
                "malformed Balancer V2 Swap log",
            ));
        }
    };

    let (updates, repair, quality) = match &pool.metadata {
        ProtocolMetadata::BalancerV2(metadata) => match metadata.vault {
            Some(vault) => match pool.key.bytes32().and_then(|pool_id| {
                event_source_swap(
                    view,
                    vault,
                    pool_id,
                    metadata,
                    SwapCashDelta {
                        token_in: decoded.tokenIn,
                        amount_in: decoded.amountIn,
                        token_out: decoded.tokenOut,
                        amount_out: decoded.amountOut,
                    },
                )
            }) {
                Some(updates) => (updates, RepairAction::None, UpdateQuality::Exact),
                None => {
                    let (repair, quality) = resync_repair(vault, metadata);
                    (Vec::new(), repair, quality)
                }
            },
            None => (
                Vec::new(),
                RepairAction::None,
                UpdateQuality::ConservativeInvalidation,
            ),
        },
        _ => (
            Vec::new(),
            RepairAction::None,
            UpdateQuality::ConservativeInvalidation,
        ),
    };

    AdapterEventResult::event(
        AdapterEvent::new(
            pool.key.clone(),
            log.address,
            Swap::SIGNATURE_HASH,
            AdapterEventKind::Swap,
            quality,
        )
        .with_updates(updates)
        .with_repair(repair),
    )
}

/// Decode a vault `PoolBalanceChanged` (join/exit). It changes the vault
/// balances, so v1 resyncs the known read-set (event-sourcing its signed
/// `deltas`, net of `protocolFeeAmounts`, is a follow-up). The `kind` tag is
/// taken from the delta signs.
fn decode_liquidity_change(pool: &PoolRegistration, log: &Log) -> AdapterEventResult {
    let decoded = match PoolBalanceChanged::decode_log_data_validate(&log.data) {
        Ok(decoded) => decoded,
        Err(_) => {
            return AdapterEventResult::error(AdapterEventError::MalformedLog(
                "malformed Balancer V2 PoolBalanceChanged log",
            ));
        }
    };
    let kind = if decoded.deltas.iter().any(|delta| delta.is_positive()) {
        AdapterEventKind::LiquidityAdded
    } else {
        AdapterEventKind::LiquidityRemoved
    };

    let (repair, quality) = match &pool.metadata {
        ProtocolMetadata::BalancerV2(metadata) => match metadata.vault {
            Some(vault) => resync_repair(vault, metadata),
            None => (RepairAction::None, UpdateQuality::ConservativeInvalidation),
        },
        _ => (RepairAction::None, UpdateQuality::ConservativeInvalidation),
    };

    AdapterEventResult::event(
        AdapterEvent::new(
            pool.key.clone(),
            log.address,
            PoolBalanceChanged::SIGNATURE_HASH,
            kind,
            quality,
        )
        .with_repair(repair),
    )
}

/// Adapter for Balancer V2 (shared-vault) pools.
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
            .map(|vault| {
                EventSource::indexed_bytes32(
                    vault,
                    vec![Swap::SIGNATURE_HASH, PoolBalanceChanged::SIGNATURE_HASH],
                    1,
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
        // Resolve the vault: prefer cached metadata, fall back to the first
        // registered state address. Without one there is nothing to discover on.
        let vault = match &pool.metadata {
            ProtocolMetadata::BalancerV2(metadata) => metadata
                .vault
                .or_else(|| pool.state_addresses.first().copied()),
            _ => pool.state_addresses.first().copied(),
        };
        let Some(vault) = vault else {
            return Err(UnsupportedReason::MissingMetadata("Balancer vault"));
        };

        // The poolId is the bytes32-keyed pool identity; it drives `getPoolTokens`.
        let Some(pool_id) = pool.key.bytes32() else {
            return Err(UnsupportedReason::Custom(
                "Balancer V2 pool key is not bytes32-keyed".into(),
            ));
        };

        // A non-empty `balance_slots` means the vault read-set is already known (a
        // prior discovery / trace), so the planner runs a verify-only fast path
        // instead of rediscovering. `tokens` and the probed `token_cash` map are
        // preserved across a verify-only run (there is no `getPoolTokens` decode /
        // probe to repopulate them).
        let (known_slots, tokens, known_token_cash) = match &pool.metadata {
            ProtocolMetadata::BalancerV2(metadata) => (
                metadata.balance_slots.clone(),
                metadata.tokens.clone(),
                metadata.token_cash.clone(),
            ),
            _ => (Vec::new(), Vec::new(), Vec::new()),
        };

        Ok(Box::new(BalancerV2ColdStartPlanner::new(
            vault,
            pool_id,
            known_slots,
            tokens,
            known_token_cash,
            policy,
        )))
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
            decode_swap(pool, log, view)
        } else if topic0 == PoolBalanceChanged::SIGNATURE_HASH {
            decode_liquidity_change(pool, log)
        } else {
            AdapterEventResult::ignored()
        }
    }

    /// Quote via `Vault.queryBatchSwap(GIVEN_IN, [swap], assets, funds)`.
    ///
    /// The vault simulates the swap against the warmed pool balances and returns
    /// the signed asset deltas; the negative delta on the `tokenOut` index is
    /// the (vault-paid-out) output amount, so `amount_out = -delta`. Chain code
    /// does the math — there is no reimplemented stableswap/weighted formula.
    fn simulate_swap(
        &self,
        pool: &PoolRegistration,
        cache: &mut dyn AdapterCache,
        token_in: Address,
        token_out: Address,
        amount_in: U256,
        config: &SimConfig,
    ) -> Result<SwapQuote, SimError> {
        let (vault, pool_id) = match (&pool.metadata, pool.key.bytes32()) {
            (ProtocolMetadata::BalancerV2(metadata), Some(pool_id)) => {
                let vault = metadata
                    .vault
                    .or_else(|| pool.state_addresses.first().copied())
                    .ok_or(SimError::MissingMetadata("Balancer vault"))?;
                (vault, pool_id)
            }
            (ProtocolMetadata::BalancerV2(_), None) => {
                return Err(SimError::MissingMetadata("Balancer poolId"));
            }
            _ => return Err(SimError::MissingMetadata("Balancer metadata")),
        };

        // assets[0] = tokenIn, assets[1] = tokenOut; a single GIVEN_IN step
        // swaps `amount_in` of asset 0 into asset 1 through `pool_id`.
        let calldata = Bytes::from(
            queryBatchSwapCall {
                kind: 0, // GIVEN_IN
                swaps: vec![BatchSwapStep {
                    poolId: pool_id,
                    assetInIndex: U256::ZERO,
                    assetOutIndex: U256::from(1),
                    amount: amount_in,
                    userData: Bytes::new(),
                }],
                assets: vec![token_in, token_out],
                funds: FundManagement {
                    sender: Address::ZERO,
                    fromInternalBalance: false,
                    recipient: Address::ZERO,
                    toInternalBalance: false,
                },
            }
            .abi_encode(),
        );

        let output = quote_via_call_from(cache, config.from, vault, calldata)?;
        let asset_deltas = queryBatchSwapCall::abi_decode_returns_validate(&output)
            .map_err(|_| SimError::MalformedOutput("queryBatchSwap return"))?;

        // assetDeltas[1] is the tokenOut delta: negative = paid out by the vault.
        let delta_out = asset_deltas
            .get(1)
            .copied()
            .ok_or(SimError::MalformedOutput("missing tokenOut delta"))?;
        if delta_out.is_positive() {
            return Err(SimError::MalformedOutput(
                "tokenOut delta is non-negative (no output)",
            ));
        }
        let amount_out = U256::from(delta_out.unsigned_abs());

        Ok(SwapQuote::new(amount_out))
    }
}

/// The phase a [`BalancerV2ColdStartPlanner`] is in between rounds.
enum BalancerPhase {
    /// Round 1 ran the `getPoolTokens` discover call; classify its result next.
    Discover,
    /// Round 2 verified the discovered balance slots; the next `on_results` is done.
    Verify,
}

/// Why a Balancer cold start could not reach `Ready`.
enum BalancerRepair {
    /// The discover call reverted, halted, or returned undecodable data.
    DiscoverFailed,
    /// The discover call decoded but touched no slots under `restrict_to`.
    NoSlotsDiscovered,
    /// A discovered vault balance slot could not be fetched in the verify round
    /// (an archive miss), so the warmed balances are not authoritative.
    BalancesUnfetched,
}

/// Cold-start planner for a Balancer V2 pool, in one of two modes:
///
/// - **Discover → verify** (the balance read-set is unknown). Balancer pool state
///   lives in the vault behind a non-predictable storage layout, so the planner
///   cannot name the slots up front: round 1 runs a `getPoolTokens(poolId)`
///   view-call on the vault (`restrict_to = [vault]`), capturing the
///   `(vault, slot)` pairs it SLOADs and decoding the token list from the return
///   data; round 2 authoritatively verifies exactly those slots so the live
///   balances are warmed.
/// - **Verify-only** (the read-set is already known — `BalancerV2Metadata`
///   `balance_slots` pre-populated from a prior discovery or a trace). The planner
///   skips the `getPoolTokens` discovery — no vault-account fetch and no
///   cold-cache faulting — and warms exactly the known slots in a **single verify
///   round**, matching the bundled `cold_start_many` storage-program path. The
///   config-supplied `tokens` are preserved (there is no decode to repopulate).
///
/// The planner stays policy-aware in shape (the policy is threaded into the
/// report) so later slices can refine `HotSlotsOnly`/`Lazy`.
struct BalancerV2ColdStartPlanner {
    vault: Address,
    pool_id: B256,
    policy: ColdStartPolicy,
    phase: BalancerPhase,
    /// Tokens decoded from `getPoolTokens` (discover mode) or carried from the
    /// config-supplied metadata (verify-only mode).
    tokens: Vec<Address>,
    /// Per-token balances decoded from `getPoolTokens` (discover mode only), used
    /// by the Verify-phase probe to locate each token's cash field. Empty in
    /// verify-only mode (no `getPoolTokens` call).
    balances: Vec<U256>,
    /// Per-token cash-field locations: rebuilt by the discover-phase probe, or
    /// carried from config-supplied metadata in verify-only mode.
    token_cash: Vec<BalancerTokenBalance>,
    /// The vault balance slots discovered in round 1 and verified in round 2.
    verified_slots: Vec<(Address, U256)>,
    /// Slots injected across the run (the refreshed balances).
    changed_slots: Vec<SlotChange>,
    /// Set when the run cannot reach `Ready` (discover failure / empty capture).
    repair: Option<BalancerRepair>,
}

impl BalancerV2ColdStartPlanner {
    fn new(
        vault: Address,
        pool_id: B256,
        known_slots: Vec<U256>,
        tokens: Vec<Address>,
        known_token_cash: Vec<BalancerTokenBalance>,
        policy: ColdStartPolicy,
    ) -> Self {
        // A pre-populated balance read-set selects the verify-only fast path:
        // start in `Verify` with those slots, so `initial_plan` emits one verify
        // round (no getPoolTokens discover) and `finish` persists them. Sort +
        // dedup for a stable, minimal fetch set. An empty read-set keeps the
        // discover->verify default (byte-for-byte unchanged from before).
        let mut slots = known_slots;
        slots.sort_unstable();
        slots.dedup();
        let (phase, verified_slots) = if slots.is_empty() {
            (BalancerPhase::Discover, Vec::new())
        } else {
            (
                BalancerPhase::Verify,
                slots.into_iter().map(|slot| (vault, slot)).collect(),
            )
        };
        Self {
            vault,
            pool_id,
            policy,
            phase,
            // Discover mode overwrites this from the getPoolTokens decode;
            // verify-only mode preserves the config-supplied tokens (they came
            // from the prior discovery that produced the known read-set).
            tokens,
            // Discover mode fills these from the getPoolTokens decode + probe;
            // verify-only mode has no decode, so `balances` stays empty (probe
            // skipped) and the carried `token_cash` is preserved.
            balances: Vec::new(),
            token_cash: known_token_cash,
            verified_slots,
            changed_slots: Vec::new(),
            repair: None,
        }
    }
}

impl AdapterColdStartPlanner for BalancerV2ColdStartPlanner {
    fn initial_plan(&mut self, _state: &dyn StateView) -> ColdStartPlan {
        // Verify-only fast path: the balance read-set is already known
        // (pre-populated in `new`), so skip the `getPoolTokens` discovery — and
        // the vault-account fetch + cold-cache faulting it needs — and warm
        // exactly those slots in a single verify round. `on_results` lands
        // directly in its `Verify` branch. A stale/incomplete set is safe: the
        // first `simulate_swap` lazily faults anything missing.
        if matches!(self.phase, BalancerPhase::Verify) {
            return ColdStartPlan {
                verify: self.verified_slots.clone(),
                ..Default::default()
            };
        }

        // Round 1: ensure the vault's code, then run `getPoolTokens` and capture
        // the vault slots it touches (restricted to the vault so only its balance
        // storage is collected).
        ColdStartPlan {
            accounts: vec![self.vault],
            discover: vec![ColdStartCall {
                from: Address::ZERO,
                to: self.vault,
                calldata: Bytes::from(
                    getPoolTokensCall {
                        poolId: self.pool_id,
                    }
                    .abi_encode(),
                ),
                restrict_to: Some(vec![self.vault]),
            }],
            ..Default::default()
        }
    }

    fn on_results(&mut self, results: &ColdStartResults, state: &dyn StateView) -> ColdStartStep {
        // Record any slots injected this round (round 2's refreshed balances).
        self.changed_slots.extend(results.verified.iter().cloned());

        match self.phase {
            BalancerPhase::Discover => {
                let Some(call) = results.discovered.first() else {
                    // No discover result at all — treat as a failed discovery.
                    self.repair = Some(BalancerRepair::DiscoverFailed);
                    return ColdStartStep::Done;
                };

                // Classify off the load-bearing success signal first (mirroring
                // the V2/V3 planners) rather than relying on the decoder to
                // reject a revert/halt payload.
                if !call.result.is_success() {
                    self.repair = Some(BalancerRepair::DiscoverFailed);
                    return ColdStartStep::Done;
                }
                // Decode the token list from the call's return data. Undecodable
                // data is a degraded/unsupported pool, not a panic. Use the
                // validating decoder so a malformed payload is rejected, not
                // best-effort reinterpreted.
                let Some(output) = call.result.output() else {
                    self.repair = Some(BalancerRepair::DiscoverFailed);
                    return ColdStartStep::Done;
                };
                match getPoolTokensCall::abi_decode_returns_validate(output) {
                    Ok(decoded) => {
                        self.tokens = decoded.tokens;
                        // Keep the balances to probe each token's cash slot/offset
                        // once the verify round warms the discovered slots.
                        self.balances = decoded.balances;
                    }
                    Err(_) => {
                        self.repair = Some(BalancerRepair::DiscoverFailed);
                        return ColdStartStep::Done;
                    }
                }

                // Collect the discovered vault slots (already restricted to the
                // vault). The access list is a set, so order is unspecified.
                let discovered: Vec<(Address, U256)> = call
                    .access
                    .slots
                    .iter()
                    .filter(|(address, _)| *address == self.vault)
                    .copied()
                    .collect();

                // Empty capture is a distinguishable signal: a verify round over
                // zero slots would be a no-op, so record a repair and finish rather
                // than continue.
                if discovered.is_empty() {
                    self.repair = Some(BalancerRepair::NoSlotsDiscovered);
                    return ColdStartStep::Done;
                }

                self.verified_slots = discovered.clone();
                self.phase = BalancerPhase::Verify;
                ColdStartStep::Continue(ColdStartPlan {
                    verify: discovered,
                    ..Default::default()
                })
            }
            BalancerPhase::Verify => {
                // The discovered vault slots are the hot state. Source their
                // verdict from the per-slot `SlotFetch` classification (like the
                // V2/V3 planners) so an archive miss is not silently accepted as
                // a warmed `Ready`. A genuine `Zero` is legitimate (a fresh pool
                // can hold a zero balance), so only an unfetchable / never-
                // attempted slot forces a repair.
                let any_unfetched = self.verified_slots.iter().any(|(address, slot)| {
                    matches!(
                        results
                            .fetched
                            .iter()
                            .find(|o| o.address == *address && o.slot == *slot)
                            .map(|o| &o.fetch),
                        Some(SlotFetch::FetchFailed { .. }) | Some(SlotFetch::NotAttempted) | None
                    )
                });
                if any_unfetched {
                    self.repair = Some(BalancerRepair::BalancesUnfetched);
                }
                // Discover-origin verify: the slots are now warm, so probe them to
                // locate each token's `cash` field by value — this builds the map
                // the reactive `Swap` path event-sources from. Verify-only mode has
                // no fresh `balances`, so the carried `token_cash` is left intact.
                if !self.balances.is_empty() {
                    self.token_cash = probe_token_cash(
                        &self.tokens,
                        &self.balances,
                        &self.verified_slots,
                        self.vault,
                        state,
                        is_two_token_pool(self.pool_id),
                    );
                }
                ColdStartStep::Done
            }
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

        match self.repair {
            Some(BalancerRepair::DiscoverFailed) => {
                report.status = PoolStatus::Degraded;
                // Re-running discovery from scratch is the repair for a failed or
                // undecodable `getPoolTokens` call.
                ColdStartOutcome::NeedsRepair(
                    report,
                    RepairAction::ColdStart {
                        pool: pool.key.clone(),
                        policy: self.policy,
                    },
                )
            }
            Some(BalancerRepair::NoSlotsDiscovered) => {
                report.status = PoolStatus::Degraded;
                // The vault is a shared singleton, so a wholesale
                // PurgeStorage(vault) would wipe every co-tenant Balancer pool's
                // warmed state. Nothing pool-specific was discovered to scope a
                // purge to, so re-run discovery instead (as DiscoverFailed does).
                ColdStartOutcome::NeedsRepair(
                    report,
                    RepairAction::ColdStart {
                        pool: pool.key.clone(),
                        policy: self.policy,
                    },
                )
            }
            Some(BalancerRepair::BalancesUnfetched) => {
                report.status = PoolStatus::Degraded;
                // Archive-miss repair: re-verify exactly the discovered slots
                // (mirrors the V2/V3 archive-miss repair).
                ColdStartOutcome::NeedsRepair(
                    report,
                    RepairAction::VerifySlots(self.verified_slots.clone()),
                )
            }
            None => {
                // The pool address is the leading 20 bytes of the poolId, matching
                // Balancer's poolId encoding (`address(20) | nonce/kind`).
                let pool_address = Address::from_slice(&self.pool_id.as_slice()[..20]);
                // Persist the discovered balance slots (slot-only; all on the
                // vault) so the reactive `Swap` path can refresh exactly them.
                // The discovered set is order-unspecified; sort for a stable,
                // deduped record.
                let mut balance_slots: Vec<U256> =
                    self.verified_slots.iter().map(|(_, slot)| *slot).collect();
                balance_slots.sort_unstable();
                balance_slots.dedup();
                pool.metadata = ProtocolMetadata::BalancerV2(BalancerV2Metadata {
                    vault: Some(self.vault),
                    pool_address: Some(pool_address),
                    tokens: self.tokens.clone(),
                    balance_slots,
                    // The probed per-token cash locations (discover mode) or the
                    // carried map (verify-only mode) — drives reactive `Swap`
                    // event-sourcing.
                    token_cash: self.token_cash.clone(),
                });
                pool.status = PoolStatus::Ready;
                report.status = PoolStatus::Ready;
                ColdStartOutcome::Ready(report)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// Minimal `StateView` over an in-memory `(address, slot) -> value` map.
    struct MapView(HashMap<(Address, U256), U256>);
    impl StateView for MapView {
        fn storage(&self, address: Address, slot: U256) -> Option<U256> {
            self.0.get(&(address, slot)).copied()
        }
    }

    fn addr(byte: u8) -> Address {
        Address::repeat_byte(byte)
    }

    fn pool_id(specialization: u16) -> B256 {
        let mut bytes = [0x11; 32];
        bytes[20..22].copy_from_slice(&specialization.to_be_bytes());
        B256::from(bytes)
    }

    fn swap_delta(
        token_in: Address,
        amount_in: u64,
        token_out: Address,
        amount_out: u64,
    ) -> SwapCashDelta {
        SwapCashDelta {
            token_in,
            amount_in: U256::from(amount_in),
            token_out,
            amount_out: U256::from(amount_out),
        }
    }

    /// A single-token `BalanceAllocation`: `[block:32][managed:112][cash:112]`.
    fn packed(block: u64, managed: u128, cash: u128) -> U256 {
        (U256::from(block) << 224) | (U256::from(managed) << 112) | U256::from(cash)
    }

    /// A TWO_TOKEN shared cash slot: `[block:32][cash_high:112][cash_low:112]`.
    fn packed_two(block: u64, cash_high: u128, cash_low: u128) -> U256 {
        (U256::from(block) << 224) | (U256::from(cash_high) << 112) | U256::from(cash_low)
    }

    #[test]
    fn cash_field_round_trips_both_offsets_preserving_others() {
        let word = packed(0x1234, 0xAAAA, 0xBBBB);
        assert_eq!(cash_field(word, false), U256::from(0xBBBB_u64));
        assert_eq!(cash_field(word, true), U256::from(0xAAAA_u64));

        let low = set_cash_field(word, false, U256::from(0xCCCC_u64));
        assert_eq!(cash_field(low, false), U256::from(0xCCCC_u64));
        assert_eq!(cash_field(low, true), U256::from(0xAAAA_u64));
        assert_eq!(low >> 224, U256::from(0x1234_u64));

        let high = set_cash_field(word, true, U256::from(0xDDDD_u64));
        assert_eq!(cash_field(high, true), U256::from(0xDDDD_u64));
        assert_eq!(cash_field(high, false), U256::from(0xBBBB_u64));
        assert_eq!(high >> 224, U256::from(0x1234_u64));
    }

    #[test]
    fn apply_cash_delta_adds_subtracts_and_bounds() {
        let word = packed(9, 0, 100);
        assert_eq!(
            cash_field(
                apply_cash_delta(word, false, true, U256::from(5_u64)).unwrap(),
                false
            ),
            U256::from(105_u64)
        );
        assert_eq!(
            cash_field(
                apply_cash_delta(word, false, false, U256::from(40_u64)).unwrap(),
                false
            ),
            U256::from(60_u64)
        );
        // Underflow (burn more than cash) and 112-bit overflow both reject.
        assert!(apply_cash_delta(word, false, false, U256::from(101_u64)).is_none());
        let full = set_cash_field(U256::ZERO, false, cash_mask());
        assert!(apply_cash_delta(full, false, true, U256::from(1_u64)).is_none());
    }

    #[test]
    fn probe_locates_two_token_shared_slot() {
        let vault = addr(0xba);
        let (t0, t1) = (addr(0x01), addr(0x02));
        let slot = U256::from(0x77_u64);
        let mut m = HashMap::new();
        m.insert((vault, slot), packed_two(5, 222, 111)); // low=t0=111, high=t1=222
        let view = MapView(m);

        let cash = probe_token_cash(
            &[t0, t1],
            &[U256::from(111_u64), U256::from(222_u64)],
            &[(vault, slot)],
            vault,
            &view,
            true,
        );
        assert_eq!(cash.len(), 2);
        assert!(cash.contains(&BalancerTokenBalance::new(t0, slot, false)));
        assert!(cash.contains(&BalancerTokenBalance::new(t1, slot, true)));
    }

    #[test]
    fn probe_locates_per_token_slots_and_skips_ambiguous_and_zero() {
        let vault = addr(0xba);
        let (t0, t1, t2, t3) = (addr(0x01), addr(0x02), addr(0x03), addr(0x04));
        let (s0, s1, s2, s3) = (
            U256::from(1_u64),
            U256::from(2_u64),
            U256::from(3_u64),
            U256::from(4_u64),
        );
        let mut m = HashMap::new();
        m.insert((vault, s0), packed(9, 0, 1000)); // t0
        m.insert((vault, s1), packed(9, 0, 2000)); // t1 (unique)
        m.insert((vault, s2), packed(9, 0, 1000)); // t2 collides with t0
        m.insert((vault, s3), packed(9, 0, 0)); // t3 empty
        let view = MapView(m);

        let cash = probe_token_cash(
            &[t0, t1, t2, t3],
            &[
                U256::from(1000_u64),
                U256::from(2000_u64),
                U256::from(1000_u64),
                U256::ZERO,
            ],
            &[(vault, s0), (vault, s1), (vault, s2), (vault, s3)],
            vault,
            &view,
            false,
        );
        // Only t1 is unambiguous: t0/t2 share value 1000 (2 matches each), t3 is zero.
        assert_eq!(cash, vec![BalancerTokenBalance::new(t1, s1, false)]);
    }

    #[test]
    fn probe_skips_high_managed_field_for_non_two_token_pools() {
        let vault = addr(0xba);
        let token = addr(0x01);
        let slot = U256::from(0x77_u64);
        let mut m = HashMap::new();
        m.insert((vault, slot), packed(9, 777, 111)); // high=managed, low=cash
        let view = MapView(m);

        let cash = probe_token_cash(
            &[token],
            &[U256::from(777_u64)],
            &[(vault, slot)],
            vault,
            &view,
            false,
        );
        assert!(cash.is_empty());
    }

    #[test]
    fn event_source_swap_shared_slot_accumulates_both_fields() {
        let vault = addr(0xba);
        let (t_in, t_out) = (addr(0x01), addr(0x02));
        let slot = U256::from(0x77_u64);
        let meta = BalancerV2Metadata::default()
            .with_vault(vault)
            .with_token_cash([
                BalancerTokenBalance::new(t_in, slot, false),
                BalancerTokenBalance::new(t_out, slot, true),
            ]);
        let mut m = HashMap::new();
        m.insert((vault, slot), packed_two(5, 1000, 500)); // high(t_out)=1000, low(t_in)=500
        let view = MapView(m);

        let updates = event_source_swap(
            &view,
            vault,
            pool_id(TWO_TOKEN_SPECIALIZATION),
            &meta,
            swap_delta(t_in, 30, t_out, 20),
        )
        .unwrap();
        assert_eq!(updates.len(), 1, "shared slot -> one combined write");
        let StateUpdate::Slot { value, .. } = &updates[0] else {
            panic!("expected a Slot write");
        };
        assert_eq!(cash_field(*value, false), U256::from(530_u64)); // t_in + 30
        assert_eq!(cash_field(*value, true), U256::from(980_u64)); // t_out - 20
        assert_eq!(*value >> 224, U256::from(5_u64), "block preserved");
    }

    #[test]
    fn event_source_swap_separate_slots() {
        let vault = addr(0xba);
        let (t_in, t_out) = (addr(0x01), addr(0x02));
        let (s_in, s_out) = (U256::from(1_u64), U256::from(2_u64));
        let meta = BalancerV2Metadata::default()
            .with_vault(vault)
            .with_token_cash([
                BalancerTokenBalance::new(t_in, s_in, false),
                BalancerTokenBalance::new(t_out, s_out, false),
            ]);
        let mut m = HashMap::new();
        m.insert((vault, s_in), packed(9, 0, 100));
        m.insert((vault, s_out), packed(9, 0, 200));
        let view = MapView(m);

        let updates = event_source_swap(
            &view,
            vault,
            pool_id(0),
            &meta,
            swap_delta(t_in, 10, t_out, 20),
        )
        .unwrap();
        assert_eq!(updates.len(), 2);
        for update in updates {
            let StateUpdate::Slot { slot, value, .. } = update else {
                panic!("expected Slot writes");
            };
            if slot == s_in {
                assert_eq!(cash_field(value, false), U256::from(110_u64)); // +10
            } else {
                assert_eq!(cash_field(value, false), U256::from(180_u64)); // -20
            }
        }
    }

    #[test]
    fn event_source_swap_unknown_token_or_cold_slot_falls_back() {
        let vault = addr(0xba);
        let (t_in, t_out) = (addr(0x01), addr(0x02));
        let slot = U256::from(0x77_u64);
        // Unknown token: empty token_cash -> None.
        let bare = BalancerV2Metadata::default().with_vault(vault);
        let view = MapView(HashMap::new());
        assert!(
            event_source_swap(
                &view,
                vault,
                pool_id(TWO_TOKEN_SPECIALIZATION),
                &bare,
                swap_delta(t_in, 1, t_out, 1)
            )
            .is_none()
        );
        // Known token but cold slot (not in the view) -> None.
        let mapped = BalancerV2Metadata::default()
            .with_vault(vault)
            .with_token_cash([
                BalancerTokenBalance::new(t_in, slot, false),
                BalancerTokenBalance::new(t_out, slot, true),
            ]);
        assert!(
            event_source_swap(
                &view,
                vault,
                pool_id(TWO_TOKEN_SPECIALIZATION),
                &mapped,
                swap_delta(t_in, 1, t_out, 1)
            )
            .is_none()
        );
    }

    #[test]
    fn event_source_swap_rejects_invalid_token_cash_metadata() {
        let vault = addr(0xba);
        let (t_in, t_out) = (addr(0x01), addr(0x02));
        let slot = U256::from(0x77_u64);
        let mut m = HashMap::new();
        m.insert((vault, slot), packed_two(5, 1000, 500));
        let view = MapView(m);

        let duplicate_field = BalancerV2Metadata::default()
            .with_vault(vault)
            .with_token_cash([
                BalancerTokenBalance::new(t_in, slot, false),
                BalancerTokenBalance::new(t_out, slot, false),
            ]);
        assert!(
            event_source_swap(
                &view,
                vault,
                pool_id(TWO_TOKEN_SPECIALIZATION),
                &duplicate_field,
                swap_delta(t_in, 30, t_out, 20),
            )
            .is_none()
        );

        let high_field_on_general = BalancerV2Metadata::default()
            .with_vault(vault)
            .with_token_cash([
                BalancerTokenBalance::new(t_in, slot, false),
                BalancerTokenBalance::new(t_out, slot, true),
            ]);
        assert!(
            event_source_swap(
                &view,
                vault,
                pool_id(0),
                &high_field_on_general,
                swap_delta(t_in, 30, t_out, 20),
            )
            .is_none()
        );
    }
}
