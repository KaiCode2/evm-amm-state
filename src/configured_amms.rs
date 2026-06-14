//! TOML-driven AMM configuration loading and lazy V3 tick initialization.
//!
//! This module parses an `amms.toml` config into [`AmmConfigEntry`] records,
//! optionally filters them by token relevance, and initializes each entry into
//! a [`crate::amm_wrapper::LocalAMM`] backed by an [`evm_fork_cache::cache::EvmCache`].
//!
//! Two loading strategies are provided:
//! - [`load_configured_amms`] / [`load_configured_amms_from_entries`] fully
//!   initialize every pool (including the expensive V3 tick scan) in one call.
//! - [`load_configured_amms_lazy`] initializes V2/Balancer pools and V3
//!   metadata, deferring V3 tick prefetch into a [`DeferredV3Work`] handle that
//!   [`complete_deferred_v3_work`] finishes later. This lets a caller load only
//!   the pools it needs for the current cycle before paying the tick cost.

use alloy_primitives::{Address, B256, U256};
use serde::Deserialize;
use std::{
    collections::{HashMap, HashSet},
    path::Path,
    time::Instant,
};
use tracing::{debug, info, warn};

use crate::cache_sync::{
    V3BitmapPrefetchTarget, V3Flavor, V3InitPhase1Result, V3PrefetchStats,
    compute_adaptive_scan_params, incremental_sync_v3_ticks, init_balancer_from_cache,
    init_uniswap_v2_from_cache, init_v3_phase1, inject_v3_tick_data, prefetch_accounts,
    prefetch_v3_bitmap_slots, prefetch_v3_incremental_resync_slots, prefetch_v3_tick_info_slots,
    save_v3_tick_snapshot, sync_uniswap_v3_ticks,
};
use crate::progress::{finish_with_message, progress_bar};
use crate::{
    amm_wrapper::LocalAMM, balancer_pool::BalancerPool, balancer_v3_pool::BalancerV3PoolType,
    slipstream_pool::SlipstreamPool,
};
use evm_fork_cache::cache::EvmCache;

/// Wrapper function type for converting a V3 pool into the appropriate LocalAMM variant.
/// Uses Box<dyn Fn> because Slipstream wrappers capture tick_spacing.
type WrapperFn = Box<dyn Fn(amms::amms::uniswap_v3::UniswapV3Pool) -> LocalAMM>;
type DeferredV3Resync = (
    Address,
    amms::amms::uniswap_v3::UniswapV3Pool,
    V3Flavor,
    WrapperFn,
);
type DeferredV3Incremental = (
    Address,
    amms::amms::uniswap_v3::UniswapV3Pool,
    V3Flavor,
    std::collections::HashMap<i16, alloy_primitives::U256>,
    std::collections::HashMap<i32, amms::amms::uniswap_v3::Info>,
    WrapperFn,
);

/// V3 pools whose tick data was deferred during lazy AMM loading.
/// Call `complete_deferred_v3_work()` to finish their initialization.
pub struct DeferredV3Work {
    needs_resync: Vec<DeferredV3Resync>,
    needs_incremental: Vec<DeferredV3Incremental>,
}

impl Default for DeferredV3Work {
    fn default() -> Self {
        Self::new()
    }
}

impl DeferredV3Work {
    /// Create an empty DeferredV3Work.
    pub fn new() -> Self {
        Self {
            needs_resync: Vec::new(),
            needs_incremental: Vec::new(),
        }
    }

    /// Returns true if there's no deferred V3 work to complete.
    pub fn is_empty(&self) -> bool {
        self.needs_resync.is_empty() && self.needs_incremental.is_empty()
    }

    /// Returns the total number of deferred V3 pools.
    pub fn len(&self) -> usize {
        self.needs_resync.len() + self.needs_incremental.len()
    }

    /// Add a V3 pool that needs full bitmap resync (cold start).
    pub fn push_resync(
        &mut self,
        address: Address,
        pool: amms::amms::uniswap_v3::UniswapV3Pool,
        flavor: V3Flavor,
        wrapper: impl Fn(amms::amms::uniswap_v3::UniswapV3Pool) -> LocalAMM + 'static,
    ) {
        self.needs_resync
            .push((address, pool, flavor, Box::new(wrapper)));
    }

    /// Add a V3 pool that needs incremental resync (stale snapshot).
    pub fn push_incremental(
        &mut self,
        address: Address,
        pool: amms::amms::uniswap_v3::UniswapV3Pool,
        flavor: V3Flavor,
        old_bitmap: std::collections::HashMap<i16, alloy_primitives::U256>,
        old_ticks: std::collections::HashMap<i32, amms::amms::uniswap_v3::Info>,
        wrapper: impl Fn(amms::amms::uniswap_v3::UniswapV3Pool) -> LocalAMM + 'static,
    ) {
        self.needs_incremental.push((
            address,
            pool,
            flavor,
            old_bitmap,
            old_ticks,
            Box::new(wrapper),
        ));
    }
}

fn is_non_archive_state_error(message: &str) -> bool {
    let normalized = message.to_ascii_lowercase();
    normalized.contains("missing trie node")
        || normalized.contains("non-archive node")
        || normalized.contains("state ") && normalized.contains("is not available, not found")
}

fn v3_prefetch_skip_threshold(total_requested: usize) -> usize {
    if total_requested > 0 {
        (total_requested * 3 / 4).max(200)
    } else {
        200
    }
}

fn v3_prefetch_skip_reason(stats: &V3PrefetchStats, address: Address) -> Option<&'static str> {
    let errors = stats.errors_by_pool.get(&address).copied().unwrap_or(0);
    if errors == 0 {
        return None;
    }

    if stats
        .error_samples_by_pool
        .get(&address)
        .is_some_and(|sample| is_non_archive_state_error(sample))
    {
        return Some("required historical storage is unavailable on the current RPC");
    }

    let total_requested = stats
        .total_requested_by_pool
        .get(&address)
        .copied()
        .unwrap_or(0);
    if errors > v3_prefetch_skip_threshold(total_requested) {
        return Some("too many tick slots failed prefetch and would trigger serial RPC fallback");
    }

    None
}

/// AMM family for a configured pool entry.
///
/// Deserialized from the TOML `type` field using snake_case names
/// (e.g. `uniswap_v2`, `pancake_swap_v3`, `balancer_v3`). The variant selects
/// which loader is used to initialize the pool and which [`crate::amm_wrapper::LocalAMM`]
/// variant the entry becomes.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AmmType {
    UniswapV2,
    UniswapV3,
    PancakeSwapV3,
    Balancer,
    BalancerV3,
    Curve,
    SolidlyV2,
    Slipstream,
    UniswapV4,
}

/// A single AMM entry parsed from the `amms.toml` config.
///
/// `kind` and `address` are always required; the remaining fields are optional
/// and only consulted for the AMM families that need them (e.g. `pool_id` for
/// Balancer V2, `tick_spacing` for Slipstream/Uniswap V4, `stable` and
/// `factory_address` for Solidly V2, `curve_use_uint256` for Curve). Fields
/// irrelevant to a given `kind` are ignored.
#[derive(Debug, Clone, Deserialize)]
pub struct AmmConfigEntry {
    #[serde(rename = "type")]
    pub kind: AmmType,
    pub address: Address,
    #[serde(default)]
    pub tokens: Vec<Address>,
    #[serde(default)]
    pub fee_tier: Option<u32>,
    #[serde(default)]
    pub vault_address: Option<Address>,
    #[serde(default)]
    pub pool_id: Option<B256>,
    /// Tick spacing for Slipstream and UniswapV4 pools.
    #[serde(default)]
    pub tick_spacing: Option<i32>,
    /// Whether this is a stable (true) or volatile (false) pool (SolidlyV2).
    #[serde(default)]
    pub stable: Option<bool>,
    /// Factory address for SolidlyV2/Slipstream pools.
    #[serde(default)]
    pub factory_address: Option<Address>,
    /// Hooks address for UniswapV4 pools.
    #[serde(default)]
    pub hooks: Option<Address>,
    /// `false` = stableswap (int128 indices), `true` = cryptoswap (uint256 indices).
    #[serde(default)]
    pub curve_use_uint256: Option<bool>,
    /// Balancer V3 pool type hint: `stable` or `weighted`.
    /// When set, avoids trial-and-error RPC calls to detect pool type.
    /// Falls back to auto-detection if omitted.
    #[serde(default)]
    pub balancer_v3_pool_type: Option<BalancerV3PoolType>,
}

#[derive(Debug, Deserialize)]
struct AmmConfigFile {
    amms: HashMap<String, Vec<AmmConfigEntry>>,
}

/// Filters AMM config entries to only include those that involve at least one of the active tokens.
///
/// An AMM is included if ANY of its tokens appear in the `active_tokens` set.
/// This is a conservative approach that ensures we don't accidentally filter out
/// pools needed for triangular arbitrage routing.
///
/// Returns the filtered entries and the count of filtered-out entries.
pub fn filter_amm_entries_by_tokens(
    entries: &[AmmConfigEntry],
    active_tokens: &HashSet<Address>,
) -> (Vec<AmmConfigEntry>, usize) {
    if active_tokens.is_empty() {
        // If no active tokens specified, include all AMMs (conservative)
        return (entries.to_vec(), 0);
    }

    let mut included = Vec::with_capacity(entries.len());
    let mut filtered_count = 0;

    for entry in entries {
        // Check if any of the AMM's tokens are in the active set
        let has_active_token = entry.tokens.iter().any(|t| active_tokens.contains(t));

        if has_active_token || entry.tokens.is_empty() {
            // Include AMMs with active tokens or AMMs without token info (conservative)
            included.push(entry.clone());
        } else {
            filtered_count += 1;
            debug!(
                amm = %entry.address,
                amm_tokens = ?entry.tokens,
                "filtered out AMM (no token overlap with active strategies)"
            );
        }
    }

    (included, filtered_count)
}

/// Filters AMM config entries for swap routing: only pools that can form a valid routing leg.
///
/// A pool is included if it contains at least 2 tokens from the `swap_tokens` set.
/// For a swap from A→B via WETH, `swap_tokens` should be `{A, B, WETH}`.
/// This ensures we only load pools that can actually participate in direct or 2-leg routes,
/// unlike `filter_amm_entries_by_tokens` which includes any pool with just 1 matching token.
///
/// Returns the filtered entries and the count of filtered-out entries.
pub fn filter_amm_entries_for_swap(
    entries: &[AmmConfigEntry],
    swap_tokens: &HashSet<Address>,
) -> (Vec<AmmConfigEntry>, usize) {
    if swap_tokens.len() < 2 {
        // Need at least 2 tokens for a swap pair filter to make sense
        return filter_amm_entries_by_tokens(entries, swap_tokens);
    }

    let mut included = Vec::with_capacity(entries.len());
    let mut filtered_count = 0;

    for entry in entries {
        let overlap_count = entry
            .tokens
            .iter()
            .filter(|t| swap_tokens.contains(*t))
            .count();

        if overlap_count >= 2 || entry.tokens.is_empty() {
            // Pool has at least 2 swap-relevant tokens (valid routing leg),
            // or has no token info (conservative — include to avoid missing pools)
            included.push(entry.clone());
        } else {
            filtered_count += 1;
            debug!(
                amm = %entry.address,
                amm_tokens = ?entry.tokens,
                overlap = overlap_count,
                "filtered out AMM (insufficient token overlap for swap routing)"
            );
        }
    }

    (included, filtered_count)
}

/// Collects all unique tokens from a list of strategy token sets.
///
/// Used to build the active token set for AMM filtering.
pub fn collect_active_tokens<I, T>(strategy_tokens: I) -> HashSet<Address>
where
    I: IntoIterator<Item = T>,
    T: IntoIterator<Item = Address>,
{
    strategy_tokens
        .into_iter()
        .flat_map(|tokens| tokens.into_iter())
        .collect()
}

/// Result of `build_strategy_amm_ownership`: which AMMs each owner "owns",
/// and which AMMs are unclaimed by any owner (treated as shared pools).
#[derive(Debug, Default, Clone)]
pub struct StrategyAmmOwnership {
    /// Owner address -> set of AMM addresses whose tokens overlap with the
    /// owner's token set.
    pub owned_by_strategy: HashMap<Address, HashSet<Address>>,
    /// AMMs claimed by no owner. Always loaded when any owner is active.
    pub unclaimed: HashSet<Address>,
}

impl StrategyAmmOwnership {
    /// Compute the active AMM set for a cycle: union of AMMs owned by any
    /// `ready` owner plus all unclaimed/shared AMMs.
    pub fn active_amms(&self, ready: &[Address]) -> HashSet<Address> {
        let mut out: HashSet<Address> = self.unclaimed.clone();
        for strategy in ready {
            if let Some(set) = self.owned_by_strategy.get(strategy) {
                out.extend(set.iter().copied());
            }
        }
        out
    }
}

/// Build an owner->AMM ownership map from token-owner pairs and AMM entries.
///
/// An owner "owns" an AMM when the AMM's `tokens` overlap with the owner's
/// token set. AMMs claimed by no owner go into the `unclaimed` set.
///
/// The input is intentionally generic so callers can use strategy configs,
/// token universes, routing groups, or any other address-keyed owner model.
pub fn build_strategy_amm_ownership<I, T>(
    owners: I,
    entries: &[AmmConfigEntry],
) -> StrategyAmmOwnership
where
    I: IntoIterator<Item = (Address, T)>,
    T: IntoIterator<Item = Address>,
{
    let mut owned_by_strategy: HashMap<Address, HashSet<Address>> = HashMap::new();
    let mut claimed: HashSet<Address> = HashSet::new();

    for (owner, tokens) in owners {
        let involved: HashSet<Address> = tokens.into_iter().collect();
        let mut owned: HashSet<Address> = HashSet::new();
        if !involved.is_empty() {
            for entry in entries {
                if entry.tokens.iter().any(|t| involved.contains(t)) {
                    owned.insert(entry.address);
                }
            }
        }
        if !owned.is_empty() {
            claimed.extend(owned.iter().copied());
        }
        owned_by_strategy.insert(owner, owned);
    }

    let unclaimed: HashSet<Address> = entries
        .iter()
        .map(|e| e.address)
        .filter(|addr| !claimed.contains(addr))
        .collect();

    StrategyAmmOwnership {
        owned_by_strategy,
        unclaimed,
    }
}

/// Loads AMM config entries from a TOML file for a specific chain.
///
/// This function just parses the config without initializing the AMMs,
/// allowing for filtering before the expensive RPC calls.
pub fn load_amm_config_entries(
    chain_name: &str,
    file_path: Option<&Path>,
) -> anyhow::Result<Vec<AmmConfigEntry>> {
    let Some(path) = file_path else {
        return Ok(Vec::new());
    };

    let content = std::fs::read_to_string(path)?;
    let parsed: AmmConfigFile = toml::from_str(&content)?;

    Ok(parsed.amms.get(chain_name).cloned().unwrap_or_default())
}

/// Load and fully initialize every AMM configured for `chain_name`.
///
/// Reads entries from the TOML file at `file_path` (returns an empty map when
/// `file_path` is `None`), then initializes each pool eagerly — including the
/// expensive V3 tick scan — via [`load_configured_amms_from_entries`].
/// `default_balancer_vault` is used for Balancer entries that omit an explicit
/// `vault_address`. Entries that fail to load are recorded as `None`.
///
/// Use [`load_configured_amms_lazy`] instead when you want to defer V3 tick
/// work until after filtering down to the pools you actually need.
pub async fn load_configured_amms(
    cache: &mut EvmCache,
    chain_name: &str,
    default_balancer_vault: Address,
    file_path: Option<&Path>,
) -> anyhow::Result<HashMap<Address, Option<LocalAMM>>> {
    let entries = load_amm_config_entries(chain_name, file_path)?;
    load_configured_amms_from_entries(cache, &entries, default_balancer_vault).await
}

/// Loads configured AMMs from pre-filtered entries.
///
/// Use this when you've already filtered the entries (e.g., by active strategy tokens)
/// to avoid loading unnecessary AMMs. Fully initializes all pools including V3 tick data.
pub async fn load_configured_amms_from_entries(
    cache: &mut EvmCache,
    entries: &[AmmConfigEntry],
    default_balancer_vault: Address,
) -> anyhow::Result<HashMap<Address, Option<LocalAMM>>> {
    let (mut amms, deferred) =
        init_amms_phase1_phase2(cache, entries, default_balancer_vault).await?;
    complete_deferred_v3_work(cache, deferred, &mut amms).await?;
    Ok(amms)
}

/// Load AMMs with deferred V3 tick initialization.
///
/// V2 and Balancer pools are fully initialized. V3 pools that have
/// preloaded storage (cache hit) are also fully initialized. V3 pools
/// needing resync are returned in `DeferredV3Work` — call
/// `complete_deferred_v3_work()` to finish them before simulation.
pub async fn load_configured_amms_lazy(
    cache: &mut EvmCache,
    chain_name: &str,
    default_balancer_vault: Address,
    file_path: Option<&Path>,
) -> anyhow::Result<(HashMap<Address, Option<LocalAMM>>, DeferredV3Work)> {
    let entries = load_amm_config_entries(chain_name, file_path)?;
    init_amms_phase1_phase2(cache, &entries, default_balancer_vault).await
}

/// Phase 1+2: Prefetch accounts and initialize AMMs (V2/Balancer fully, V3 metadata only).
///
/// V3 pools needing tick resync are returned in `DeferredV3Work` for later completion
/// via `complete_deferred_v3_work()`.
async fn init_amms_phase1_phase2(
    cache: &mut EvmCache,
    entries: &[AmmConfigEntry],
    default_balancer_vault: Address,
) -> anyhow::Result<(HashMap<Address, Option<LocalAMM>>, DeferredV3Work)> {
    if entries.is_empty() {
        return Ok((
            HashMap::new(),
            DeferredV3Work {
                needs_resync: Vec::new(),
                needs_incremental: Vec::new(),
            },
        ));
    }

    let total_start = Instant::now();

    info!(count = entries.len(), "loading configured AMMs");

    // Phase 1: Prefetch all accounts in parallel
    let balancer_entries: Vec<(B256, Address)> = entries
        .iter()
        .filter_map(|e| match e.kind {
            AmmType::Balancer => e
                .pool_id
                .map(|pid| (pid, e.vault_address.unwrap_or(default_balancer_vault))),
            _ => None,
        })
        .collect();

    let prefetch_start = Instant::now();

    let mut all_addresses: Vec<Address> = Vec::new();
    let mut seen: HashSet<Address> = HashSet::new();
    for entry in entries.iter() {
        match entry.kind {
            AmmType::UniswapV2
            | AmmType::UniswapV3
            | AmmType::PancakeSwapV3
            | AmmType::BalancerV3
            | AmmType::Curve
            | AmmType::SolidlyV2
            | AmmType::Slipstream
            | AmmType::UniswapV4 => {
                if seen.insert(entry.address) {
                    all_addresses.push(entry.address);
                }
            }
            AmmType::Balancer => {} // handled via balancer_entries below
        }
    }
    for (pool_id, vault) in &balancer_entries {
        let pool_addr = BalancerPool::address(*pool_id);
        if seen.insert(pool_addr) {
            all_addresses.push(pool_addr);
        }
        if seen.insert(*vault) {
            all_addresses.push(*vault);
        }
    }

    debug!(
        total_addresses = all_addresses.len(),
        "prefetching all pool accounts"
    );
    prefetch_accounts(cache, &all_addresses).await?;

    let total_prefetch_ms = prefetch_start.elapsed().as_millis();
    debug!(total_prefetch_ms, "prefetch phase complete");

    // Purge Balancer vault storage once before the init loop.
    // The vault is a shared contract — purging per-pool would wipe data
    // that was just fetched by the previous pool's init.
    {
        let mut purged_vaults: HashSet<Address> = HashSet::new();
        for (_, vault) in &balancer_entries {
            if purged_vaults.insert(*vault) {
                cache.purge_pool_storage(*vault);
            }
        }
    }

    // Phase 2: Initialize AMMs — V2/Balancer fully, V3 phase1 only (determines cache status)
    let init_start = Instant::now();
    let init_pb = progress_bar(entries.len() as u64, "Initializing AMMs");
    let mut amms = HashMap::new();
    let mut v3_needs_resync: Vec<DeferredV3Resync> = Vec::new();
    let mut v3_needs_incremental: Vec<DeferredV3Incremental> = Vec::new();

    for entry in entries.iter() {
        let address = entry.address;
        init_pb.set_message(format!("{:.8}...", address));

        match entry.kind {
            AmmType::UniswapV2 => {
                let fee: usize = entry.fee_tier.unwrap_or(300).try_into().unwrap();
                match init_uniswap_v2_from_cache(cache, address, fee).await {
                    Ok(pool) => {
                        amms.insert(address, Some(LocalAMM::UniswapV2(pool)));
                    }
                    Err(e) => {
                        warn!("Failed to load V2 AMM {:?}: {:?}", address, e);
                        amms.insert(address, None);
                    }
                }
            }
            AmmType::UniswapV3 | AmmType::PancakeSwapV3 => {
                let is_pancake = matches!(entry.kind, AmmType::PancakeSwapV3);
                let flavor = if is_pancake {
                    V3Flavor::PancakeSwapV3
                } else {
                    V3Flavor::UniswapV3
                };
                let wrapper: WrapperFn = if is_pancake {
                    Box::new(LocalAMM::PancakeSwapV3)
                } else {
                    Box::new(LocalAMM::UniswapV3)
                };

                match init_v3_phase1(cache, address, flavor).await {
                    Ok(V3InitPhase1Result::Complete(pool)) => {
                        amms.insert(address, Some(wrapper(pool)));
                    }
                    Ok(V3InitPhase1Result::NeedsResync { pool, flavor }) => {
                        v3_needs_resync.push((address, pool, flavor, wrapper));
                    }
                    Ok(V3InitPhase1Result::NeedsIncrementalResync {
                        pool,
                        flavor,
                        old_bitmap,
                        old_ticks,
                    }) => {
                        v3_needs_incremental
                            .push((address, pool, flavor, old_bitmap, old_ticks, wrapper));
                    }
                    Err(e) => {
                        warn!("Failed to load V3 AMM {:?}: {:?}", address, e);
                        amms.insert(address, None);
                    }
                }
            }
            AmmType::Balancer => {
                let pool_id = entry.pool_id.ok_or_else(|| {
                    anyhow::anyhow!("Missing pool_id for Balancer AMM {address:?}")
                })?;
                let vault_address = entry.vault_address.unwrap_or(default_balancer_vault);

                match init_balancer_from_cache(cache, pool_id, vault_address).await {
                    Ok(pool) => {
                        amms.insert(address, Some(LocalAMM::Balancer(pool)));
                    }
                    Err(e) => {
                        warn!("Failed to load Balancer AMM {:?}: {:?}", address, e);
                        amms.insert(address, None);
                    }
                }
            }
            AmmType::BalancerV3 => {
                let tokens = &entry.tokens;
                let vault = entry.vault_address.unwrap_or(Address::ZERO);
                let token_a = tokens.first().copied().unwrap_or(Address::ZERO);
                let token_b = tokens.get(1).copied().unwrap_or(Address::ZERO);
                match crate::cache_sync::init_balancer_v3_from_cache(
                    cache,
                    address,
                    vault,
                    token_a,
                    token_b,
                    entry.balancer_v3_pool_type,
                )
                .await
                {
                    Ok(pool) => {
                        amms.insert(address, Some(LocalAMM::BalancerV3(pool)));
                    }
                    Err(e) => {
                        warn!("Failed to load BalancerV3 AMM {:?}: {:?}", address, e);
                        amms.insert(address, None);
                    }
                }
            }
            AmmType::Curve => {
                let use_uint256 = entry.curve_use_uint256.unwrap_or(false);
                match crate::cache_sync::init_curve_from_cache(
                    cache,
                    address,
                    &entry.tokens,
                    use_uint256,
                )
                .await
                {
                    Ok(pool) => {
                        amms.insert(address, Some(LocalAMM::Curve(pool)));
                    }
                    Err(e) => {
                        warn!("Failed to load Curve AMM {:?}: {:?}", address, e);
                        amms.insert(address, None);
                    }
                }
            }
            AmmType::SolidlyV2 => {
                match crate::cache_sync::init_solidly_v2_from_cache(
                    cache,
                    address,
                    entry.stable.unwrap_or(false),
                    entry.factory_address.unwrap_or(Address::ZERO),
                    entry.fee_tier.unwrap_or(30),
                )
                .await
                {
                    Ok(pool) => {
                        amms.insert(address, Some(LocalAMM::SolidlyV2(pool)));
                    }
                    Err(e) => {
                        warn!("Failed to load SolidlyV2 AMM {:?}: {:?}", address, e);
                        amms.insert(address, None);
                    }
                }
            }
            AmmType::Slipstream => {
                match crate::cache_sync::init_slipstream_from_cache(
                    cache,
                    address,
                    entry.tick_spacing.unwrap_or(1),
                )
                .await
                {
                    Ok(pool) => {
                        // Slipstream pools need tick data — defer to phase 3
                        // if they have no bitmap data yet
                        if pool.tick_bitmap.is_empty() {
                            let ts = entry.tick_spacing.unwrap_or(1);
                            v3_needs_resync.push((
                                address,
                                pool.as_v3_pool(),
                                V3Flavor::Slipstream,
                                Box::new(move |p| {
                                    LocalAMM::Slipstream(SlipstreamPool::from_v3_pool(p, ts))
                                }),
                            ));
                        } else {
                            amms.insert(address, Some(LocalAMM::Slipstream(pool)));
                        }
                    }
                    Err(e) => {
                        warn!("Failed to load Slipstream AMM {:?}: {:?}", address, e);
                        amms.insert(address, None);
                    }
                }
            }
            AmmType::UniswapV4 => {
                use crate::uniswap_v4_pool::UniswapV4Pool;
                let tokens = &entry.tokens;
                let pool = UniswapV4Pool {
                    address,
                    currency0: tokens.first().copied().unwrap_or(Address::ZERO),
                    currency1: tokens.get(1).copied().unwrap_or(Address::ZERO),
                    fee: entry.fee_tier.unwrap_or(3000),
                    tick_spacing: entry.tick_spacing.unwrap_or(60),
                    hooks: entry.hooks.unwrap_or(Address::ZERO),
                    tick: 0,
                    sqrt_price: U256::ZERO,
                    liquidity: 0,
                };
                amms.insert(address, Some(LocalAMM::UniswapV4(pool)));
            }
        };

        init_pb.inc(1);
    }

    finish_with_message(
        &init_pb,
        &format!(
            "{} loaded, {} deferred resync, {} deferred incremental, {} failed",
            amms.values().filter(|opt| opt.is_some()).count(),
            v3_needs_resync.len(),
            v3_needs_incremental.len(),
            amms.values().filter(|opt| opt.is_none()).count(),
        ),
    );

    let total_ms = total_start.elapsed().as_millis();
    let loaded_count = amms.values().filter(|opt| opt.is_some()).count();
    debug!(
        loaded = loaded_count,
        failed = amms.len() - loaded_count,
        deferred_resync = v3_needs_resync.len(),
        deferred_incremental = v3_needs_incremental.len(),
        init_ms = init_start.elapsed().as_millis(),
        total_prefetch_ms = total_prefetch_ms,
        total_ms = total_ms,
        "AMM init phase 1+2 complete"
    );

    let deferred = DeferredV3Work {
        needs_resync: v3_needs_resync,
        needs_incremental: v3_needs_incremental,
    };

    Ok((amms, deferred))
}

/// Complete deferred V3 tick initialization.
///
/// This runs the expensive bitmap prefetch and tick resync for V3 pools
/// that were deferred during `load_configured_amms_lazy()`.
pub async fn complete_deferred_v3_work(
    cache: &mut EvmCache,
    deferred: DeferredV3Work,
    amms: &mut HashMap<Address, Option<LocalAMM>>,
) -> anyhow::Result<()> {
    // Phase 3a: Parallel bitmap prefetch + resync for V3 pools with no snapshot (cold start)
    let mut v3_resync_ms = 0u128;
    if !deferred.needs_resync.is_empty() {
        let bitmap_start = Instant::now();

        let targets: Vec<V3BitmapPrefetchTarget> = deferred
            .needs_resync
            .iter()
            .map(|(addr, pool, flavor, _)| {
                let scan_params = compute_adaptive_scan_params(pool.liquidity, pool.tick_spacing);
                V3BitmapPrefetchTarget {
                    address: *addr,
                    flavor: *flavor,
                    tick_spacing: pool.tick_spacing,
                    center_tick: pool.tick,
                    max_scan_words: scan_params.max_scan_words,
                    empty_word_threshold: scan_params.empty_word_threshold,
                }
            })
            .collect();

        prefetch_v3_bitmap_slots(cache, &targets).await?;
        let bitmap_ms = bitmap_start.elapsed().as_millis();
        debug!(
            bitmap_prefetch_ms = bitmap_ms,
            "bitmap prefetch phase complete (deferred)"
        );

        // Prefetch tick info slots (reads cached bitmaps, then parallel-fetches tick data)
        let tick_info_start = Instant::now();
        let prefetch_stats = prefetch_v3_tick_info_slots(cache, &targets).await?;
        let tick_info_ms = tick_info_start.elapsed().as_millis();
        debug!(
            tick_info_prefetch_ms = tick_info_ms,
            "tick info prefetch phase complete (deferred)"
        );

        // Complete V3 resync (bitmap + tick info reads now hit cache — very fast)
        let resync_start = Instant::now();
        let resync_pb = progress_bar(
            deferred.needs_resync.len() as u64,
            "Completing deferred V3 tick resync",
        );

        for (address, mut pool, flavor, wrapper) in deferred.needs_resync {
            resync_pb.set_message(format!("{:.8}...", address));
            let pool_start = Instant::now();

            if let Some(reason) = v3_prefetch_skip_reason(&prefetch_stats, address) {
                let errors = prefetch_stats
                    .errors_by_pool
                    .get(&address)
                    .copied()
                    .unwrap_or(0);
                let total_requested = prefetch_stats
                    .total_requested_by_pool
                    .get(&address)
                    .copied()
                    .unwrap_or(0);
                warn!(
                    pool = %address,
                    failed_slots = errors,
                    total_slots = total_requested,
                    failure_pct = format!(
                        "{:.0}%",
                        errors as f64 / total_requested.max(1) as f64 * 100.0
                    ),
                    reason,
                    "Skipping pool during deferred V3 resync"
                );
                amms.insert(address, None);
                resync_pb.inc(1);
                continue;
            }

            match sync_uniswap_v3_ticks(cache, &mut pool, flavor) {
                Ok(()) => {
                    save_v3_tick_snapshot(cache, &pool);
                    let elapsed = pool_start.elapsed();
                    debug!(
                        pool = %address,
                        elapsed_ms = elapsed.as_millis(),
                        "V3 pool resync complete (deferred)"
                    );
                    amms.insert(address, Some(wrapper(pool)));
                }
                Err(e) => {
                    warn!("Failed to resync V3 ticks for {:?}: {:?}", address, e);
                    amms.insert(address, None);
                }
            }
            resync_pb.inc(1);
        }

        v3_resync_ms = resync_start.elapsed().as_millis();
        finish_with_message(&resync_pb, "Deferred V3 resync complete");
    }

    // Phase 3b: Parallel prefetch + incremental resync for V3 pools with stale snapshots
    let mut v3_incremental_ms = 0u128;
    if !deferred.needs_incremental.is_empty() {
        let incr_start = Instant::now();

        // Build prefetch targets: (address, flavor, tick_spacing, current_tick, liquidity, &old_bitmap)
        let prefetch_targets: Vec<_> = deferred
            .needs_incremental
            .iter()
            .map(|(addr, pool, flavor, old_bitmap, _, _)| {
                (
                    *addr,
                    *flavor,
                    pool.tick_spacing,
                    pool.tick,
                    pool.liquidity,
                    old_bitmap as &std::collections::HashMap<i16, alloy_primitives::U256>,
                )
            })
            .collect();

        let prefetch_stats = prefetch_v3_incremental_resync_slots(cache, &prefetch_targets).await?;
        let prefetch_ms = incr_start.elapsed().as_millis();
        debug!(
            incremental_prefetch_ms = prefetch_ms,
            pools = deferred.needs_incremental.len(),
            pools_with_errors = prefetch_stats.errors_by_pool.len(),
            "incremental resync prefetch phase complete (deferred)"
        );

        // Run incremental resync with pre_purged=true (all reads hit cache)
        let resync_start = Instant::now();
        let resync_pb = progress_bar(
            deferred.needs_incremental.len() as u64,
            "Completing deferred V3 incremental resync",
        );

        for (address, mut pool, flavor, old_bitmap, old_ticks, wrapper) in
            deferred.needs_incremental
        {
            if let Some(reason) = v3_prefetch_skip_reason(&prefetch_stats, address) {
                let errors = prefetch_stats
                    .errors_by_pool
                    .get(&address)
                    .copied()
                    .unwrap_or(0);
                let total_requested = prefetch_stats
                    .total_requested_by_pool
                    .get(&address)
                    .copied()
                    .unwrap_or(0);
                warn!(
                    pool = %address,
                    failed_slots = errors,
                    total_slots = total_requested,
                    failure_pct = format!(
                        "{:.0}%",
                        errors as f64 / total_requested.max(1) as f64 * 100.0
                    ),
                    reason,
                    "Skipping pool during deferred V3 incremental resync"
                );
                amms.insert(address, None);
                resync_pb.inc(1);
                continue;
            }

            resync_pb.set_message(format!("{:.8}...", address));
            let pool_start = Instant::now();

            match incremental_sync_v3_ticks(cache, &mut pool, &old_bitmap, &old_ticks, flavor, true)
            {
                Ok(()) => {
                    inject_v3_tick_data(cache, address, &pool, flavor);
                    let elapsed = pool_start.elapsed();
                    debug!(
                        pool = %address,
                        elapsed_ms = elapsed.as_millis(),
                        "V3 pool incremental resync complete (deferred)"
                    );
                    amms.insert(address, Some(wrapper(pool)));
                }
                Err(e) => {
                    warn!("Failed incremental resync for V3 {:?}: {:?}", address, e);
                    amms.insert(address, None);
                }
            }
            resync_pb.inc(1);
        }

        v3_incremental_ms = resync_start.elapsed().as_millis();
        finish_with_message(&resync_pb, "Deferred V3 incremental resync complete");
    }

    debug!(
        v3_resync_ms,
        v3_incremental_ms, "complete_deferred_v3_work done"
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_entry(addr_byte: u8, token_bytes: &[u8]) -> AmmConfigEntry {
        AmmConfigEntry {
            kind: AmmType::UniswapV2,
            address: Address::repeat_byte(addr_byte),
            tokens: token_bytes
                .iter()
                .map(|b| Address::repeat_byte(*b))
                .collect(),
            fee_tier: None,
            vault_address: None,
            pool_id: None,
            tick_spacing: None,
            stable: None,
            factory_address: None,
            hooks: None,
            curve_use_uint256: None,
            balancer_v3_pool_type: None,
        }
    }

    #[test]
    fn test_filter_empty_active_tokens_includes_all() {
        let entries = vec![
            make_entry(0x01, &[0xAA, 0xBB]),
            make_entry(0x02, &[0xCC, 0xDD]),
        ];
        let active = HashSet::new();

        let (filtered, count) = filter_amm_entries_by_tokens(&entries, &active);

        assert_eq!(filtered.len(), 2);
        assert_eq!(count, 0);
    }

    #[test]
    fn test_filter_with_overlap_includes_matching() {
        let entries = vec![
            make_entry(0x01, &[0xAA, 0xBB]), // Has 0xAA
            make_entry(0x02, &[0xCC, 0xDD]), // No overlap
            make_entry(0x03, &[0xAA, 0xCC]), // Has 0xAA
        ];
        let active: HashSet<Address> = [Address::repeat_byte(0xAA)].into_iter().collect();

        let (filtered, count) = filter_amm_entries_by_tokens(&entries, &active);

        assert_eq!(filtered.len(), 2);
        assert_eq!(count, 1);
        assert!(
            filtered
                .iter()
                .any(|e| e.address == Address::repeat_byte(0x01))
        );
        assert!(
            filtered
                .iter()
                .any(|e| e.address == Address::repeat_byte(0x03))
        );
    }

    #[test]
    fn test_filter_no_overlap_excludes_all() {
        let entries = vec![
            make_entry(0x01, &[0xAA, 0xBB]),
            make_entry(0x02, &[0xCC, 0xDD]),
        ];
        let active: HashSet<Address> = [Address::repeat_byte(0xFF)].into_iter().collect();

        let (filtered, count) = filter_amm_entries_by_tokens(&entries, &active);

        assert_eq!(filtered.len(), 0);
        assert_eq!(count, 2);
    }

    #[test]
    fn test_filter_empty_token_list_always_included() {
        // AMMs without token info should always be included (conservative)
        let entries = vec![
            make_entry(0x01, &[]),           // Empty tokens
            make_entry(0x02, &[0xAA, 0xBB]), // Has tokens but no overlap
        ];
        let active: HashSet<Address> = [Address::repeat_byte(0xFF)].into_iter().collect();

        let (filtered, count) = filter_amm_entries_by_tokens(&entries, &active);

        assert_eq!(filtered.len(), 1); // Only the empty-tokens entry
        assert_eq!(count, 1);
        assert_eq!(filtered[0].address, Address::repeat_byte(0x01));
    }

    #[test]
    fn test_filter_multiple_active_tokens() {
        let entries = vec![
            make_entry(0x01, &[0xAA, 0xBB]),
            make_entry(0x02, &[0xCC, 0xDD]),
            make_entry(0x03, &[0xEE, 0xFF]),
        ];
        let active: HashSet<Address> = [Address::repeat_byte(0xAA), Address::repeat_byte(0xDD)]
            .into_iter()
            .collect();

        let (filtered, count) = filter_amm_entries_by_tokens(&entries, &active);

        assert_eq!(filtered.len(), 2); // Entry 1 (has AA) and entry 2 (has DD)
        assert_eq!(count, 1); // Entry 3 filtered out
    }

    #[test]
    fn test_collect_active_tokens() {
        let strategy_tokens: Vec<Vec<Address>> = vec![
            vec![Address::repeat_byte(0xAA), Address::repeat_byte(0xBB)],
            vec![Address::repeat_byte(0xBB), Address::repeat_byte(0xCC)], // BB is duplicate
            vec![Address::repeat_byte(0xDD)],
        ];

        let active = collect_active_tokens(strategy_tokens);

        assert_eq!(active.len(), 4); // AA, BB, CC, DD (deduplicated)
        assert!(active.contains(&Address::repeat_byte(0xAA)));
        assert!(active.contains(&Address::repeat_byte(0xBB)));
        assert!(active.contains(&Address::repeat_byte(0xCC)));
        assert!(active.contains(&Address::repeat_byte(0xDD)));
    }

    #[test]
    fn test_collect_active_tokens_empty() {
        let strategy_tokens: Vec<Vec<Address>> = vec![];
        let active = collect_active_tokens(strategy_tokens);
        assert!(active.is_empty());
    }

    #[test]
    fn test_filter_partial_overlap_includes() {
        // If an AMM has tokens [A, B] and only A is active, include it
        let entries = vec![make_entry(0x01, &[0xAA, 0xBB])];
        let active: HashSet<Address> = [Address::repeat_byte(0xAA)].into_iter().collect();

        let (filtered, count) = filter_amm_entries_by_tokens(&entries, &active);

        assert_eq!(filtered.len(), 1);
        assert_eq!(count, 0);
    }

    fn make_owner(addr_byte: u8, involved_bytes: &[u8]) -> (Address, Vec<Address>) {
        (
            Address::repeat_byte(addr_byte),
            involved_bytes
                .iter()
                .map(|b| Address::repeat_byte(*b))
                .collect(),
        )
    }

    #[test]
    fn test_strategy_amm_ownership_basic_overlap() {
        // Strategy A uses tokens [BAL, WETH]; strategy B uses [SPELL, WETH, USDC].
        // AMM 1: BAL/WETH (owned by A only)
        // AMM 2: SPELL/WETH (owned by B only)
        // AMM 3: WETH/USDC (owned by B only — A has no USDC)
        // AMM 4: DAI/USDC (unclaimed — neither strategy involves DAI or USDC alone)
        //   wait, B has USDC, so AMM 4 IS claimed by B. Use a true unclaimed:
        // AMM 5: FOO/BAR — unclaimed
        let strat_a = make_owner(0x10, &[0xBA, 0xEE]); // BAL=BA, WETH=EE
        let strat_b = make_owner(0x20, &[0x5E, 0xEE, 0xDC]); // SPELL=5E, WETH=EE, USDC=DC

        let amm1 = make_entry(0x01, &[0xBA, 0xEE]);
        let amm2 = make_entry(0x02, &[0x5E, 0xEE]);
        let amm3 = make_entry(0x03, &[0xEE, 0xDC]);
        let amm5 = make_entry(0x05, &[0xF0, 0xF1]); // truly unclaimed
        let entries = vec![amm1.clone(), amm2.clone(), amm3.clone(), amm5.clone()];
        let strategies = vec![strat_a.clone(), strat_b.clone()];

        let ownership = build_strategy_amm_ownership(strategies, &entries);

        let owned_a = ownership.owned_by_strategy.get(&strat_a.0).unwrap();
        let owned_b = ownership.owned_by_strategy.get(&strat_b.0).unwrap();

        // A owns AMM 1 (BAL/WETH) and AMM 3 (WETH/_): WETH overlap
        assert!(owned_a.contains(&amm1.address));
        // A owns AMM 2 (SPELL/WETH): WETH overlap
        assert!(owned_a.contains(&amm2.address));
        // A owns AMM 3 (WETH/USDC): WETH overlap
        assert!(owned_a.contains(&amm3.address));
        // A does NOT own AMM 5 (no token overlap)
        assert!(!owned_a.contains(&amm5.address));

        // B owns AMM 2 (SPELL/WETH), AMM 3 (WETH/USDC), AMM 1 (WETH overlap)
        assert!(owned_b.contains(&amm1.address));
        assert!(owned_b.contains(&amm2.address));
        assert!(owned_b.contains(&amm3.address));
        assert!(!owned_b.contains(&amm5.address));

        // Unclaimed: AMM 5 only
        assert_eq!(ownership.unclaimed.len(), 1);
        assert!(ownership.unclaimed.contains(&amm5.address));
    }

    #[test]
    fn test_strategy_amm_ownership_active_amms_includes_unclaimed() {
        let strat_a = make_owner(0x10, &[0xAA]);
        let strat_b = make_owner(0x20, &[0xBB]);

        let amm_a = make_entry(0x01, &[0xAA]);
        let amm_b = make_entry(0x02, &[0xBB]);
        let amm_unclaimed = make_entry(0x03, &[0xCC]);
        let entries = vec![amm_a.clone(), amm_b.clone(), amm_unclaimed.clone()];

        let ownership = build_strategy_amm_ownership([strat_a.clone(), strat_b.clone()], &entries);

        // Only A ready: A's pool + unclaimed, but NOT B's pool.
        let active = ownership.active_amms(&[strat_a.0]);
        assert!(active.contains(&amm_a.address));
        assert!(active.contains(&amm_unclaimed.address));
        assert!(!active.contains(&amm_b.address));

        // Empty ready set: only unclaimed pools are included.
        let active_empty = ownership.active_amms(&[]);
        assert_eq!(active_empty.len(), 1);
        assert!(active_empty.contains(&amm_unclaimed.address));
    }

    #[test]
    fn test_strategy_amm_ownership_strategy_with_no_involved_tokens_owns_nothing() {
        let strat = make_owner(0x10, &[]);
        let amm = make_entry(0x01, &[0xAA]);
        let ownership = build_strategy_amm_ownership([strat.clone()], std::slice::from_ref(&amm));

        assert!(
            ownership
                .owned_by_strategy
                .get(&strat.0)
                .unwrap()
                .is_empty()
        );
        assert!(ownership.unclaimed.contains(&amm.address));
    }

    #[test]
    fn test_pancake_swap_v3_toml_deserialization() {
        let toml_content = r#"
[[amms.arbitrum]]
type = "pancake_swap_v3"
address = "0x7fCDc35463E3770c2fB992716Cd070B63540b947"
tokens = [
    "0xaf88d065e77c8cC2239327C5EDb3A432268e5831",
    "0x82af49447d8a07e3bd95bd0d56f35241523fbab1",
]
fee_tier = 100
"#;
        let parsed: AmmConfigFile = toml::from_str(toml_content).expect("should parse TOML");
        let entries = parsed.amms.get("arbitrum").expect("should have arbitrum");
        assert_eq!(entries.len(), 1);
        assert!(matches!(entries[0].kind, AmmType::PancakeSwapV3));
        assert_eq!(entries[0].fee_tier, Some(100));
    }

    #[test]
    fn test_balancer_toml_deserialization_with_vault_override() {
        let toml_content = r#"
[[amms.base]]
type = "balancer"
address = "0xa04259de0129ac4c4a0ce22be2ec729482034ba0"
vault_address = "0xBA12222222228d8Ba445958a75a0704d566BF2C8"
pool_id = "0xa04259de0129ac4c4a0ce22be2ec729482034ba000020000000000000000016d"
tokens = [
    "0x4158734D47Fc9692176B5085E0F52ee0Da5d47F1",
    "0x1509706a6c66CA549ff0cB464de88231DDBe213B",
]
"#;

        let parsed: AmmConfigFile = toml::from_str(toml_content).expect("should parse TOML");
        let entries = parsed.amms.get("base").expect("should have base");
        assert_eq!(entries.len(), 1);
        assert!(matches!(entries[0].kind, AmmType::Balancer));
        assert_eq!(
            entries[0].vault_address,
            Some(
                Address::parse_checksummed("0xBA12222222228d8Ba445958a75a0704d566BF2C8", None)
                    .expect("valid address")
            )
        );
    }

    #[test]
    fn test_pancake_swap_v3_grouped_with_v3_in_prefetch() {
        // PancakeSwapV3 entries should be grouped with UniswapV3 for prefetching
        let entries = [
            AmmConfigEntry {
                kind: AmmType::UniswapV3,
                address: Address::repeat_byte(0x01),
                tokens: vec![],
                fee_tier: Some(3000),
                vault_address: None,
                pool_id: None,
                tick_spacing: None,
                stable: None,
                factory_address: None,
                hooks: None,
                curve_use_uint256: None,
                balancer_v3_pool_type: None,
            },
            AmmConfigEntry {
                kind: AmmType::PancakeSwapV3,
                address: Address::repeat_byte(0x02),
                tokens: vec![],
                fee_tier: Some(100),
                vault_address: None,
                pool_id: None,
                tick_spacing: None,
                stable: None,
                factory_address: None,
                hooks: None,
                curve_use_uint256: None,
                balancer_v3_pool_type: None,
            },
        ];

        // Count V3-style entries the same way the prefetch phase does
        let v3_count = entries
            .iter()
            .filter(|e| matches!(e.kind, AmmType::UniswapV3 | AmmType::PancakeSwapV3))
            .count();
        assert_eq!(
            v3_count, 2,
            "both UniswapV3 and PancakeSwapV3 should be V3-style"
        );
    }

    #[test]
    fn test_filter_pancake_swap_v3_by_tokens() {
        let entries = vec![
            make_entry(0x01, &[0xAA, 0xBB]),
            AmmConfigEntry {
                kind: AmmType::PancakeSwapV3,
                address: Address::repeat_byte(0x02),
                tokens: vec![Address::repeat_byte(0xCC), Address::repeat_byte(0xDD)],
                fee_tier: Some(100),
                vault_address: None,
                pool_id: None,
                tick_spacing: None,
                stable: None,
                factory_address: None,
                hooks: None,
                curve_use_uint256: None,
                balancer_v3_pool_type: None,
            },
        ];

        // Only 0xCC is active, so only PancakeSwapV3 entry should be included
        let active: HashSet<Address> = [Address::repeat_byte(0xCC)].into_iter().collect();
        let (filtered, count) = filter_amm_entries_by_tokens(&entries, &active);

        assert_eq!(filtered.len(), 1);
        assert_eq!(count, 1);
        assert!(matches!(filtered[0].kind, AmmType::PancakeSwapV3));
    }

    #[test]
    fn test_curve_toml_deserialization_with_use_uint256() {
        let toml_content = r#"
[[amms.arbitrum]]
type = "curve"
address = "0x0000000000000000000000000000000000000001"
tokens = [
    "0x11cDb42B0EB46D95f990BeDD4695A6e3fA034978",
    "0x2f2a2543B76A4166549F7aaB2e75Bef0aefC5B0f",
    "0x82af49447d8a07e3bd95bd0d56f35241523fbab1",
]
curve_use_uint256 = true
"#;
        let parsed: AmmConfigFile = toml::from_str(toml_content).expect("should parse TOML");
        let entries = parsed.amms.get("arbitrum").expect("should have arbitrum");
        assert_eq!(entries.len(), 1);
        assert!(matches!(entries[0].kind, AmmType::Curve));
        assert_eq!(entries[0].tokens.len(), 3);
        assert_eq!(entries[0].curve_use_uint256, Some(true));
    }

    #[test]
    fn test_curve_toml_deserialization_defaults() {
        // Curve entries without explicit curve_use_uint256 should parse with None
        let toml_content = r#"
[[amms.ethereum]]
type = "curve"
address = "0x0000000000000000000000000000000000000002"
tokens = [
    "0xD533a949740bb3306d119CC777fa900bA034cd52",
    "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2",
]
"#;
        let parsed: AmmConfigFile = toml::from_str(toml_content).expect("should parse TOML");
        let entries = parsed.amms.get("ethereum").expect("should have ethereum");
        assert_eq!(entries.len(), 1);
        assert!(matches!(entries[0].kind, AmmType::Curve));
        assert_eq!(entries[0].tokens.len(), 2);
        assert!(entries[0].curve_use_uint256.is_none());
    }

    #[test]
    fn test_balancer_v3_pool_type_hint_deserialization() {
        let toml_content = r#"
[[amms.arbitrum]]
type = "balancer_v3"
address = "0x5418a64e0cdb20548acb394f5d00a089baf02161"
vault_address = "0xbA1333333333a1BA1108E8412f11850A5C319bA9"
balancer_v3_pool_type = "stable"
tokens = [
    "0x4ce13a79f45c1be00bdabd38b764ac28c082704e",
    "0xec70dcb4a1efa46b8f2d97c310c9c4790ba5ffa8",
]
"#;
        let parsed: AmmConfigFile = toml::from_str(toml_content).expect("should parse TOML");
        let entries = parsed.amms.get("arbitrum").expect("should have arbitrum");
        assert_eq!(entries.len(), 1);
        assert!(matches!(entries[0].kind, AmmType::BalancerV3));
        assert_eq!(
            entries[0].balancer_v3_pool_type,
            Some(BalancerV3PoolType::Stable)
        );
    }

    #[test]
    fn test_balancer_v3_pool_type_hint_defaults_to_none() {
        let toml_content = r#"
[[amms.arbitrum]]
type = "balancer_v3"
address = "0x5418a64e0cdb20548acb394f5d00a089baf02161"
vault_address = "0xbA1333333333a1BA1108E8412f11850A5C319bA9"
tokens = [
    "0x4ce13a79f45c1be00bdabd38b764ac28c082704e",
    "0xec70dcb4a1efa46b8f2d97c310c9c4790ba5ffa8",
]
"#;
        let parsed: AmmConfigFile = toml::from_str(toml_content).expect("should parse TOML");
        let entries = parsed.amms.get("arbitrum").expect("should have arbitrum");
        assert!(entries[0].balancer_v3_pool_type.is_none());
    }

    #[test]
    fn test_v3_prefetch_skip_reason_matches_non_archive_errors() {
        let address = Address::repeat_byte(0x42);
        let stats = V3PrefetchStats {
            errors_by_pool: HashMap::from([(address, 1)]),
            total_requested_by_pool: HashMap::from([(address, 600)]),
            error_samples_by_pool: HashMap::from([(
                address,
                "missing trie node: state is not available, not found".to_string(),
            )]),
        };

        assert_eq!(
            v3_prefetch_skip_reason(&stats, address),
            Some("required historical storage is unavailable on the current RPC")
        );
    }

    #[test]
    fn test_v3_prefetch_skip_reason_matches_large_failure_ratio() {
        let address = Address::repeat_byte(0x43);
        let stats = V3PrefetchStats {
            errors_by_pool: HashMap::from([(address, 500)]),
            total_requested_by_pool: HashMap::from([(address, 600)]),
            error_samples_by_pool: HashMap::new(),
        };

        assert_eq!(
            v3_prefetch_skip_reason(&stats, address),
            Some("too many tick slots failed prefetch and would trigger serial RPC fallback")
        );
    }

    #[test]
    fn test_v3_prefetch_skip_reason_ignores_small_transient_failures() {
        let address = Address::repeat_byte(0x44);
        let stats = V3PrefetchStats {
            errors_by_pool: HashMap::from([(address, 4)]),
            total_requested_by_pool: HashMap::from([(address, 600)]),
            error_samples_by_pool: HashMap::from([(
                address,
                "HTTP error 429: rate limited".to_string(),
            )]),
        };

        assert_eq!(v3_prefetch_skip_reason(&stats, address), None);
    }
}
