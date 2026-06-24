use alloy_primitives::{Address, B256, Bytes, Log, U256};
use alloy_sol_types::{SolCall, SolEvent, sol};
use evm_fork_cache::cold_start::{
    ColdStartCall, ColdStartPlan, ColdStartResults, ColdStartRunReport, ColdStartStep, SlotFetch,
};

use super::cold_start::AdapterColdStartPlanner;
use super::sim::{
    BatchSwapStep, FundManagement, SimConfig, SimError, SwapQuote, queryBatchSwapCall, run_quote,
};
use super::{
    AdapterCache, AdapterEvent, AdapterEventError, AdapterEventKind, AdapterEventResult,
    AmmAdapter, BalancerV2Metadata, ColdStartOutcome, ColdStartPolicy, ColdStartReport,
    EventSource, PoolRegistration, PoolStatus, ProtocolId, ProtocolMetadata, RepairAction,
    SlotChange, StateView, UnsupportedReason, UpdateQuality,
};

sol! {
    event Swap(bytes32 indexed poolId, address indexed tokenIn, address indexed tokenOut, uint256 amountIn, uint256 amountOut);
}

sol! {
    /// Local Balancer V2 vault ABI for cold-start discovery. Kept here rather than
    /// reusing the simulation-gated `cache_sync` copy so the planner compiles under
    /// the `balancer-v2` adapter feature alone.
    function getPoolTokens(bytes32 poolId)
        returns (address[] tokens, uint256[] balances, uint256 lastChangeBlock);
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

        Ok(Box::new(BalancerV2ColdStartPlanner::new(
            vault, pool_id, policy,
        )))
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

        // The vault balances live behind a non-predictable storage mapping, so
        // the Swap event payload cannot be turned into an exact masked write.
        // Instead we keep the cached balances fresh by re-verifying the exact
        // `(vault, slot)` pairs the cold-start `getPoolTokens` discovery found:
        // a `VerifySlots` repair the reactive runtime lowers into a hash-pinned
        // resync, re-reading the post-swap balances authoritatively. This stays
        // consistent with the discover-based cold start and avoids lossy
        // event-delta arithmetic. The discovered slots are persisted on
        // `BalancerV2Metadata.balance_slots` by the cold-start `finish`.
        let repair = match &pool.metadata {
            ProtocolMetadata::BalancerV2(metadata) => {
                match (metadata.vault, metadata.balance_slots.as_slice()) {
                    (Some(vault), slots) if !slots.is_empty() => {
                        RepairAction::VerifySlots(slots.iter().map(|slot| (vault, *slot)).collect())
                    }
                    // Vault known but no discovered slots yet (cold-start has not
                    // run / found them): fall back to the conservative no-op so
                    // the routing/observability behavior is preserved.
                    _ => RepairAction::None,
                }
            }
            _ => RepairAction::None,
        };

        AdapterEventResult::event(AdapterEvent {
            pool: pool.key.clone(),
            emitter: log.address,
            topic0: Swap::SIGNATURE_HASH,
            kind: AdapterEventKind::Swap,
            updates: Vec::new(),
            quality: UpdateQuality::ConservativeInvalidation,
            repair,
        })
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
        _config: &SimConfig,
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

        let output = run_quote(cache, vault, calldata)?;
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

/// Cold-start planner for a Balancer V2 pool: a discover → verify access-list run.
///
/// Balancer pool state lives in the vault behind a non-predictable storage layout,
/// so the planner cannot name the balance slots up front. Instead round 1 runs a
/// `getPoolTokens(poolId)` view-call on the vault (`restrict_to = [vault]`) and
/// captures the `(vault, slot)` pairs it SLOADs. Round 2 authoritatively verifies
/// exactly those discovered slots so the live balances are warmed. The token list
/// is decoded from the discover call's return data.
///
/// The flow runs for every policy: the vault balances are the hot state, so there
/// is no verify-only shortcut. The planner stays policy-aware in shape (the policy
/// is threaded into the report) so later slices can refine `HotSlotsOnly`/`Lazy`.
struct BalancerV2ColdStartPlanner {
    vault: Address,
    pool_id: B256,
    policy: ColdStartPolicy,
    phase: BalancerPhase,
    /// Tokens decoded from the `getPoolTokens` return data (round 1).
    tokens: Vec<Address>,
    /// The vault balance slots discovered in round 1 and verified in round 2.
    verified_slots: Vec<(Address, U256)>,
    /// Slots injected across the run (the refreshed balances).
    changed_slots: Vec<SlotChange>,
    /// Set when the run cannot reach `Ready` (discover failure / empty capture).
    repair: Option<BalancerRepair>,
}

impl BalancerV2ColdStartPlanner {
    fn new(vault: Address, pool_id: B256, policy: ColdStartPolicy) -> Self {
        Self {
            vault,
            pool_id,
            policy,
            phase: BalancerPhase::Discover,
            tokens: Vec::new(),
            verified_slots: Vec::new(),
            changed_slots: Vec::new(),
            repair: None,
        }
    }
}

impl AdapterColdStartPlanner for BalancerV2ColdStartPlanner {
    fn initial_plan(&mut self, _state: &dyn StateView) -> ColdStartPlan {
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

    fn on_results(&mut self, results: &ColdStartResults, _state: &dyn StateView) -> ColdStartStep {
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
                // the V2/V3 planners and cache_sync::call_view) rather than
                // relying on the decoder to reject a revert/halt payload.
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
                    Ok(decoded) => self.tokens = decoded.tokens,
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
                });
                pool.status = PoolStatus::Ready;
                report.status = PoolStatus::Ready;
                ColdStartOutcome::Ready(report)
            }
        }
    }
}
