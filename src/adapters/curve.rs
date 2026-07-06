//! Curve plain-pool adapter (StableSwap, StableSwap-NG, CryptoSwap v2, Tricrypto-NG).
//!
//! See [`docs/curve-adapter.md`] for the full guide. The closest analog is
//! [`BalancerV2Adapter`](super::balancer_v2::BalancerV2Adapter): a discover→verify
//! cold-start (Curve has no predictable balance-slot layout, so the planner
//! captures the SLOAD set of a `get_dy` call rather than naming slots up front)
//! and a resync-on-event reactive path (Curve events carry deltas, not absolute
//! balances, so re-verify the discovered slots instead of lossy delta math). Swap
//! simulation calls the pool's own `get_dy` — no reimplemented Curve math.
//!
//! ## Dialects ([`CurveVariant`])
//!
//! All four supported dialects are **plain pools** (self-contained `get_dy`); a
//! per-pool [`CurveVariant`] selects the `get_dy` index ABI and the event set.
//! The variant axes are independent:
//!
//! | Variant | `get_dy` indices | `TokenExchange` | Liquidity events |
//! | --- | --- | --- | --- |
//! | `StableSwap` (classic **and** NG) | `int128` | `int128` ids | fixed `uint256[N]` arrays; both 2-arg (classic) and 3-arg (NG) `RemoveLiquidityOne` |
//! | `CryptoSwap` (Curve v2, e.g. tricrypto2) | `uint256` | `uint256` ids (5-arg) | single-fee `AddLiquidity`, 3-arg `RemoveLiquidityOne` (no imbalance) |
//! | `CryptoSwapNG` (Tricrypto-NG) | `uint256` | extended 7-arg | extended 5-arg `AddLiquidity`, 6-arg `RemoveLiquidityOne`, `ClaimAdminFee` |
//!
//! StableSwap and StableSwap-NG share the `int128` quote path (they differ only
//! in the 3-arg `RemoveLiquidityOne`, routed by both). CryptoSwap v2 and
//! Tricrypto-NG share the `uint256` quote path (they differ only in events). All
//! event signatures were verified on-chain before routing.
//!
//! ## Out of scope
//!
//! Metapools and lending pools, whose `get_dy` makes external calls — the
//! `restrict_to=[pool]` discover capture would miss the base pool's slots.
//!
//! [`docs/curve-adapter.md`]: https://github.com/KaiCode2/evm-amm-state/blob/main/docs/curve-adapter.md

use super::cold_start::{
    AdapterColdStartPlanner, ColdStartCall, ColdStartPlan, ColdStartResults, ColdStartRunReport,
    ColdStartStep, SlotFetch,
};
use super::factory::{CurveFactory, FactoryConfig, PoolFactory};
use super::sim::{SimConfig, SimError, SwapQuote, get_dyCall, quote_via_call};
use super::{
    AdapterCache, AdapterEvent, AdapterEventError, AdapterEventKind, AdapterEventResult,
    AmmAdapter, ColdStartOutcome, ColdStartPolicy, ColdStartReport, CurveMetadata, CurveVariant,
    EventSource, PoolRegistration, PoolStatus, ProtocolId, ProtocolMetadata, RepairAction,
    SlotChange, StateView, UnsupportedReason, UpdateQuality,
};
use alloy_primitives::{Address, B256, Bytes, Log, U256, keccak256};
use alloy_sol_types::{SolCall, SolEvent, sol};

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

sol! {
    // CryptoSwap (Curve v2, e.g. tricrypto2) events. Namespaced under an interface
    // so the generated types don't collide with the StableSwap ones above. All
    // signatures verified on-chain against tricrypto2 (eth_getLogs topic0 histogram).
    // Only `TokenExchange` is decode-validated before emitting a Swap; the
    // liquidity events route on topic only (the reactive path resyncs discovered
    // slots, not deltas). Arities are derived from n_coins at routing time; these
    // N=3 decls are the `#[cfg(test)]` reference for the derived hashes.
    //
    // `RemoveLiquidityOne` here is the **3-arg** form (token_amount, coin_index,
    // coin_amount) — emitted by BOTH CryptoSwap v2 AND StableSwap-NG (classic
    // StableSwap uses the 2-arg form above), so the StableSwap routing reuses this
    // hash. CryptoSwap v2 has no RemoveLiquidityImbalance.
    interface CurveCryptoSwapEvents {
        event TokenExchange(address indexed buyer, uint256 sold_id, uint256 tokens_sold, uint256 bought_id, uint256 tokens_bought);
        event AddLiquidity(address indexed provider, uint256[3] token_amounts, uint256 fee, uint256 token_supply);
        event RemoveLiquidity(address indexed provider, uint256[3] token_amounts, uint256 token_supply);
        event RemoveLiquidityOne(address indexed provider, uint256 token_amount, uint256 coin_index, uint256 coin_amount);
    }
}

sol! {
    // Tricrypto-NG (Curve's newest crypto pools) events — EXTENDED forms with
    // extra `fee`/`packed_price_scale` fields, so their signature hashes differ
    // from CryptoSwap v2's. All verified on-chain against tricryptoUSDC + USDT
    // (eth_getLogs topic0 histogram). `RemoveLiquidity` is identical to v2 (so it
    // reuses `CurveCryptoSwapEvents::RemoveLiquidity`). `ClaimAdminFee` is routed
    // because `claim_admin_fees` can update D/price_scale (the crypto read-set).
    // Arities are derived from n_coins at routing time; these N=3 decls are the
    // `#[cfg(test)]` reference + the TokenExchange decode-validation type.
    interface CurveTricryptoNgEvents {
        event TokenExchange(address indexed buyer, uint256 sold_id, uint256 tokens_sold, uint256 bought_id, uint256 tokens_bought, uint256 fee, uint256 packed_price_scale);
        event AddLiquidity(address indexed provider, uint256[3] token_amounts, uint256 fee, uint256 token_supply, uint256 packed_price_scale);
        event RemoveLiquidityOne(address indexed provider, uint256 token_amount, uint256 coin_index, uint256 coin_amount, uint256 approx_fee, uint256 packed_price_scale);
        event ClaimAdminFee(address indexed admin, uint256 tokens);
    }
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

/// The Curve dialect for a registration, defaulting to `StableSwap` when the
/// metadata is not Curve (e.g. `Unknown`) — the slice-1 / NG behavior.
fn pool_variant(pool: &PoolRegistration) -> CurveVariant {
    match &pool.metadata {
        ProtocolMetadata::Curve(metadata) => metadata.variant,
        _ => CurveVariant::StableSwap,
    }
}

/// The StableSwap `AddLiquidity` topic hash for an `n_coins`-coin pool:
/// `uint256[N]` token_amounts + `uint256[N]` fees + invariant + supply. The
/// `uint256[N]` arity IS part of the canonical signature, so the hash is
/// pool-specific; derived from `n_coins`. A `#[cfg(test)]` check asserts the N=3
/// derivation equals the `sol!`-macro `SIGNATURE_HASH`.
fn add_liquidity_topic(n_coins: usize) -> B256 {
    keccak256(
        format!("AddLiquidity(address,uint256[{n_coins}],uint256[{n_coins}],uint256,uint256)")
            .as_bytes(),
    )
}

/// The CryptoSwap (v2) `AddLiquidity` topic hash for an `n_coins`-coin pool:
/// `uint256[N]` token_amounts + a SINGLE `uint256 fee` + supply (no fees array) —
/// distinct from the StableSwap shape above.
fn crypto_add_liquidity_topic(n_coins: usize) -> B256 {
    keccak256(format!("AddLiquidity(address,uint256[{n_coins}],uint256,uint256)").as_bytes())
}

/// The Tricrypto-NG `AddLiquidity` topic hash for an `n_coins`-coin pool:
/// `uint256[N]` token_amounts + fee + token_supply + packed_price_scale (3
/// trailing scalars) — distinct from CryptoSwap v2's single-fee form.
fn crypto_ng_add_liquidity_topic(n_coins: usize) -> B256 {
    keccak256(
        format!("AddLiquidity(address,uint256[{n_coins}],uint256,uint256,uint256)").as_bytes(),
    )
}

/// Topic hashes this adapter routes for an `n_coins`-coin pool of `variant`. All
/// signatures verified on-chain (docs/curve-slice3-liquidity-events-spec.md).
///
/// **StableSwap** (classic + NG): `TokenExchange(int128…)` + the fixed-`uint256[N]`
/// liquidity events (AddLiquidity / RemoveLiquidity / RemoveLiquidityImbalance,
/// arity-derived) + `RemoveLiquidityOne` in BOTH the 2-arg (classic) and 3-arg
/// (NG) forms. A pool emits only one RemoveLiquidityOne form, so routing both is
/// harmless. `n_coins == 0` routes only the arity-independent topics.
///
/// **CryptoSwap** (Curve v2): `TokenExchange(uint256…)` + the CryptoSwap liquidity
/// events — single-fee `AddLiquidity` and fees-array-less `RemoveLiquidity` (both
/// fixed-`uint256[N]`) + the 3-arg `RemoveLiquidityOne`. (No RemoveLiquidityImbalance.)
///
/// **CryptoSwapNG** (Tricrypto-NG): the EXTENDED events — 7-arg `TokenExchange`,
/// 5-arg `AddLiquidity` (extra `packed_price_scale`), 6-arg `RemoveLiquidityOne`,
/// the shared fees-array-less `RemoveLiquidity`, and `ClaimAdminFee` (routed
/// because `claim_admin_fees` can move D/price_scale).
fn curve_event_topics(n_coins: usize, variant: CurveVariant) -> Vec<B256> {
    // 3-arg RemoveLiquidityOne (token_amount, coin_index, coin_amount): shared by
    // CryptoSwap v2 and StableSwap-NG.
    let remove_one_3arg = CurveCryptoSwapEvents::RemoveLiquidityOne::SIGNATURE_HASH;
    // RemoveLiquidity (uint256[N] token_amounts, supply) is identical for CryptoSwap
    // v2 and Tricrypto-NG.
    let crypto_remove_liquidity =
        |n: usize| keccak256(format!("RemoveLiquidity(address,uint256[{n}],uint256)").as_bytes());
    match variant {
        CurveVariant::CryptoSwap => {
            let mut topics = vec![
                CurveCryptoSwapEvents::TokenExchange::SIGNATURE_HASH,
                remove_one_3arg,
            ];
            if n_coins >= 1 {
                topics.push(crypto_add_liquidity_topic(n_coins));
                topics.push(crypto_remove_liquidity(n_coins));
            }
            topics
        }
        CurveVariant::CryptoSwapNG => {
            let mut topics = vec![
                CurveTricryptoNgEvents::TokenExchange::SIGNATURE_HASH, // 7-arg
                CurveTricryptoNgEvents::RemoveLiquidityOne::SIGNATURE_HASH, // 6-arg
                CurveTricryptoNgEvents::ClaimAdminFee::SIGNATURE_HASH,
            ];
            if n_coins >= 1 {
                topics.push(crypto_ng_add_liquidity_topic(n_coins)); // 5-arg
                topics.push(crypto_remove_liquidity(n_coins)); // shared with v2
            }
            topics
        }
        CurveVariant::StableSwap => {
            let mut topics = vec![
                TokenExchange::SIGNATURE_HASH,
                RemoveLiquidityOne::SIGNATURE_HASH, // 2-arg (classic)
                remove_one_3arg,                    // 3-arg (NG)
            ];
            if n_coins >= 1 {
                topics.push(add_liquidity_topic(n_coins));
                topics.push(keccak256(
                    format!(
                        "RemoveLiquidity(address,uint256[{n_coins}],uint256[{n_coins}],uint256)"
                    )
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
    }
}

impl AmmAdapter for CurveAdapter {
    fn protocol(&self) -> ProtocolId {
        ProtocolId::Curve
    }

    /// One [`CurveFactory`] per configured
    /// [`CurveFactoryConfig`](super::factory::CurveFactoryConfig) MetaRegistry
    /// endpoint — Curve plain-pool discovery is a ViewCall against the registry
    /// (see [`CurveFactory`]).
    fn pool_factories(&self, config: &FactoryConfig) -> Vec<Box<dyn PoolFactory>> {
        config
            .curve
            .iter()
            .map(|cfg| Box::new(CurveFactory::new(cfg.clone())) as Box<dyn PoolFactory>)
            .collect()
    }

    fn event_sources(&self, pool: &PoolRegistration) -> Vec<EventSource> {
        let n_coins = pool_n_coins(pool);
        let variant = pool_variant(pool);
        pool.key
            .address()
            .map(|address| EventSource::direct(address, curve_event_topics(n_coins, variant)))
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

        // Preserve the config-supplied coins + variant across cold-start so
        // `finish` can re-emit them alongside the discovered slots. The variant
        // also drives the discover call's `get_dy` ABI (a CryptoSwap pool
        // reverts the int128 discover, which would be a spurious DiscoverFailed).
        let (coins, variant) = match &pool.metadata {
            ProtocolMetadata::Curve(metadata) => (metadata.coins.clone(), metadata.variant),
            _ => (Vec::new(), CurveVariant::StableSwap),
        };

        Ok(Box::new(CurveColdStartPlanner::new(
            address, coins, variant, policy,
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
        // Route against the pool's VARIANT- and ARITY-specific topic set: the
        // liquidity-event hashes depend on n_coins (the `uint256[N]` arity is part
        // of the signature) AND the StableSwap vs CryptoSwap shapes differ, so a
        // fixed/wrong set would silently drop a pool's liquidity events.
        let n_coins = pool_n_coins(pool);
        let variant = pool_variant(pool);
        if !curve_event_topics(n_coins, variant).contains(&topic0) {
            return AdapterEventResult::ignored();
        }

        // `TokenExchange` is the swap event; validate it decodes against the
        // variant's ABI (a malformed log is a hard decode error, matching
        // Balancer). Liquidity events route on topic only — their payloads are
        // never decoded, since the reactive path resyncs the discovered slots
        // rather than applying their deltas. Added-vs-Removed is keyed off the
        // variant-specific AddLiquidity topic.
        let kind = match variant {
            CurveVariant::CryptoSwap => {
                if topic0 == CurveCryptoSwapEvents::TokenExchange::SIGNATURE_HASH {
                    if CurveCryptoSwapEvents::TokenExchange::decode_log_data_validate(&log.data)
                        .is_err()
                    {
                        return AdapterEventResult::error(AdapterEventError::MalformedLog(
                            "malformed Curve CryptoSwap TokenExchange log",
                        ));
                    }
                    AdapterEventKind::Swap
                } else if topic0 == crypto_add_liquidity_topic(n_coins) {
                    AdapterEventKind::LiquidityAdded
                } else {
                    // CryptoSwap RemoveLiquidity / RemoveLiquidityOne (3-arg).
                    AdapterEventKind::LiquidityRemoved
                }
            }
            CurveVariant::CryptoSwapNG => {
                if topic0 == CurveTricryptoNgEvents::TokenExchange::SIGNATURE_HASH {
                    if CurveTricryptoNgEvents::TokenExchange::decode_log_data_validate(&log.data)
                        .is_err()
                    {
                        return AdapterEventResult::error(AdapterEventError::MalformedLog(
                            "malformed Curve Tricrypto-NG TokenExchange log",
                        ));
                    }
                    AdapterEventKind::Swap
                } else if topic0 == crypto_ng_add_liquidity_topic(n_coins) {
                    AdapterEventKind::LiquidityAdded
                } else if topic0 == CurveTricryptoNgEvents::ClaimAdminFee::SIGNATURE_HASH {
                    // Admin-fee claim: a protocol-internal state update (can move
                    // D / price_scale), routed conservatively -> resync. Not a
                    // user swap or liquidity op, so the kind is Unknown.
                    AdapterEventKind::Unknown
                } else {
                    // Tricrypto-NG RemoveLiquidity / RemoveLiquidityOne (6-arg).
                    AdapterEventKind::LiquidityRemoved
                }
            }
            CurveVariant::StableSwap => {
                if topic0 == TokenExchange::SIGNATURE_HASH {
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
                }
            }
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
        // (The token->index mapping and these guards are variant-INDEPENDENT.)
        if i == j {
            return Err(SimError::Custom("Curve token_in == token_out".into()));
        }

        // The index ABI is the only quote-axis: classic StableSwap (and
        // StableSwap-NG) `get_dy` takes `int128` indices (the `sol!` macro maps
        // `int128` to native `i128`); CryptoSwap v2 AND Tricrypto-NG both take
        // `uint256` indices (they differ only in events, not the quote ABI). Both
        // run the pool's own `get_dy` against the warmed state and decode a bare
        // `uint256` output (no reimplemented AMM math).
        let dy = match pool_variant(pool) {
            CurveVariant::StableSwap => {
                let calldata = Bytes::from(
                    get_dyCall {
                        i: i as i128,
                        j: j as i128,
                        dx: amount_in,
                    }
                    .abi_encode(),
                );
                let output = quote_via_call(cache, pool_address, calldata)?;
                get_dyCall::abi_decode_returns_validate(&output)
                    .map_err(|_| SimError::MalformedOutput("get_dy return"))?
            }
            CurveVariant::CryptoSwap | CurveVariant::CryptoSwapNG => {
                let calldata = Bytes::from(
                    super::sim::CurveCryptoSwap::get_dyCall {
                        i: U256::from(i),
                        j: U256::from(j),
                        dx: amount_in,
                    }
                    .abi_encode(),
                );
                let output = quote_via_call(cache, pool_address, calldata)?;
                super::sim::CurveCryptoSwap::get_dyCall::abi_decode_returns_validate(&output)
                    .map_err(|_| SimError::MalformedOutput("CryptoSwap get_dy return"))?
            }
        };
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
    /// Config-supplied Curve dialect; drives the discover `get_dy` ABI and is
    /// re-emitted on `Ready` so reactive + later sims keep it.
    variant: CurveVariant,
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
    fn new(
        pool: Address,
        coins: Vec<Address>,
        variant: CurveVariant,
        policy: ColdStartPolicy,
    ) -> Self {
        Self {
            pool,
            coins,
            variant,
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
        // own read-set is collected — plain pools are self-contained). The
        // discover calldata MUST use the variant's `get_dy` ABI: a CryptoSwap
        // pool reverts the int128 `get_dy`, which would mis-classify as a
        // spurious DiscoverFailed.
        let calldata = match self.variant {
            CurveVariant::StableSwap => Bytes::from(
                get_dyCall {
                    i: 0i128,
                    j: 1i128,
                    dx: DISCOVER_DX,
                }
                .abi_encode(),
            ),
            // CryptoSwap v2 and Tricrypto-NG share the uint256 get_dy ABI.
            CurveVariant::CryptoSwap | CurveVariant::CryptoSwapNG => Bytes::from(
                super::sim::CurveCryptoSwap::get_dyCall {
                    i: U256::ZERO,
                    j: U256::from(1),
                    dx: DISCOVER_DX,
                }
                .abi_encode(),
            ),
        };
        ColdStartPlan {
            accounts: vec![self.pool],
            discover: vec![ColdStartCall {
                from: Address::ZERO,
                to: self.pool,
                calldata,
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
                    // Persist the config-supplied variant so the reactive path
                    // and later sims keep the correct `get_dy` / event ABI.
                    variant: self.variant,
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
        let t3 = curve_event_topics(3, CurveVariant::StableSwap);
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
            curve_event_topics(2, CurveVariant::StableSwap),
            curve_event_topics(3, CurveVariant::StableSwap),
            curve_event_topics(4, CurveVariant::StableSwap),
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
            assert!(
                curve_event_topics(n, CurveVariant::StableSwap)
                    .contains(&TokenExchange::SIGNATURE_HASH)
            );
        }
        // Unconfigured (n=0) routes only the arity-independent topics:
        // TokenExchange + RemoveLiquidityOne in both the 2-arg (classic) and
        // 3-arg (NG) forms.
        assert_eq!(curve_event_topics(0, CurveVariant::StableSwap).len(), 3);
    }

    // The CryptoSwap (uint256-id) TokenExchange signature hash MUST differ from
    // the classic StableSwap (int128-id) one — they are distinct ABIs, and a
    // collision would route the wrong decode/validation. keccak is unforgiving.
    #[test]
    fn cryptoswap_token_exchange_topic_differs_from_stableswap() {
        assert_ne!(
            CurveCryptoSwapEvents::TokenExchange::SIGNATURE_HASH,
            TokenExchange::SIGNATURE_HASH,
            "CryptoSwap (uint256 ids) and StableSwap (int128 ids) TokenExchange \
             hashes must differ"
        );
    }

    // The CryptoSwap liquidity topics derived from n_coins must equal the `sol!`
    // macro hashes — proving the hand-written format strings (single-fee
    // AddLiquidity, fees-array-less RemoveLiquidity, 3-arg RemoveLiquidityOne) are
    // byte-for-byte correct, anchored to the macro. All verified on-chain too.
    #[test]
    fn cryptoswap_derived_liquidity_topics_match_sol_macro() {
        assert_eq!(
            crypto_add_liquidity_topic(3),
            CurveCryptoSwapEvents::AddLiquidity::SIGNATURE_HASH,
            "derived CryptoSwap AddLiquidity hash must match the macro"
        );
        assert_eq!(
            keccak256("RemoveLiquidity(address,uint256[3],uint256)".as_bytes()),
            CurveCryptoSwapEvents::RemoveLiquidity::SIGNATURE_HASH,
            "derived CryptoSwap RemoveLiquidity hash must match the macro"
        );
        // CryptoSwap vs StableSwap AddLiquidity are distinct shapes.
        assert_ne!(crypto_add_liquidity_topic(3), add_liquidity_topic(3));
        // 3-arg (NG/crypto) RemoveLiquidityOne differs from 2-arg (classic).
        assert_ne!(
            CurveCryptoSwapEvents::RemoveLiquidityOne::SIGNATURE_HASH,
            RemoveLiquidityOne::SIGNATURE_HASH,
        );
    }

    // Each variant routes its FULL liquidity set, swap topics never cross-route,
    // and the 3-arg RemoveLiquidityOne is routed by BOTH variants (NG + crypto).
    #[test]
    fn variant_topic_sets_are_correct() {
        let crypto = curve_event_topics(3, CurveVariant::CryptoSwap);
        let stable = curve_event_topics(3, CurveVariant::StableSwap);
        let remove_one_3arg = CurveCryptoSwapEvents::RemoveLiquidityOne::SIGNATURE_HASH;

        for expected in [
            CurveCryptoSwapEvents::TokenExchange::SIGNATURE_HASH,
            crypto_add_liquidity_topic(3),
            keccak256("RemoveLiquidity(address,uint256[3],uint256)".as_bytes()),
            remove_one_3arg,
        ] {
            assert!(
                crypto.contains(&expected),
                "CryptoSwap set missing {expected:?}"
            );
        }
        // CryptoSwap v2 has no RemoveLiquidityImbalance.
        assert!(!crypto.contains(&keccak256(
            "RemoveLiquidityImbalance(address,uint256[3],uint256[3],uint256,uint256)".as_bytes()
        )));
        // Swap topics never cross-route.
        assert!(!crypto.contains(&TokenExchange::SIGNATURE_HASH));
        assert!(!stable.contains(&CurveCryptoSwapEvents::TokenExchange::SIGNATURE_HASH));
        // StableSwap routes BOTH RemoveLiquidityOne forms (classic 2-arg + NG 3-arg).
        assert!(stable.contains(&RemoveLiquidityOne::SIGNATURE_HASH));
        assert!(stable.contains(&remove_one_3arg));
    }

    // Tricrypto-NG derived liquidity topics must equal the sol! macro hashes
    // (extended 5-arg AddLiquidity), and its EXTENDED event signatures must be
    // distinct from both CryptoSwap v2 and StableSwap. All verified on-chain.
    #[test]
    fn cryptoswap_ng_derived_topics_match_sol_macro() {
        assert_eq!(
            crypto_ng_add_liquidity_topic(3),
            CurveTricryptoNgEvents::AddLiquidity::SIGNATURE_HASH,
            "derived Tricrypto-NG AddLiquidity hash must match the macro"
        );
        // NG TokenExchange (7-arg) differs from v2 (5-arg) and StableSwap (int128).
        assert_ne!(
            CurveTricryptoNgEvents::TokenExchange::SIGNATURE_HASH,
            CurveCryptoSwapEvents::TokenExchange::SIGNATURE_HASH,
        );
        assert_ne!(
            CurveTricryptoNgEvents::TokenExchange::SIGNATURE_HASH,
            TokenExchange::SIGNATURE_HASH,
        );
        // NG RemoveLiquidityOne (6-arg) differs from v2 (3-arg) and classic (2-arg).
        assert_ne!(
            CurveTricryptoNgEvents::RemoveLiquidityOne::SIGNATURE_HASH,
            CurveCryptoSwapEvents::RemoveLiquidityOne::SIGNATURE_HASH,
        );
        // NG AddLiquidity (5-arg) differs from v2 (4-arg single-fee).
        assert_ne!(
            crypto_ng_add_liquidity_topic(3),
            crypto_add_liquidity_topic(3)
        );
    }

    // The Tricrypto-NG topic set routes its extended events + the shared
    // RemoveLiquidity + ClaimAdminFee, and never the v2/StableSwap swap topics.
    #[test]
    fn cryptoswap_ng_topic_set_correct() {
        let ng = curve_event_topics(3, CurveVariant::CryptoSwapNG);
        for expected in [
            CurveTricryptoNgEvents::TokenExchange::SIGNATURE_HASH,
            crypto_ng_add_liquidity_topic(3),
            keccak256("RemoveLiquidity(address,uint256[3],uint256)".as_bytes()), // shared w/ v2
            CurveTricryptoNgEvents::RemoveLiquidityOne::SIGNATURE_HASH,
            CurveTricryptoNgEvents::ClaimAdminFee::SIGNATURE_HASH,
        ] {
            assert!(
                ng.contains(&expected),
                "Tricrypto-NG set missing {expected:?}"
            );
        }
        // No swap-topic cross-routing with v2 or StableSwap.
        assert!(!ng.contains(&CurveCryptoSwapEvents::TokenExchange::SIGNATURE_HASH));
        assert!(!ng.contains(&TokenExchange::SIGNATURE_HASH));
        // NG uses its own AddLiquidity shape, not v2's.
        assert!(!ng.contains(&crypto_add_liquidity_topic(3)));
    }
}
