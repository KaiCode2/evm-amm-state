//! Curve StableSwap (plain pool) adapter — slice 1.
//!
//! See `docs/curve-stableswap-adapter-spec.md`. The closest analog is
//! [`BalancerV2Adapter`](super::balancer_v2::BalancerV2Adapter): a
//! discover→verify cold-start (Curve has no predictable balance-slot layout, so
//! the planner captures the SLOAD set of a `get_dy` call rather than naming
//! slots up front) and a resync-on-event reactive path (`TokenExchange` /
//! liquidity events carry deltas, not absolute balances, so re-verify the
//! discovered slots instead of doing lossy delta arithmetic). Swap simulation
//! calls the pool's own `get_dy(i, j, dx)` — no reimplemented StableSwap math.
//!
//! Scope (slice 1): classic StableSwap **plain pools** with a self-contained
//! `get_dy(int128 i, int128 j, uint256 dx)`. Deferred (documented non-goals):
//! CryptoSwap (Curve v2) / StableSwap-NG (which use `uint256` indices), and
//! metapools / lending pools whose `get_dy` makes external calls (the
//! `restrict_to=[pool]` discover capture would be incomplete for them).

use alloy_primitives::{Address, B256, Bytes, Log, U256, keccak256};
use alloy_sol_types::{SolCall, SolEvent, sol};
use evm_fork_cache::cold_start::{
    ColdStartCall, ColdStartPlan, ColdStartResults, ColdStartRunReport, ColdStartStep, SlotFetch,
};

use super::cold_start::AdapterColdStartPlanner;
use super::sim::{SimConfig, SimError, SwapQuote, get_dyCall, run_quote};
use super::{
    AdapterCache, AdapterEvent, AdapterEventError, AdapterEventKind, AdapterEventResult,
    AmmAdapter, ColdStartOutcome, ColdStartPolicy, ColdStartReport, CurveMetadata, EventSource,
    PoolRegistration, PoolStatus, ProtocolId, ProtocolMetadata, RepairAction, SlotChange,
    StateView, UnsupportedReason, UpdateQuality,
};

sol! {
    // Classic Curve StableSwap plain-pool events. Only the signature hashes are
    // used for topic routing; the liquidity-event payloads are not decoded (the
    // reactive path resyncs the discovered slots rather than applying deltas).
    event TokenExchange(address indexed buyer, int128 sold_id, uint256 tokens_sold, int128 bought_id, uint256 tokens_bought);
    event AddLiquidity(address indexed provider, uint256[3] token_amounts, uint256[3] fees, uint256 invariant, uint256 token_supply);
    event RemoveLiquidity(address indexed provider, uint256[3] token_amounts, uint256[3] fees, uint256 token_supply);
    event RemoveLiquidityOne(address indexed provider, uint256 token_amount, uint256 coin_amount);
    event RemoveLiquidityImbalance(address indexed provider, uint256[3] token_amounts, uint256[3] fees, uint256 invariant, uint256 token_supply);
}

/// The `dx` used by the cold-start discover call.
///
/// Its magnitude is irrelevant: `get_dy` SLOADs the full balance set +
/// amplification + fee unconditionally, so any non-reverting `dx` captures the
/// same read-set. A small fixed nonzero value keeps the discover call cheap and
/// avoids the `dx == 0` degenerate path some StableSwap builds short-circuit.
const DISCOVER_DX: U256 = U256::from_limbs([1_000_000, 0, 0, 0]);

/// Adapter for Curve StableSwap plain pools (slice 1).
#[derive(Clone, Debug, Default)]
pub struct CurveAdapter {
    _private: (),
}

/// The coin count for a registration, or 0 if not Curve metadata / unconfigured.
fn pool_n_coins(pool: &PoolRegistration) -> usize {
    match &pool.metadata {
        ProtocolMetadata::Curve(metadata) => metadata.coins.len(),
        _ => 0,
    }
}

/// The `AddLiquidity` topic hash for an `n_coins`-coin pool.
///
/// The `uint256[N]` array arity IS part of the canonical event signature, so the
/// topic hash is pool-specific. Derived from `n_coins` (not a fixed N) so routing
/// is correct for every plain-pool arity. A `#[cfg(test)]` check asserts the N=3
/// derivation equals the `sol!`-macro `SIGNATURE_HASH` (i.e. the format string is
/// right) and that arities differ.
fn add_liquidity_topic(n_coins: usize) -> B256 {
    keccak256(
        format!("AddLiquidity(address,uint256[{n_coins}],uint256[{n_coins}],uint256,uint256)")
            .as_bytes(),
    )
}

/// Topic hashes this adapter routes for an `n_coins`-coin pool.
///
/// `TokenExchange(address,int128,uint256,int128,uint256)` and
/// `RemoveLiquidityOne(address,uint256,uint256)` carry no array params, so their
/// signature hashes are arity-independent and route for any pool (`TokenExchange`
/// is the swap event, so swap-driven resync is always covered). The other three
/// carry `uint256[N]` arrays, so their topic hashes are derived from `n_coins`.
/// `n_coins == 0` (unconfigured) routes only the arity-independent topics.
fn curve_event_topics(n_coins: usize) -> Vec<B256> {
    let mut topics = vec![
        TokenExchange::SIGNATURE_HASH,
        RemoveLiquidityOne::SIGNATURE_HASH,
    ];
    if n_coins >= 1 {
        topics.push(add_liquidity_topic(n_coins));
        topics.push(keccak256(
            format!("RemoveLiquidity(address,uint256[{n_coins}],uint256[{n_coins}],uint256)")
                .as_bytes(),
        ));
        topics.push(keccak256(
            format!(
                "RemoveLiquidityImbalance(address,uint256[{n_coins}],uint256[{n_coins}],uint256,uint256)"
            )
            .as_bytes(),
        ));
    }
    topics
}

impl AmmAdapter for CurveAdapter {
    fn protocol(&self) -> ProtocolId {
        ProtocolId::Curve
    }

    fn event_sources(&self, pool: &PoolRegistration) -> Vec<EventSource> {
        let n_coins = pool_n_coins(pool);
        pool.key
            .address()
            .map(|address| EventSource::direct(address, curve_event_topics(n_coins)))
            .into_iter()
            .collect()
    }

    fn cold_start_planner(
        &self,
        pool: &PoolRegistration,
        policy: ColdStartPolicy,
    ) -> Result<Box<dyn AdapterColdStartPlanner>, UnsupportedReason> {
        // The pool is its own state + event source; without an address there is
        // nothing to discover on. `coins` may be empty here — it is config-
        // supplied static identity, only required at simulate time — so this is
        // the only precondition (no MissingMetadata layout path, unlike Solidly:
        // discovery handles the layout).
        let Some(address) = pool.key.address() else {
            return Err(UnsupportedReason::Custom(
                "Curve pool key is not address-keyed".into(),
            ));
        };

        // Preserve the config-supplied coins across cold-start so `finish` can
        // re-emit them alongside the discovered slots.
        let coins = match &pool.metadata {
            ProtocolMetadata::Curve(metadata) => metadata.coins.clone(),
            _ => Vec::new(),
        };

        Ok(Box::new(CurveColdStartPlanner::new(address, coins, policy)))
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
        // Route against the pool's ARITY-SPECIFIC topic set — the liquidity-event
        // hashes depend on n_coins (the `uint256[N]` arity is part of the event
        // signature), so a fixed-arity set would silently drop 2-/4-coin pools'
        // liquidity events.
        let n_coins = pool_n_coins(pool);
        if !curve_event_topics(n_coins).contains(&topic0) {
            return AdapterEventResult::ignored();
        }

        // `TokenExchange` is the swap event; validate it decodes (a malformed log
        // is a hard decode error, matching Balancer). The liquidity events route
        // on topic only — their payloads are never decoded, since the reactive
        // path resyncs the discovered slots rather than applying their deltas.
        let kind = if topic0 == TokenExchange::SIGNATURE_HASH {
            if TokenExchange::decode_log_data_validate(&log.data).is_err() {
                return AdapterEventResult::error(AdapterEventError::MalformedLog(
                    "malformed Curve TokenExchange log",
                ));
            }
            AdapterEventKind::Swap
        } else if topic0 == add_liquidity_topic(n_coins) {
            AdapterEventKind::LiquidityAdded
        } else {
            // RemoveLiquidity / RemoveLiquidityOne / RemoveLiquidityImbalance.
            AdapterEventKind::LiquidityRemoved
        };

        // A Curve event delta is not an exact absolute balance (`get_dy`'s
        // read-set spans balances + A + fee, all behind a non-predictable Vyper
        // layout), so the reactive path re-verifies exactly the cold-start
        // discovered slots: a `VerifySlots` repair the runtime lowers into a
        // hash-pinned resync that re-reads the post-event state authoritatively.
        // This mirrors Balancer's `Swap` decode and avoids lossy delta math. The
        // discovered slots are persisted on `CurveMetadata.discovered_slots` by
        // the cold-start `finish`.
        let repair = match &pool.metadata {
            ProtocolMetadata::Curve(metadata) if !metadata.discovered_slots.is_empty() => {
                match pool.key.address() {
                    Some(address) => RepairAction::VerifySlots(
                        metadata
                            .discovered_slots
                            .iter()
                            .map(|slot| (address, *slot))
                            .collect(),
                    ),
                    // Address-less Curve key (should not happen for a routed
                    // event) — nothing to target, fall back to the no-op.
                    None => RepairAction::None,
                }
            }
            // Empty discovered slots (cold-start has not run / found them) OR
            // non-Curve / Unknown metadata: fall back to the conservative no-op.
            // Crucially NOT an error — an error here would fail the WHOLE
            // `ingest_batch` (the Solidly batch-robustness lesson).
            _ => RepairAction::None,
        };

        AdapterEventResult::event(AdapterEvent {
            pool: pool.key.clone(),
            emitter: log.address,
            topic0,
            kind,
            updates: Vec::new(),
            quality: UpdateQuality::ConservativeInvalidation,
            repair,
        })
    }

    /// Quote via the pool's own `get_dy(i, j, dx)` (chain code, no reimplemented
    /// StableSwap math). `i`/`j` are the coin indices in `CurveMetadata.coins`;
    /// the deployed contract reads the warmed balances + amplification + fee and
    /// returns the `j`-coin output for `amount_in` of coin `i`.
    fn simulate_swap(
        &self,
        pool: &PoolRegistration,
        cache: &mut dyn AdapterCache,
        token_in: Address,
        token_out: Address,
        amount_in: U256,
        _config: &SimConfig,
    ) -> Result<SwapQuote, SimError> {
        let pool_address = pool
            .key
            .address()
            .ok_or(SimError::MissingMetadata("Curve pool address"))?;

        let coins = match &pool.metadata {
            ProtocolMetadata::Curve(metadata) if !metadata.coins.is_empty() => &metadata.coins,
            // Empty/missing coins: cold-start never configured the static coin
            // ordering, so the token→index mapping cannot be built.
            _ => return Err(SimError::MissingMetadata("Curve coins")),
        };

        // Map token_in→i, token_out→j by their position in `coins`. A token that
        // is not in the pool has no index, so the call must NOT be built/run —
        // this is a clean error, never a (wrong-index) quote. Both must resolve.
        let i = coins
            .iter()
            .position(|coin| *coin == token_in)
            .ok_or(SimError::MissingMetadata("Curve token not in pool"))?;
        let j = coins
            .iter()
            .position(|coin| *coin == token_out)
            .ok_or(SimError::MissingMetadata("Curve token not in pool"))?;

        // A self-swap (same coin in and out) has no meaningful quote; reject it
        // cleanly rather than building a get_dy(i, i) call the pool would revert.
        if i == j {
            return Err(SimError::Custom("Curve token_in == token_out".into()));
        }

        // Classic StableSwap `get_dy` takes `int128` indices (the `sol!` macro
        // maps `int128` to native `i128`). CryptoSwap / StableSwap-NG use
        // `uint256` indices — out of scope here (a future metadata flag).
        let calldata = Bytes::from(
            get_dyCall {
                i: i as i128,
                j: j as i128,
                dx: amount_in,
            }
            .abi_encode(),
        );

        let output = run_quote(cache, pool_address, calldata)?;
        let dy = get_dyCall::abi_decode_returns_validate(&output)
            .map_err(|_| SimError::MalformedOutput("get_dy return"))?;
        Ok(SwapQuote::new(dy))
    }
}

/// The phase a [`CurveColdStartPlanner`] is in between rounds.
enum CurvePhase {
    /// Round 1 ran the `get_dy` discover call; classify its result next.
    Discover,
    /// Round 2 verified the discovered slots; the next `on_results` is done.
    Verify,
}

/// Why a Curve cold start could not reach `Ready`.
enum CurveRepair {
    /// The discover `get_dy` call reverted, halted, or returned no output.
    DiscoverFailed,
    /// The discover call succeeded but touched no slots under `restrict_to`.
    NoSlotsDiscovered,
    /// A discovered slot could not be fetched in the verify round (an archive
    /// miss), so the warmed read-set is not authoritative.
    BalancesUnfetched,
}

/// Cold-start planner for a Curve StableSwap plain pool: a discover → verify run.
///
/// A real Curve pool's `get_dy` read-set (balances + amplification + fee) lives
/// behind a non-predictable Vyper storage layout, so the planner cannot name the
/// slots up front. Instead round 1 runs a `get_dy(0, 1, DISCOVER_DX)` call on the
/// pool (`restrict_to = [pool]`) and captures the `(pool, slot)` pairs it SLOADs.
/// Round 2 authoritatively verifies exactly those discovered slots so the live
/// read-set is warmed for a subsequent `simulate_swap`.
///
/// The flow runs for every policy (the pool state is the hot set, so there is no
/// verify-only shortcut), mirroring Balancer. The planner stays policy-aware in
/// shape (the policy is threaded into the report) so later slices can refine
/// `HotSlotsOnly`/`Lazy`.
struct CurveColdStartPlanner {
    pool: Address,
    /// Config-supplied coins, preserved across the run and re-emitted on `Ready`.
    coins: Vec<Address>,
    policy: ColdStartPolicy,
    phase: CurvePhase,
    /// The pool slots discovered in round 1 and verified in round 2.
    verified_slots: Vec<(Address, U256)>,
    /// Slots injected across the run (the refreshed read-set).
    changed_slots: Vec<SlotChange>,
    /// Set when the run cannot reach `Ready` (discover failure / empty capture /
    /// archive miss).
    repair: Option<CurveRepair>,
}

impl CurveColdStartPlanner {
    fn new(pool: Address, coins: Vec<Address>, policy: ColdStartPolicy) -> Self {
        Self {
            pool,
            coins,
            policy,
            phase: CurvePhase::Discover,
            verified_slots: Vec::new(),
            changed_slots: Vec::new(),
            repair: None,
        }
    }
}

impl AdapterColdStartPlanner for CurveColdStartPlanner {
    fn initial_plan(&mut self, _state: &dyn StateView) -> ColdStartPlan {
        // Round 1: ensure the pool's code, then run `get_dy(0, 1, DISCOVER_DX)`
        // and capture the slots it touches (restricted to the pool so only its
        // own read-set is collected — plain pools are self-contained).
        ColdStartPlan {
            accounts: vec![self.pool],
            discover: vec![ColdStartCall {
                from: Address::ZERO,
                to: self.pool,
                calldata: Bytes::from(
                    get_dyCall {
                        i: 0i128,
                        j: 1i128,
                        dx: DISCOVER_DX,
                    }
                    .abi_encode(),
                ),
                restrict_to: Some(vec![self.pool]),
            }],
            ..Default::default()
        }
    }

    fn on_results(&mut self, results: &ColdStartResults, _state: &dyn StateView) -> ColdStartStep {
        // Record any slots injected this round (round 2's refreshed read-set).
        self.changed_slots.extend(results.verified.iter().cloned());

        match self.phase {
            CurvePhase::Discover => {
                let Some(call) = results.discovered.first() else {
                    // No discover result at all — treat as a failed discovery.
                    self.repair = Some(CurveRepair::DiscoverFailed);
                    return ColdStartStep::Done;
                };

                // Classify off the load-bearing success signal first (mirroring
                // the Balancer / V2 / V3 planners): a revert/halt, or a success
                // with no output, is a failed discovery — never silently driven
                // to Ready over an empty read-set.
                if !call.result.is_success() || call.result.output().is_none() {
                    self.repair = Some(CurveRepair::DiscoverFailed);
                    return ColdStartStep::Done;
                }

                // Collect the discovered pool slots (already restricted to the
                // pool). The access list is a set, so order is unspecified.
                let discovered: Vec<(Address, U256)> = call
                    .access
                    .slots
                    .iter()
                    .filter(|(address, _)| *address == self.pool)
                    .copied()
                    .collect();

                // Empty capture is a distinguishable signal: a verify round over
                // zero slots would be a no-op, so record a repair and finish
                // rather than continue.
                if discovered.is_empty() {
                    self.repair = Some(CurveRepair::NoSlotsDiscovered);
                    return ColdStartStep::Done;
                }

                self.verified_slots = discovered.clone();
                self.phase = CurvePhase::Verify;
                ColdStartStep::Continue(ColdStartPlan {
                    verify: discovered,
                    ..Default::default()
                })
            }
            CurvePhase::Verify => {
                // The discovered slots are the hot read-set. Source their verdict
                // from the per-slot `SlotFetch` classification (like the Balancer
                // / V2 / V3 planners) so an archive miss is not silently accepted
                // as a warmed `Ready`. A genuine `Zero` is legitimate (a fresh /
                // empty pool can hold a zero balance), so only an unfetchable /
                // never-attempted slot forces a repair.
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
                    self.repair = Some(CurveRepair::BalancesUnfetched);
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
            Some(CurveRepair::DiscoverFailed) => {
                report.status = PoolStatus::Degraded;
                // Re-running discovery from scratch is the repair for a failed or
                // empty `get_dy` discover call. A Curve plain pool is a standalone
                // contract, but re-running discovery is the consistent, safe
                // repair (it captures the read-set afresh).
                ColdStartOutcome::NeedsRepair(
                    report,
                    RepairAction::ColdStart {
                        pool: pool.key.clone(),
                        policy: self.policy,
                    },
                )
            }
            Some(CurveRepair::NoSlotsDiscovered) => {
                report.status = PoolStatus::Degraded;
                // Nothing pool-specific was discovered to scope a purge to, so
                // re-run discovery (as DiscoverFailed does) rather than purge.
                ColdStartOutcome::NeedsRepair(
                    report,
                    RepairAction::ColdStart {
                        pool: pool.key.clone(),
                        policy: self.policy,
                    },
                )
            }
            Some(CurveRepair::BalancesUnfetched) => {
                report.status = PoolStatus::Degraded;
                // Archive-miss repair: re-verify exactly the discovered slots
                // (mirrors the Balancer / V2 / V3 archive-miss repair).
                ColdStartOutcome::NeedsRepair(
                    report,
                    RepairAction::VerifySlots(self.verified_slots.clone()),
                )
            }
            None => {
                // Persist the discovered read-set (slot-only; all on the pool) so
                // the reactive `TokenExchange`/liquidity path can re-verify
                // exactly them. The discovered set is order-unspecified; sort for
                // a stable, deduped record. The config-supplied `coins` are
                // preserved (static pool identity, required at simulate time).
                let mut discovered_slots: Vec<U256> =
                    self.verified_slots.iter().map(|(_, slot)| *slot).collect();
                discovered_slots.sort_unstable();
                discovered_slots.dedup();
                pool.metadata = ProtocolMetadata::Curve(CurveMetadata {
                    coins: self.coins.clone(),
                    discovered_slots,
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

    // The arity-3 topics derived from `n_coins` must equal the `sol!`-macro
    // SIGNATURE_HASHes — this proves the hand-written signature format strings in
    // `curve_event_topics`/`add_liquidity_topic` are byte-for-byte correct
    // (keccak is unforgiving), anchoring the dynamic derivation to the macro.
    #[test]
    fn derived_arity_3_topics_match_sol_macro_hashes() {
        let t3 = curve_event_topics(3);
        for expected in [
            TokenExchange::SIGNATURE_HASH,
            RemoveLiquidityOne::SIGNATURE_HASH,
            AddLiquidity::SIGNATURE_HASH,
            RemoveLiquidity::SIGNATURE_HASH,
            RemoveLiquidityImbalance::SIGNATURE_HASH,
        ] {
            assert!(
                t3.contains(&expected),
                "derived 3-coin topic set must contain the sol! hash {expected:?}"
            );
        }
        assert_eq!(add_liquidity_topic(3), AddLiquidity::SIGNATURE_HASH);
    }

    // The `uint256[N]` arity is part of the event signature, so the liquidity
    // topic hashes MUST differ per coin count — the bug the audit caught was
    // routing only the N=3 hashes, silently dropping 2-/4-coin pools' events.
    #[test]
    fn liquidity_topics_differ_per_arity() {
        let (t2, t3, t4) = (
            curve_event_topics(2),
            curve_event_topics(3),
            curve_event_topics(4),
        );
        assert_ne!(t2, t3, "2-coin and 3-coin topic sets must differ");
        assert_ne!(t3, t4, "3-coin and 4-coin topic sets must differ");
        assert_ne!(
            add_liquidity_topic(2),
            add_liquidity_topic(3),
            "AddLiquidity hash must depend on arity"
        );
        // The arity-independent swap topic is present at every arity.
        for n in [0, 2, 3, 4] {
            assert!(curve_event_topics(n).contains(&TokenExchange::SIGNATURE_HASH));
        }
        // Unconfigured (n=0) routes only the two arity-independent topics.
        assert_eq!(curve_event_topics(0).len(), 2);
    }
}
