//! Factory-based AMM discovery.
//!
//! Queries on-chain factory contracts to find pools for caller-provided token pairs
//! that are not already in the configured AMM set. Discovered pools are filtered
//! by liquidity (non-zero) and returned as initialized AMM state.
//!
//! Token pair generation is intentionally caller-owned so this crate stays
//! independent from any particular strategy, search, or execution system.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};

use alloy_primitives::{Address, Signed, U256, Uint};
use alloy_sol_types::SolCall;
use amms::amms::amm::AutomatedMarketMaker;
use anyhow::Result;
use tracing::{debug, info};

use evm_fork_cache::cache::EvmCache;
use evm_fork_cache::multicall::try_decode_result;

use crate::amm_wrapper::LocalAMM;
use crate::cache_sync::{
    init_pancakeswap_v3_from_cache, init_slipstream_from_cache, init_uniswap_v2_from_cache,
    init_uniswap_v3_from_cache,
};
use crate::tuning::ProtocolAddresses;

/// Type alias for AMM references used throughout the system.
pub type AMMRef = Arc<RwLock<LocalAMM>>;

alloy_sol_types::sol! {
    interface IUniswapV3Factory {
        function getPool(address tokenA, address tokenB, uint24 fee)
            external view returns (address pool);
    }

    interface ISlipstreamFactory {
        function getPool(address tokenA, address tokenB, int24 tickSpacing)
            external view returns (address pool);
    }

    interface IUniswapV2Factory {
        function getPair(address tokenA, address tokenB)
            external view returns (address pair);
    }
}

/// PancakeSwap V3 has different fee tiers from Uniswap V3.
const PANCAKE_V3_FEE_TIERS: &[u32] = &[100, 500, 2500, 10000];

/// Common Slipstream (Aerodrome/Velodrome CL) tick spacings.
const SLIPSTREAM_TICK_SPACINGS: &[i32] = &[1, 50, 100, 200];

/// Default V2 fee in basis points (0.3%).
const DEFAULT_V2_FEE: u32 = 300;

/// Factory discovery settings.
#[derive(Debug, Clone)]
pub struct FactoryDiscoveryConfig {
    pub protocol_addresses: ProtocolAddresses,
    pub uniswap_v3_fee_tiers: Vec<u32>,
    pub pancake_v3_fee_tiers: Vec<u32>,
    pub v2_fee_bps: u32,
}

impl FactoryDiscoveryConfig {
    pub fn new(protocol_addresses: ProtocolAddresses, uniswap_v3_fee_tiers: Vec<u32>) -> Self {
        Self {
            protocol_addresses,
            uniswap_v3_fee_tiers,
            pancake_v3_fee_tiers: PANCAKE_V3_FEE_TIERS.to_vec(),
            v2_fee_bps: DEFAULT_V2_FEE,
        }
    }
}

/// Display metadata used for discovery logs and TOML suggestions.
#[derive(Debug, Clone)]
pub struct TokenDisplayMetadata {
    pub symbol: String,
    pub decimals: u8,
}

/// The type of pool discovered from a factory.
#[derive(Debug, Clone, Copy)]
enum DiscoveredPoolType {
    UniswapV3,
    PancakeSwapV3,
    Slipstream { tick_spacing: i32 },
    UniswapV2 { fee: u32 },
}

/// A pool candidate returned by a factory query, before initialization.
#[derive(Debug, Clone)]
struct FactoryPoolCandidate {
    address: Address,
    pool_type: DiscoveredPoolType,
}

/// Result of a discovery run.
pub struct DiscoveryResult {
    pub discovered: Vec<(Address, LocalAMM)>,
    pub skipped_zero_liquidity: usize,
    pub skipped_already_known: usize,
    pub total_queried: usize,
}

/// Canonical pair ordering to avoid duplicates.
pub fn canonical_pair(a: Address, b: Address) -> (Address, Address) {
    if a < b { (a, b) } else { (b, a) }
}

/// Generate WETH-centric token pairs: every known token paired with WETH.
///
/// The caller can also add any other application-specific pairs (e.g. pairs
/// observed in recent swaps, or per-route token-set pairs for multi-hop
/// shortcuts) before passing the set to [`discover_factory_amms`].
pub fn generate_weth_centric_pairs(
    active_tokens: &HashSet<Address>,
    weth: Address,
) -> HashSet<(Address, Address)> {
    let mut pairs = HashSet::new();
    for token in active_tokens {
        if *token != weth {
            pairs.insert(canonical_pair(*token, weth));
        }
    }
    pairs
}

/// Query a V3-style factory (Uniswap V3 or PancakeSwap V3) for pools.
fn query_v3_factory_pools(
    cache: &mut EvmCache,
    factory: Address,
    pairs: &[(Address, Address)],
    fee_tiers: &[u32],
    pool_type_fn: fn(u32) -> DiscoveredPoolType,
) -> Result<Vec<FactoryPoolCandidate>> {
    let total_calls = pairs.len() * fee_tiers.len();
    if total_calls == 0 {
        return Ok(Vec::new());
    }

    let mut call_meta: Vec<u32> = Vec::with_capacity(total_calls);
    let mut calls: Vec<(Address, alloy_primitives::Bytes, bool)> = Vec::with_capacity(total_calls);

    for (token_a, token_b) in pairs {
        for &fee in fee_tiers {
            let call = IUniswapV3Factory::getPoolCall {
                tokenA: *token_a,
                tokenB: *token_b,
                fee: Uint::from(fee),
            };
            calls.push((factory, call.abi_encode().into(), true));
            call_meta.push(fee);
        }
    }

    let results = evm_fork_cache::multicall::execute_batched(cache, calls)?;

    let mut candidates = Vec::new();
    for (i, result) in results.iter().enumerate() {
        if let Some(pool_addr) = try_decode_result::<IUniswapV3Factory::getPoolCall>(result)
            && pool_addr != Address::ZERO
        {
            candidates.push(FactoryPoolCandidate {
                address: pool_addr,
                pool_type: pool_type_fn(call_meta[i]),
            });
        }
    }

    Ok(candidates)
}

/// Query a Slipstream factory for pools.
fn query_slipstream_factory_pools(
    cache: &mut EvmCache,
    factory: Address,
    pairs: &[(Address, Address)],
) -> Result<Vec<FactoryPoolCandidate>> {
    let total_calls = pairs.len() * SLIPSTREAM_TICK_SPACINGS.len();
    if total_calls == 0 {
        return Ok(Vec::new());
    }

    let mut call_meta: Vec<i32> = Vec::with_capacity(total_calls);
    let mut calls: Vec<(Address, alloy_primitives::Bytes, bool)> = Vec::with_capacity(total_calls);

    for (token_a, token_b) in pairs {
        for &ts in SLIPSTREAM_TICK_SPACINGS {
            let call = ISlipstreamFactory::getPoolCall {
                tokenA: *token_a,
                tokenB: *token_b,
                tickSpacing: Signed::try_from(ts).unwrap_or_default(),
            };
            calls.push((factory, call.abi_encode().into(), true));
            call_meta.push(ts);
        }
    }

    let results = evm_fork_cache::multicall::execute_batched(cache, calls)?;

    let mut candidates = Vec::new();
    for (i, result) in results.iter().enumerate() {
        if let Some(pool_addr) = try_decode_result::<ISlipstreamFactory::getPoolCall>(result)
            && pool_addr != Address::ZERO
        {
            candidates.push(FactoryPoolCandidate {
                address: pool_addr,
                pool_type: DiscoveredPoolType::Slipstream {
                    tick_spacing: call_meta[i],
                },
            });
        }
    }

    Ok(candidates)
}

/// Query a V2-style factory for pools.
fn query_v2_factory_pools(
    cache: &mut EvmCache,
    factory: Address,
    pairs: &[(Address, Address)],
    fee: u32,
) -> Result<Vec<FactoryPoolCandidate>> {
    if pairs.is_empty() {
        return Ok(Vec::new());
    }

    let calls: Vec<(Address, alloy_primitives::Bytes, bool)> = pairs
        .iter()
        .map(|(token_a, token_b)| {
            let call = IUniswapV2Factory::getPairCall {
                tokenA: *token_a,
                tokenB: *token_b,
            };
            (factory, call.abi_encode().into(), true)
        })
        .collect();

    let results = evm_fork_cache::multicall::execute_batched(cache, calls)?;

    let mut candidates = Vec::new();
    for result in &results {
        if let Some(pair_addr) = try_decode_result::<IUniswapV2Factory::getPairCall>(result)
            && pair_addr != Address::ZERO
        {
            candidates.push(FactoryPoolCandidate {
                address: pair_addr,
                pool_type: DiscoveredPoolType::UniswapV2 { fee },
            });
        }
    }

    Ok(candidates)
}

/// Initialize discovered pool candidates and filter out zero-liquidity pools.
async fn init_and_filter_candidates(
    cache: &mut EvmCache,
    candidates: Vec<FactoryPoolCandidate>,
) -> (Vec<(Address, LocalAMM)>, usize) {
    let mut discovered = Vec::new();
    let mut skipped = 0usize;

    for candidate in candidates {
        let result = match candidate.pool_type {
            DiscoveredPoolType::UniswapV3 => init_uniswap_v3_from_cache(cache, candidate.address)
                .await
                .map(LocalAMM::UniswapV3),
            DiscoveredPoolType::PancakeSwapV3 => {
                init_pancakeswap_v3_from_cache(cache, candidate.address)
                    .await
                    .map(LocalAMM::PancakeSwapV3)
            }
            DiscoveredPoolType::Slipstream { tick_spacing } => {
                init_slipstream_from_cache(cache, candidate.address, tick_spacing)
                    .await
                    .map(LocalAMM::Slipstream)
            }
            DiscoveredPoolType::UniswapV2 { fee } => {
                init_uniswap_v2_from_cache(cache, candidate.address, fee as usize)
                    .await
                    .map(LocalAMM::UniswapV2)
            }
        };

        match result {
            Ok(amm) => {
                if has_nonzero_liquidity(&amm) {
                    discovered.push((candidate.address, amm));
                } else {
                    debug!(
                        pool = %candidate.address,
                        pool_type = ?candidate.pool_type,
                        "Skipping discovered pool: zero liquidity"
                    );
                    skipped += 1;
                }
            }
            Err(e) => {
                debug!(
                    pool = %candidate.address,
                    pool_type = ?candidate.pool_type,
                    error = %e,
                    "Failed to initialize discovered pool"
                );
                skipped += 1;
            }
        }
    }

    (discovered, skipped)
}

/// Check if a pool has non-zero liquidity (usable for swaps).
fn has_nonzero_liquidity(amm: &LocalAMM) -> bool {
    match amm {
        LocalAMM::UniswapV2(pool) => pool.reserve_0 > 0 && pool.reserve_1 > 0,
        LocalAMM::UniswapV3(pool) | LocalAMM::PancakeSwapV3(pool) => {
            pool.liquidity > 0 && pool.sqrt_price != U256::ZERO
        }
        LocalAMM::Slipstream(pool) => pool.liquidity > 0 && pool.sqrt_price != U256::ZERO,
        _ => true,
    }
}

/// Main entry point: discover AMMs from on-chain factories.
///
/// `discovery_pairs` should contain all interesting token pairs to query — the caller
/// is responsible for assembling them from whatever sources are relevant (e.g.
/// WETH-centric pairs, recently observed pairs, and per-route token-set pairs;
/// use [`generate_weth_centric_pairs`] and [`canonical_pair`] as helpers).
pub async fn discover_factory_amms(
    cache: &mut EvmCache,
    discovery_pairs: &HashSet<(Address, Address)>,
    config: &FactoryDiscoveryConfig,
    existing_amms: &HashMap<Address, AMMRef>,
) -> Result<DiscoveryResult> {
    let protocol_addrs = &config.protocol_addresses;
    let pairs: Vec<(Address, Address)> = discovery_pairs.iter().copied().collect();

    debug!(pair_count = pairs.len(), "Discovery token pairs");

    if pairs.is_empty() {
        return Ok(DiscoveryResult {
            discovered: Vec::new(),
            skipped_zero_liquidity: 0,
            skipped_already_known: 0,
            total_queried: 0,
        });
    }

    // Query all configured factories
    let mut all_candidates: Vec<FactoryPoolCandidate> = Vec::new();
    let mut total_queried = 0usize;

    // Uniswap V3 factory
    if let Some(factory) = protocol_addrs.uniswap_v3_factory {
        cache.ensure_account(factory).await?;
        let candidates = query_v3_factory_pools(
            cache,
            factory,
            &pairs,
            &config.uniswap_v3_fee_tiers,
            |_fee| DiscoveredPoolType::UniswapV3,
        )?;
        total_queried += pairs.len() * config.uniswap_v3_fee_tiers.len();
        debug!(factory = %factory, candidates = candidates.len(), "UniswapV3 factory query");
        all_candidates.extend(candidates);
    }

    // PancakeSwap V3 factory
    if let Some(factory) = protocol_addrs.pancake_v3_factory {
        cache.ensure_account(factory).await?;
        let candidates = query_v3_factory_pools(
            cache,
            factory,
            &pairs,
            &config.pancake_v3_fee_tiers,
            |_fee| DiscoveredPoolType::PancakeSwapV3,
        )?;
        total_queried += pairs.len() * config.pancake_v3_fee_tiers.len();
        debug!(factory = %factory, candidates = candidates.len(), "PancakeSwapV3 factory query");
        all_candidates.extend(candidates);
    }

    // Slipstream factory
    if let Some(factory) = protocol_addrs.slipstream_factory {
        cache.ensure_account(factory).await?;
        let candidates = query_slipstream_factory_pools(cache, factory, &pairs)?;
        total_queried += pairs.len() * SLIPSTREAM_TICK_SPACINGS.len();
        debug!(factory = %factory, candidates = candidates.len(), "Slipstream factory query");
        all_candidates.extend(candidates);
    }

    // V2 factories
    if let Some(factory) = protocol_addrs.uniswap_v2_factory {
        cache.ensure_account(factory).await?;
        let candidates = query_v2_factory_pools(cache, factory, &pairs, config.v2_fee_bps)?;
        total_queried += pairs.len();
        debug!(factory = %factory, candidates = candidates.len(), "UniswapV2 factory query");
        all_candidates.extend(candidates);
    }

    if let Some(factory) = protocol_addrs.sushiswap_v2_factory {
        cache.ensure_account(factory).await?;
        let candidates = query_v2_factory_pools(cache, factory, &pairs, config.v2_fee_bps)?;
        total_queried += pairs.len();
        debug!(factory = %factory, candidates = candidates.len(), "SushiSwapV2 factory query");
        all_candidates.extend(candidates);
    }

    // Dedup and filter already-known pools
    let mut skipped_already_known = 0usize;
    let mut seen_addresses: HashSet<Address> = HashSet::new();
    all_candidates.retain(|c| {
        if existing_amms.contains_key(&c.address) || !seen_addresses.insert(c.address) {
            skipped_already_known += 1;
            false
        } else {
            true
        }
    });

    debug!(
        candidates = all_candidates.len(),
        skipped_already_known, "Deduped factory candidates"
    );

    // Initialize and filter by liquidity
    let (discovered, skipped_zero_liquidity) =
        init_and_filter_candidates(cache, all_candidates).await;

    Ok(DiscoveryResult {
        discovered,
        skipped_zero_liquidity,
        skipped_already_known,
        total_queried,
    })
}

/// Log each discovered pool at info level.
pub fn log_discovered_amms(
    result: &DiscoveryResult,
    token_metadata: &HashMap<Address, TokenDisplayMetadata>,
) {
    for (addr, amm) in &result.discovered {
        let tokens = amm.tokens();
        let token_names: Vec<String> = tokens
            .iter()
            .map(|t| {
                token_metadata
                    .get(t)
                    .map(|m| m.symbol.clone())
                    .unwrap_or_else(|| format!("{:.8}", t))
            })
            .collect();

        let type_label = match amm {
            LocalAMM::UniswapV2(_) => "UniswapV2".to_string(),
            LocalAMM::UniswapV3(p) => format!("UniswapV3(fee={})", p.fee),
            LocalAMM::PancakeSwapV3(p) => format!("PancakeV3(fee={})", p.fee),
            LocalAMM::Slipstream(p) => format!("Slipstream(ts={})", p.tick_spacing),
            _ => "Unknown".to_string(),
        };

        info!(
            pool = %addr,
            pool_type = %type_label,
            tokens = %token_names.join("/"),
            "Discovered new AMM"
        );
    }
}

/// Format discovered pools as TOML entries for amms.toml.
pub fn format_suggested_toml(result: &DiscoveryResult, chain_name: &str) -> String {
    let mut output = String::new();
    output.push_str(&format!(
        "# Suggested AMM additions for {} ({} pools discovered)\n\n",
        chain_name,
        result.discovered.len()
    ));

    for (addr, amm) in &result.discovered {
        let tokens = amm.tokens();
        let tokens_str: Vec<String> = tokens.iter().map(|t| format!("    \"{:?}\"", t)).collect();

        match amm {
            LocalAMM::UniswapV2(_) => {
                output.push_str(&format!("[[amms.{}]]\n", chain_name));
                output.push_str("type = \"uniswap_v2\"\n");
                output.push_str(&format!("address = \"{:?}\"\n", addr));
                output.push_str(&format!("tokens = [\n{}\n]\n", tokens_str.join(",\n")));
                output.push_str("fee_tier = 300\n\n");
            }
            LocalAMM::UniswapV3(pool) => {
                output.push_str(&format!("[[amms.{}]]\n", chain_name));
                output.push_str("type = \"uniswap_v3\"\n");
                output.push_str(&format!("address = \"{:?}\"\n", addr));
                output.push_str(&format!("tokens = [\n{}\n]\n", tokens_str.join(",\n")));
                output.push_str(&format!("fee_tier = {}\n\n", pool.fee));
            }
            LocalAMM::PancakeSwapV3(pool) => {
                output.push_str(&format!("[[amms.{}]]\n", chain_name));
                output.push_str("type = \"pancake_v3\"\n");
                output.push_str(&format!("address = \"{:?}\"\n", addr));
                output.push_str(&format!("tokens = [\n{}\n]\n", tokens_str.join(",\n")));
                output.push_str(&format!("fee_tier = {}\n\n", pool.fee));
            }
            LocalAMM::Slipstream(pool) => {
                output.push_str(&format!("[[amms.{}]]\n", chain_name));
                output.push_str("type = \"slipstream\"\n");
                output.push_str(&format!("address = \"{:?}\"\n", addr));
                output.push_str(&format!("tokens = [\n{}\n]\n", tokens_str.join(",\n")));
                output.push_str(&format!("tick_spacing = {}\n\n", pool.tick_spacing));
            }
            _ => {}
        }
    }

    output
}

/// Information about a discovered AMM for interactive selection.
pub struct DiscoveredAmmInfo {
    pub address: Address,
    pub protocol: String,
    pub fee_or_param: String,
    pub token_symbols: Vec<String>,
    pub liquidity_display: String,
    /// Whether the caller flagged this pool as of interest (see
    /// `highlighted_amms` in [`build_discovered_amm_info`]).
    pub highlighted: bool,
}

/// Build display info for each discovered AMM.
///
/// `highlighted_amms` is a caller-supplied set of addresses to mark as of
/// interest in the resulting rows (e.g. pools that appear in a route the caller
/// cares about); pass an empty set if not needed.
pub fn build_discovered_amm_info(
    discovered: &[(Address, LocalAMM)],
    token_metadata: &HashMap<Address, TokenDisplayMetadata>,
    highlighted_amms: &HashSet<Address>,
) -> Vec<DiscoveredAmmInfo> {
    discovered
        .iter()
        .map(|(addr, amm)| {
            let tokens = amm.tokens();
            let token_symbols: Vec<String> = tokens
                .iter()
                .map(|t| {
                    token_metadata
                        .get(t)
                        .map(|m| m.symbol.clone())
                        .unwrap_or_else(|| format!("{:.10}", t))
                })
                .collect();

            let (protocol, fee_or_param) = match amm {
                LocalAMM::UniswapV2(_) => ("UniswapV2".to_string(), "fee=30bps".to_string()),
                LocalAMM::UniswapV3(p) => {
                    ("UniswapV3".to_string(), format!("fee={}bps", p.fee / 100))
                }
                LocalAMM::PancakeSwapV3(p) => {
                    ("PancakeV3".to_string(), format!("fee={}bps", p.fee / 100))
                }
                LocalAMM::Slipstream(p) => {
                    ("Slipstream".to_string(), format!("ts={}", p.tick_spacing))
                }
                _ => ("Unknown".to_string(), String::new()),
            };

            let liquidity_display = format_pool_liquidity(amm, &tokens, token_metadata);

            DiscoveredAmmInfo {
                address: *addr,
                protocol,
                fee_or_param,
                token_symbols,
                liquidity_display,
                highlighted: highlighted_amms.contains(addr),
            }
        })
        .collect()
}

/// Format liquidity display for a pool (e.g., "42 WETH + 120,000 USDC").
fn format_pool_liquidity(
    amm: &LocalAMM,
    tokens: &[Address],
    token_metadata: &HashMap<Address, TokenDisplayMetadata>,
) -> String {
    match amm {
        LocalAMM::UniswapV2(pool) => {
            let r0 = format_reserve(pool.reserve_0, tokens.first().copied(), token_metadata);
            let r1 = format_reserve(pool.reserve_1, tokens.get(1).copied(), token_metadata);
            format!("{} + {}", r0, r1)
        }
        LocalAMM::SolidlyV2(pool) => {
            let r0 = format_reserve(pool.reserve_0, tokens.first().copied(), token_metadata);
            let r1 = format_reserve(pool.reserve_1, tokens.get(1).copied(), token_metadata);
            format!("{} + {}", r0, r1)
        }
        LocalAMM::UniswapV3(pool) | LocalAMM::PancakeSwapV3(pool) => {
            if pool.liquidity > 0 {
                format!("liq={}", format_compact_number(pool.liquidity as f64))
            } else {
                "liq=0".to_string()
            }
        }
        LocalAMM::Slipstream(pool) => {
            if pool.liquidity > 0 {
                format!("liq={}", format_compact_number(pool.liquidity as f64))
            } else {
                "liq=0".to_string()
            }
        }
        _ => "N/A".to_string(),
    }
}

/// Format a u128 reserve with token decimals and symbol.
fn format_reserve(
    reserve: u128,
    token: Option<Address>,
    token_metadata: &HashMap<Address, TokenDisplayMetadata>,
) -> String {
    let Some(addr) = token else {
        return format!("{}", reserve);
    };
    let meta = token_metadata.get(&addr);
    let decimals = meta.map(|m| m.decimals).unwrap_or(18);
    let symbol = meta.map(|m| m.symbol.as_str()).unwrap_or("???");
    let divisor = 10f64.powi(decimals as i32);
    let human = reserve as f64 / divisor;
    format!("{} {}", format_compact_number(human), symbol)
}

/// Format a number compactly: 1,234,567 → "1.23M", 42,000 → "42K", 0.5 → "0.5".
fn format_compact_number(n: f64) -> String {
    if n >= 1_000_000_000.0 {
        format!("{:.2}B", n / 1_000_000_000.0)
    } else if n >= 1_000_000.0 {
        format!("{:.2}M", n / 1_000_000.0)
    } else if n >= 1_000.0 {
        format!("{:.1}K", n / 1_000.0)
    } else if n >= 1.0 {
        format!("{:.2}", n)
    } else if n > 0.0 {
        format!("{:.4}", n)
    } else {
        "0".to_string()
    }
}

/// Format a single AMM as a TOML entry for amms.toml.
pub fn format_single_amm_toml(addr: &Address, amm: &LocalAMM, chain_name: &str) -> Option<String> {
    let tokens = amm.tokens();
    let tokens_str: Vec<String> = tokens.iter().map(|t| format!("    \"{:?}\"", t)).collect();
    let tokens_block = format!("tokens = [\n{}\n]", tokens_str.join(",\n"));

    let mut entry = String::new();
    entry.push_str(&format!("[[amms.{}]]\n", chain_name));

    match amm {
        LocalAMM::UniswapV2(_) => {
            entry.push_str("type = \"uniswap_v2\"\n");
            entry.push_str(&format!("address = \"{:?}\"\n", addr));
            entry.push_str(&tokens_block);
            entry.push('\n');
            entry.push_str("fee_tier = 300\n");
        }
        LocalAMM::UniswapV3(pool) => {
            entry.push_str("type = \"uniswap_v3\"\n");
            entry.push_str(&format!("address = \"{:?}\"\n", addr));
            entry.push_str(&tokens_block);
            entry.push('\n');
            entry.push_str(&format!("fee_tier = {}\n", pool.fee));
        }
        LocalAMM::PancakeSwapV3(pool) => {
            entry.push_str("type = \"pancake_v3\"\n");
            entry.push_str(&format!("address = \"{:?}\"\n", addr));
            entry.push_str(&tokens_block);
            entry.push('\n');
            entry.push_str(&format!("fee_tier = {}\n", pool.fee));
        }
        LocalAMM::Slipstream(pool) => {
            entry.push_str("type = \"slipstream\"\n");
            entry.push_str(&format!("address = \"{:?}\"\n", addr));
            entry.push_str(&tokens_block);
            entry.push('\n');
            entry.push_str(&format!("tick_spacing = {}\n", pool.tick_spacing));
        }
        _ => return None,
    }

    Some(entry)
}

/// Append selected AMMs to the amms.toml config file.
///
/// Only appends — never removes existing entries.
pub fn append_amms_to_config(file_path: &std::path::Path, entries: &[String]) -> Result<usize> {
    use std::io::Write;

    if entries.is_empty() {
        return Ok(0);
    }

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(file_path)?;

    // Ensure we start on a new line
    writeln!(file)?;
    for entry in entries {
        writeln!(file, "{}", entry)?;
    }

    Ok(entries.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_canonical_pair_ordering() {
        let a = Address::repeat_byte(0x01);
        let b = Address::repeat_byte(0x02);
        assert_eq!(canonical_pair(a, b), canonical_pair(b, a));
        assert_eq!(canonical_pair(a, b), (a, b));
    }

    #[test]
    fn test_generate_weth_centric_pairs() {
        let weth = Address::repeat_byte(0xFF);
        let token_a = Address::repeat_byte(0x01);
        let token_b = Address::repeat_byte(0x02);

        let mut active_tokens = HashSet::new();
        active_tokens.insert(weth);
        active_tokens.insert(token_a);
        active_tokens.insert(token_b);

        let pairs = generate_weth_centric_pairs(&active_tokens, weth);

        assert!(pairs.contains(&canonical_pair(token_a, weth)));
        assert!(pairs.contains(&canonical_pair(token_b, weth)));
        assert_eq!(pairs.len(), 2);
    }

    #[test]
    fn test_token_set_pairs() {
        // A token set {A, B, WETH, C} should produce C(4,2) = 6 unique pairs,
        // including the direct (A,B) pair.
        let a = Address::repeat_byte(0x01);
        let b = Address::repeat_byte(0x02);
        let weth = Address::repeat_byte(0xFF);
        let c = Address::repeat_byte(0x03);

        let token_set: HashSet<Address> = [a, b, weth, c].into_iter().collect();
        let mut pairs = HashSet::new();

        let tokens: Vec<Address> = token_set.into_iter().collect();
        for i in 0..tokens.len() {
            for j in (i + 1)..tokens.len() {
                pairs.insert(canonical_pair(tokens[i], tokens[j]));
            }
        }

        assert_eq!(pairs.len(), 6);
        // The cross-path pair (A,B) must be present
        assert!(pairs.contains(&canonical_pair(a, b)));
        // The shortcut pair (A,C) must be present
        assert!(pairs.contains(&canonical_pair(a, c)));
    }
}
