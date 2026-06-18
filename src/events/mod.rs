//! Event-driven pool state updates.
//!
//! This module turns on-chain logs into in-memory pool-state mutations so that
//! a set of [`LocalAMM`]s tracks live chain state without re-reading storage on
//! every block. It is the bridge between a log subscription (e.g.
//! `provider.subscribe_logs`) and the locally-simulatable pool models.
//!
//! Three pieces fit together:
//!
//! - [`apply_log`] decodes a single log and applies it to one pool, in place
//!   and fully offline (no RPC). It supports every pool family the crate
//!   models — Uniswap V2/V3, PancakeSwap V3, Solidly V2, Slipstream, Curve,
//!   Balancer V2/V3, and ERC4626 — filling the gap left by the upstream `amms`
//!   crate, whose `sync()` is a no-op for several of these types.
//! - [`EventRouter`] owns a set of pools keyed by address, builds the topic
//!   filter to subscribe with ([`EventRouter::subscription_topics`]), routes
//!   each incoming log to the right pool (handling vault-emitted Balancer
//!   events), and applies it. [`EventRouter::snapshot`] produces an immutable,
//!   `Send + Sync` copy of all pool states for offline parallel simulation.
//! - [`mirror_updates_to_cache`] pushes the freshly-applied state back into an
//!   [`EvmCache`] so that EVM-level reads (e.g. `call_raw` quotes) see the same
//!   values, reusing the crate's existing hot-state injection path.
//!
//! # Example
//!
//! ```ignore
//! let router = EventRouter::from_loaded(amms);
//! let topics = router.subscription_topics();
//! let filter = Filter::new().event_signature(topics);
//! let mut stream = provider.subscribe_logs(&filter).await?.into_stream();
//! while let Some(log) = stream.next().await {
//!     if let Some(update) = router.apply(&log)? {
//!         // Pools now reflect the new state; simulate offline.
//!         let snapshot = router.snapshot();
//!         // ...
//!     }
//! }
//! ```

use std::collections::HashMap;

use alloy_primitives::{Address, B256, U256};
use alloy_rpc_types_eth::Log;
use alloy_sol_types::{SolEvent, sol};
use amms::amms::amm::AutomatedMarketMaker;
use evm_fork_cache::cache::{EvmCache, SlotObservationTracker};

use crate::amm_wrapper::{LocalAMM, Variant};
use crate::cache_sync::{AMMRef, V3Flavor, inject_hot_state_to_evm, inject_v3_tick_data};

sol! {
    interface IUniswapV2Events {
        event Sync(uint112 reserve0, uint112 reserve1);
    }
    interface ISolidlyEvents {
        event Sync(uint256 reserve0, uint256 reserve1);
    }
    interface IUniswapV3Events {
        event Swap(
            address indexed sender,
            address indexed recipient,
            int256 amount0,
            int256 amount1,
            uint160 sqrtPriceX96,
            uint128 liquidity,
            int24 tick
        );
        event Mint(
            address sender,
            address indexed owner,
            int24 indexed tickLower,
            int24 indexed tickUpper,
            uint128 amount,
            uint256 amount0,
            uint256 amount1
        );
        event Burn(
            address indexed owner,
            int24 indexed tickLower,
            int24 indexed tickUpper,
            uint128 amount,
            uint256 amount0,
            uint256 amount1
        );
    }
    interface ICurveStableEvents {
        event TokenExchange(
            address indexed buyer,
            int128 sold_id,
            uint256 tokens_sold,
            int128 bought_id,
            uint256 tokens_bought
        );
    }
    interface ICurveCryptoEvents {
        event TokenExchange(
            address indexed buyer,
            uint256 sold_id,
            uint256 tokens_sold,
            uint256 bought_id,
            uint256 tokens_bought
        );
    }
    interface IBalancerV2Events {
        event Swap(
            bytes32 indexed poolId,
            address indexed tokenIn,
            address indexed tokenOut,
            uint256 amountIn,
            uint256 amountOut
        );
    }
    interface IBalancerV3Events {
        event Swap(
            address indexed pool,
            address indexed tokenIn,
            address indexed tokenOut,
            uint256 amountIn,
            uint256 amountOut,
            uint256 swapFeePercentage,
            uint256 swapFeeAmount
        );
    }
    interface IERC4626Events {
        event Deposit(address indexed sender, address indexed owner, uint256 assets, uint256 shares);
        event Withdraw(
            address indexed sender,
            address indexed receiver,
            address indexed owner,
            uint256 assets,
            uint256 shares
        );
    }
}

/// What an applied log changed about a pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateKind {
    /// A swap moved reserves/balances/price. For concentrated-liquidity pools
    /// this also updates the active liquidity and current tick.
    Swap,
    /// A liquidity position was added or removed (Uniswap/Slipstream Mint/Burn,
    /// ERC4626 deposit/withdraw). For V3-style pools the affected tick range is
    /// carried so the cache mirror can re-inject exactly those ticks.
    Liquidity {
        /// Lower tick of the affected range (V3-style pools only; `0` otherwise).
        tick_lower: i32,
        /// Upper tick of the affected range (V3-style pools only; `0` otherwise).
        tick_upper: i32,
    },
}

/// The result of applying a single log to a pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolUpdate {
    /// Address of the pool whose state changed.
    pub address: Address,
    /// Family of the pool that was updated.
    pub variant: Variant,
    /// Nature of the change.
    pub kind: UpdateKind,
}

/// Errors that can occur while applying an event log.
#[derive(Debug)]
pub enum EventError {
    /// The log had no `topic0` (anonymous event); cannot be routed.
    MissingTopic,
    /// The log's data was shorter than the expected ABI layout.
    Truncated,
    /// The log's payload could not be decoded against the expected ABI.
    Decode(alloy_sol_types::Error),
    /// The underlying pool rejected the log.
    Amm(amms::amms::error::AMMError),
}

impl std::fmt::Display for EventError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EventError::MissingTopic => write!(f, "log has no topic0"),
            EventError::Truncated => write!(f, "log data shorter than expected"),
            EventError::Decode(e) => write!(f, "event decode error: {e}"),
            EventError::Amm(e) => write!(f, "pool sync error: {e}"),
        }
    }
}

impl std::error::Error for EventError {}

impl From<alloy_sol_types::Error> for EventError {
    fn from(e: alloy_sol_types::Error) -> Self {
        EventError::Decode(e)
    }
}

impl From<amms::amms::error::AMMError> for EventError {
    fn from(e: amms::amms::error::AMMError) -> Self {
        EventError::Amm(e)
    }
}

/// Read the 32-byte word at index `i` of a log's data, as a `U256`.
fn data_word(log: &Log, i: usize) -> Option<U256> {
    let bytes = log.inner.data.data.as_ref();
    let start = i * 32;
    bytes.get(start..start + 32).map(U256::from_be_slice)
}

/// Interpret an indexed `int24` topic as an `i32`.
///
/// Solidity sign-extends the value across the full 32-byte topic, so the low
/// four bytes already carry the correct two's-complement `i32` for any value in
/// the `int24` range.
fn topic_to_i32(topic: &B256) -> i32 {
    let mut b = [0u8; 4];
    b.copy_from_slice(&topic.as_slice()[28..32]);
    i32::from_be_bytes(b)
}

/// The set of `topic0` event signatures relevant to a given pool family.
///
/// These are the topics to subscribe to so that [`apply_log`] can keep a pool
/// of this family up to date.
pub fn event_topics_for(variant: Variant) -> Vec<B256> {
    match variant {
        Variant::UniswapV2 => vec![IUniswapV2Events::Sync::SIGNATURE_HASH],
        Variant::UniswapV3 | Variant::PancakeSwapV3 | Variant::Slipstream => vec![
            IUniswapV3Events::Swap::SIGNATURE_HASH,
            IUniswapV3Events::Mint::SIGNATURE_HASH,
            IUniswapV3Events::Burn::SIGNATURE_HASH,
        ],
        Variant::SolidlyV2 => vec![
            ISolidlyEvents::Sync::SIGNATURE_HASH,
            IUniswapV2Events::Sync::SIGNATURE_HASH,
        ],
        Variant::Curve => vec![
            ICurveStableEvents::TokenExchange::SIGNATURE_HASH,
            ICurveCryptoEvents::TokenExchange::SIGNATURE_HASH,
        ],
        Variant::Balancer => vec![IBalancerV2Events::Swap::SIGNATURE_HASH],
        Variant::BalancerV3 => vec![IBalancerV3Events::Swap::SIGNATURE_HASH],
        Variant::ERC4626 => vec![
            IERC4626Events::Deposit::SIGNATURE_HASH,
            IERC4626Events::Withdraw::SIGNATURE_HASH,
        ],
        // The Uniswap V4 wrapper is a non-functional stub; it has no events.
        Variant::UniswapV4 => Vec::new(),
    }
}

/// Every event `topic0` the crate knows how to apply.
///
/// Useful for a blanket subscription filter when the exact pool families are
/// not known ahead of time; logs from unrelated contracts are ignored by the
/// router's address lookup.
pub fn all_event_topics() -> Vec<B256> {
    let mut topics = vec![
        IUniswapV2Events::Sync::SIGNATURE_HASH,
        ISolidlyEvents::Sync::SIGNATURE_HASH,
        IUniswapV3Events::Swap::SIGNATURE_HASH,
        IUniswapV3Events::Mint::SIGNATURE_HASH,
        IUniswapV3Events::Burn::SIGNATURE_HASH,
        ICurveStableEvents::TokenExchange::SIGNATURE_HASH,
        ICurveCryptoEvents::TokenExchange::SIGNATURE_HASH,
        IBalancerV2Events::Swap::SIGNATURE_HASH,
        IBalancerV3Events::Swap::SIGNATURE_HASH,
        IERC4626Events::Deposit::SIGNATURE_HASH,
        IERC4626Events::Withdraw::SIGNATURE_HASH,
    ];
    topics.sort_unstable();
    topics.dedup();
    topics
}

/// The pool address a log targets.
///
/// Most pools emit their own events, so the target is `log.address()`. Balancer
/// is the exception: both the V2 and V3 vaults emit `Swap` on behalf of their
/// pools, identifying the pool in the first indexed topic (a `poolId` whose
/// first 20 bytes are the pool address for V2, or the pool address directly for
/// V3).
pub fn route_target(log: &Log) -> Option<Address> {
    let topic0 = log.topics().first().copied()?;
    if topic0 == IBalancerV2Events::Swap::SIGNATURE_HASH {
        let pool_id = log.topics().get(1)?;
        Some(Address::from_slice(&pool_id.as_slice()[0..20]))
    } else if topic0 == IBalancerV3Events::Swap::SIGNATURE_HASH {
        let pool = log.topics().get(1)?;
        Some(Address::from_word(*pool))
    } else {
        Some(log.address())
    }
}

/// Apply a single log to a pool in place, fully offline.
///
/// Returns `Ok(Some(update))` describing the change, `Ok(None)` if the log's
/// `topic0` is not relevant to this pool family, or an error if a relevant log
/// failed to decode/apply.
///
/// # Per-family behaviour
///
/// - **Uniswap V2 / PancakeSwap-V2-style**: `Sync` sets reserves exactly.
/// - **Uniswap V3 / PancakeSwap V3 / Slipstream**: `Swap` updates
///   price/tick/active-liquidity; `Mint`/`Burn` update the in-memory tick map,
///   tick bitmap and active liquidity (delegated to the `amms` tick math).
/// - **Solidly V2**: `Sync` sets reserves exactly.
/// - **Curve**: `TokenExchange` applies the swap deltas to per-coin reserves.
/// - **Balancer V2 / V3**: the vault `Swap` applies amount-in/out deltas to the
///   pool balances.
/// - **ERC4626**: `Deposit`/`Withdraw` adjust the asset/share reserves.
pub fn apply_log(amm: &mut LocalAMM, log: &Log) -> Result<Option<PoolUpdate>, EventError> {
    let topic0 = log
        .topics()
        .first()
        .copied()
        .ok_or(EventError::MissingTopic)?;
    let variant = amm.variant();
    let address = amm.address();

    let kind = match amm {
        LocalAMM::UniswapV2(pool) => {
            if topic0 != IUniswapV2Events::Sync::SIGNATURE_HASH {
                return Ok(None);
            }
            pool.sync(log)?;
            Some(UpdateKind::Swap)
        }
        LocalAMM::UniswapV3(pool) | LocalAMM::PancakeSwapV3(pool) => apply_v3(pool, log, topic0)?,
        LocalAMM::ERC4626(pool) => {
            if topic0 != IERC4626Events::Deposit::SIGNATURE_HASH
                && topic0 != IERC4626Events::Withdraw::SIGNATURE_HASH
            {
                return Ok(None);
            }
            pool.sync(log)?;
            Some(UpdateKind::Liquidity {
                tick_lower: 0,
                tick_upper: 0,
            })
        }
        LocalAMM::Slipstream(pool) => {
            let mut v3 = pool.as_v3_pool();
            let result = apply_v3(&mut v3, log, topic0)?;
            if result.is_some() {
                pool.apply_v3_state(&v3);
            }
            result
        }
        LocalAMM::SolidlyV2(pool) => {
            if topic0 != ISolidlyEvents::Sync::SIGNATURE_HASH
                && topic0 != IUniswapV2Events::Sync::SIGNATURE_HASH
            {
                return Ok(None);
            }
            let reserve0 = data_word(log, 0).ok_or(EventError::Truncated)?;
            let reserve1 = data_word(log, 1).ok_or(EventError::Truncated)?;
            pool.reserve_0 = reserve0.saturating_to();
            pool.reserve_1 = reserve1.saturating_to();
            Some(UpdateKind::Swap)
        }
        LocalAMM::Curve(pool) => apply_curve(pool, log, topic0)?,
        LocalAMM::Balancer(pool) => {
            if topic0 != IBalancerV2Events::Swap::SIGNATURE_HASH {
                return Ok(None);
            }
            let ev = IBalancerV2Events::Swap::decode_log(log.as_ref())?;
            if pool.apply_vault_swap(ev.tokenIn, ev.amountIn, ev.tokenOut, ev.amountOut) {
                Some(UpdateKind::Swap)
            } else {
                None
            }
        }
        LocalAMM::BalancerV3(pool) => {
            if topic0 != IBalancerV3Events::Swap::SIGNATURE_HASH {
                return Ok(None);
            }
            let ev = IBalancerV3Events::Swap::decode_log(log.as_ref())?;
            // Resolve both legs against the modeled token pair first. A real V3
            // pool may hold tokens this 2-token model doesn't track; applying
            // only one side would corrupt the balances, so it's all-or-nothing.
            let index_of = |token: Address| -> Option<usize> {
                if token == pool.token_a {
                    Some(0)
                } else if token == pool.token_b {
                    Some(1)
                } else {
                    None
                }
            };
            match (index_of(ev.tokenIn), index_of(ev.tokenOut)) {
                (Some(i), Some(o)) => {
                    pool.balances[i] = pool.balances[i].saturating_add(ev.amountIn);
                    pool.balances[o] = pool.balances[o].saturating_sub(ev.amountOut);
                    Some(UpdateKind::Swap)
                }
                _ => None,
            }
        }
        LocalAMM::UniswapV4(_) => None,
    };

    Ok(kind.map(|kind| PoolUpdate {
        address,
        variant,
        kind,
    }))
}

/// Apply a Uniswap-V3-style log (Swap/Mint/Burn) to a V3 pool.
fn apply_v3(
    pool: &mut amms::amms::uniswap_v3::UniswapV3Pool,
    log: &Log,
    topic0: B256,
) -> Result<Option<UpdateKind>, EventError> {
    if topic0 == IUniswapV3Events::Swap::SIGNATURE_HASH {
        pool.sync(log)?;
        Ok(Some(UpdateKind::Swap))
    } else if topic0 == IUniswapV3Events::Mint::SIGNATURE_HASH
        || topic0 == IUniswapV3Events::Burn::SIGNATURE_HASH
    {
        // tickLower / tickUpper are the 2nd and 3rd indexed topics for both
        // Mint and Burn.
        let topics = log.topics();
        let tick_lower = topics.get(2).map(topic_to_i32).unwrap_or(0);
        let tick_upper = topics.get(3).map(topic_to_i32).unwrap_or(0);
        pool.sync(log)?;
        Ok(Some(UpdateKind::Liquidity {
            tick_lower,
            tick_upper,
        }))
    } else {
        Ok(None)
    }
}

/// Apply a Curve `TokenExchange` log to per-coin reserves.
///
/// This is an approximation: the event reports the trader's `tokens_bought`,
/// but Curve's internal `balances()` are also reduced by the admin-fee share,
/// and cryptoswap pools additionally re-scale prices on each trade. The applied
/// reserves therefore drift slightly from on-chain state over many swaps and
/// should be reconciled periodically via `cache_sync::refresh_curve_reserves`.
fn apply_curve(
    pool: &mut crate::curve_pool::CurvePool,
    log: &Log,
    topic0: B256,
) -> Result<Option<UpdateKind>, EventError> {
    if topic0 != ICurveStableEvents::TokenExchange::SIGNATURE_HASH
        && topic0 != ICurveCryptoEvents::TokenExchange::SIGNATURE_HASH
    {
        return Ok(None);
    }
    // Both the int128 and uint256 variants lay out data identically for the
    // first four words: sold_id, tokens_sold, bought_id, tokens_bought.
    let missing = || {
        EventError::Decode(alloy_sol_types::Error::Other(
            "TokenExchange: short data".into(),
        ))
    };
    let sold_id: usize = data_word(log, 0).ok_or_else(missing)?.saturating_to();
    let tokens_sold = data_word(log, 1).ok_or_else(missing)?;
    let bought_id: usize = data_word(log, 2).ok_or_else(missing)?.saturating_to();
    let tokens_bought = data_word(log, 3).ok_or_else(missing)?;

    let n = pool.reserves.len();
    if sold_id >= n || bought_id >= n {
        return Ok(None);
    }
    pool.reserves[sold_id] = pool.reserves[sold_id].saturating_add(tokens_sold);
    pool.reserves[bought_id] = pool.reserves[bought_id].saturating_sub(tokens_bought);
    Ok(Some(UpdateKind::Swap))
}

/// A set of pools that can be kept current from a stream of event logs.
///
/// The router owns each pool behind an [`AMMRef`] (`Arc<RwLock<LocalAMM>>`), so
/// it can be cloned cheaply and shared across tasks. Incoming logs are routed
/// by [`route_target`] and applied with [`apply_log`].
#[derive(Clone, Default)]
pub struct EventRouter {
    pools: HashMap<Address, AMMRef>,
}

impl EventRouter {
    /// Build a router over pools already wrapped in [`AMMRef`]s.
    ///
    /// Pools are re-keyed by each one's canonical on-chain address
    /// ([`AutomatedMarketMaker::address`]) so that log routing
    /// ([`route_target`]) and the lookup key always agree. This matters for
    /// Balancer, whose vault events identify the pool by id rather than by the
    /// caller's map key.
    pub fn new(pools: HashMap<Address, AMMRef>) -> Self {
        let pools = pools
            .into_values()
            .map(|amm_ref| {
                let address = amm_ref.read().expect("AMM lock poisoned").address();
                (address, amm_ref)
            })
            .collect();
        Self { pools }
    }

    /// Build a router from a map of owned pools, wrapping each in an [`AMMRef`]
    /// and keying by the pool's canonical address.
    pub fn from_amms(amms: HashMap<Address, LocalAMM>) -> Self {
        let pools = amms
            .into_values()
            .map(|amm| (amm.address(), AMMRef::new(amm.into())))
            .collect();
        Self { pools }
    }

    /// Build a router from the output of the configured-AMM loaders
    /// (`HashMap<Address, Option<LocalAMM>>`), dropping entries that failed to
    /// load and keying by the pool's canonical address.
    pub fn from_loaded(amms: HashMap<Address, Option<LocalAMM>>) -> Self {
        let pools = amms
            .into_values()
            .flatten()
            .map(|amm| (amm.address(), AMMRef::new(amm.into())))
            .collect();
        Self { pools }
    }

    /// The pools this router tracks, keyed by address.
    pub fn pools(&self) -> &HashMap<Address, AMMRef> {
        &self.pools
    }

    /// Number of pools tracked.
    pub fn len(&self) -> usize {
        self.pools.len()
    }

    /// Whether the router tracks no pools.
    pub fn is_empty(&self) -> bool {
        self.pools.is_empty()
    }

    /// The union of event `topic0`s across every tracked pool family.
    ///
    /// Pass these to a log subscription filter (e.g.
    /// `Filter::new().event_signature(router.subscription_topics())`).
    pub fn subscription_topics(&self) -> Vec<B256> {
        let mut topics: Vec<B256> = self
            .pools
            .values()
            .filter_map(|p| p.read().ok().map(|g| g.variant()))
            .flat_map(event_topics_for)
            .collect();
        topics.sort_unstable();
        topics.dedup();
        topics
    }

    /// Route a log to its pool and apply it in place.
    ///
    /// Returns `Ok(None)` if the log targets a pool this router does not track
    /// or is not relevant to that pool.
    pub fn apply(&self, log: &Log) -> Result<Option<PoolUpdate>, EventError> {
        let Some(target) = route_target(log) else {
            return Ok(None);
        };
        let Some(amm_ref) = self.pools.get(&target) else {
            return Ok(None);
        };
        let mut guard = amm_ref.write().expect("AMM lock poisoned");
        apply_log(&mut guard, log)
    }

    /// Apply a batch of logs, returning every successful update.
    ///
    /// Decode/apply errors for individual logs are skipped so one malformed log
    /// does not abort a whole block's worth of updates.
    pub fn apply_all(&self, logs: &[Log]) -> Vec<PoolUpdate> {
        logs.iter()
            .filter_map(|log| self.apply(log).ok().flatten())
            .collect()
    }

    /// Take an immutable, `Send + Sync` snapshot of all pool states.
    ///
    /// The returned map is decoupled from the router's locks, making it ideal
    /// for fully-offline parallel simulation (e.g. arbitrage search) while the
    /// router keeps applying new events.
    pub fn snapshot(&self) -> HashMap<Address, LocalAMM> {
        self.pools
            .iter()
            .filter_map(|(addr, amm_ref)| amm_ref.read().ok().map(|g| (*addr, g.clone())))
            .collect()
    }
}

/// Summary of a [`mirror_updates_to_cache`] call.
#[derive(Debug, Clone, Copy, Default)]
pub struct CacheMirrorSummary {
    /// V2 pools whose reserves were injected directly into the cache.
    pub v2_injected: usize,
    /// V3-style pools whose slot0/liquidity were injected directly.
    pub v3_injected: usize,
    /// V3-style pools whose tick data was re-injected after a Mint/Burn.
    pub ticks_injected: usize,
    /// Pools that fell back to a storage purge (Balancer/Curve/Solidly/etc.).
    pub purged: usize,
}

/// Push applied updates into an [`EvmCache`] so EVM-level reads stay consistent.
///
/// In-memory pool state is the source of truth for [`apply_log`]; this is only
/// needed when the same pools are also queried through the forked EVM (e.g.
/// `EvmCache::call_raw`). It reuses the crate's hot-state injection path:
///
/// - **V2 / V3 / Slipstream** state is written into the cache exactly, with no
///   RPC. After a Mint/Burn the (already-correct) in-memory tick map is
///   re-injected via [`inject_v3_tick_data`].
/// - **Balancer / Curve / ERC4626** state is invalidated (purged) so the next
///   EVM read lazily re-fetches it.
///
/// `observations` should be a single tracker threaded across calls so the cache
/// can reason about which slots change frequently.
pub fn mirror_updates_to_cache(
    cache: &mut EvmCache,
    router: &EventRouter,
    updates: &[PoolUpdate],
    observations: &mut SlotObservationTracker,
) -> CacheMirrorSummary {
    if updates.is_empty() {
        return CacheMirrorSummary::default();
    }

    let mut addresses: Vec<Address> = Vec::with_capacity(updates.len());
    let mut pending_tick_ranges: HashMap<Address, Vec<(i32, i32)>> = HashMap::new();
    for update in updates {
        if !addresses.contains(&update.address) {
            addresses.push(update.address);
        }
        if let UpdateKind::Liquidity {
            tick_lower,
            tick_upper,
        } = update.kind
            && (tick_lower != 0 || tick_upper != 0)
        {
            pending_tick_ranges
                .entry(update.address)
                .or_default()
                .push((tick_lower, tick_upper));
        }
    }

    let hot = inject_hot_state_to_evm(
        cache,
        router.pools(),
        &addresses,
        &pending_tick_ranges,
        observations,
    );

    // After a Mint/Burn the in-memory tick map already reflects the new
    // position, so mirror it into the cache exactly (no RPC resync). Skip pools
    // whose slot0/liquidity injection failed (and was purged) — re-injecting
    // ticks there would leave the tick map inconsistent with stale slot0 until
    // the next read repopulates it from RPC.
    let mut ticks_injected = 0usize;
    for update in updates {
        let UpdateKind::Liquidity {
            tick_lower,
            tick_upper,
        } = update.kind
        else {
            continue;
        };
        if tick_lower == 0 && tick_upper == 0 {
            continue; // ERC4626 etc. — no tick map.
        }
        if hot.injection_failures.contains(&update.address) {
            continue;
        }
        let Some(amm_ref) = router.pools().get(&update.address) else {
            continue;
        };
        let guard = amm_ref.read().expect("AMM lock poisoned");
        match &*guard {
            LocalAMM::UniswapV3(pool) => {
                inject_v3_tick_data(cache, update.address, pool, V3Flavor::UniswapV3);
                ticks_injected += 1;
            }
            LocalAMM::PancakeSwapV3(pool) => {
                inject_v3_tick_data(cache, update.address, pool, V3Flavor::PancakeSwapV3);
                ticks_injected += 1;
            }
            LocalAMM::Slipstream(slip) => {
                inject_v3_tick_data(
                    cache,
                    update.address,
                    &slip.as_v3_pool(),
                    V3Flavor::Slipstream,
                );
                ticks_injected += 1;
            }
            _ => {}
        }
    }

    CacheMirrorSummary {
        v2_injected: hot.v2_injected,
        v3_injected: hot.v3_injected,
        ticks_injected,
        purged: hot.injection_failures.len(),
    }
}
