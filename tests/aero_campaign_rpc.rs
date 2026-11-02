//! Long-window exact replay and strict-quote campaign for the reviewed Aero
//! pools (env-gated, `#[ignore]`).
//!
//! For each chain the reviewed pool is cold-started at the start of a long
//! window, every pool / gauge / NFT-manager / hook / factory / discount-registry
//! / MEV-module log of the window is applied through the adapter driver with
//! exact block context and transaction-local staking evidence, and the
//! replayed state is compared with the chain:
//!
//! - at every event block, the block after it, every `stride`-th block and the
//!   window end, the complete quote-relevant state is read with one
//!   `eth_call` storage program (`slot0`, liquidity, the staked word, every
//!   fee/reward growth word, every bitmap word in the registered range, all six
//!   words of every initialized tick and the whole observation ring) plus the
//!   reviewed external cells, and each divergent cell is classified;
//! - at the same blocks, the offline quote runs in strict mode (an
//!   [`EvmOverlay`] over the replayed snapshot: no provider, no lazy fetch)
//!   under the block's `basefee` and the configured priority fees, both
//!   directions and exact output, and is compared wei-exact with `eth_call`
//!   at that block; every strict miss is recorded by contract and slot.
//!
//! Divergences and adapter repairs re-synchronise the cache from the chain so
//! later checks measure fresh behaviour. The per-chain report is written as
//! JSON and Markdown under `AERO_CAMPAIGN_REPORT_DIR`.
//!
//! ```text
//! E2E_RPC_URL=<alchemy archive url> [E2E_BASE_RPC_URL=…] [E2E_OPTIMISM_RPC_URL=…] \
//! AERO_CAMPAIGN_STRIDE=100 AERO_CAMPAIGN_ETH_BLOCKS=20000 AERO_CAMPAIGN_L2_BLOCKS=100000 \
//! cargo test --all-features --test aero_campaign_rpc -- --ignored --nocapture --test-threads=1
//! ```
//!
//! Optional: `AERO_CAMPAIGN_{ETH,BASE,OP}_END` (default head − 32),
//! `AERO_CAMPAIGN_{ETH,BASE,OP}_START` (default end − blocks),
//! `AERO_CAMPAIGN_QUOTES=0` to skip quotes, `AERO_CAMPAIGN_REPORT_DIR`
//! (default `target/aero-campaign`).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use alloy_eips::{BlockId, BlockNumberOrTag};
use alloy_network::TransactionBuilder;
use alloy_primitives::{Address, B256, Bytes, U256, address, b256};
use alloy_provider::{Provider, RootProvider, network::AnyNetwork};
use alloy_rpc_types_eth::state::{AccountOverride, StateOverride, StateOverridesBuilder};
use alloy_rpc_types_eth::{Filter, Log as RpcLog, TransactionRequest};
use alloy_sol_types::{SolCall, SolEvent};
use anyhow::{Context, Result, anyhow, ensure};
use serde_json::{Value, json};

use evm_amm_state::adapters::driver::AdapterDriver;
use evm_amm_state::adapters::slipstream_staking::{SlipstreamStakingBatch, StakingBlockRef};
use evm_amm_state::adapters::storage::{
    AERO_CL_FEE_GROWTH_0_SLOT, AERO_CL_FEE_GROWTH_1_SLOT, AERO_CL_GAUGE_FEES_SLOT,
    AERO_CL_OBSERVATIONS_SLOT, AERO_CL_REWARD_GROWTH_SLOT, AERO_CL_ROLLOVER_SLOT,
    AERO_CL_SECONDS_PER_STAKED_LIQUIDITY_SLOT, AERO_CL_STAKED_LIQUIDITY_SLOT, SolidlyStorageLayout,
    V3StorageLayout,
};
use evm_amm_state::adapters::v3_sync::{V3PoolSnapshot, V3SyncSpec, run_full_sync};
use evm_amm_state::adapters::{
    AdapterCache, AdapterEventContext, AdapterRegistry, AmmAdapter, CacheError, CallOutcome,
    CallTxContext, ColdStartOutcome, ColdStartPolicy, ConcentratedLiquidityAdapter, PoolKey,
    PoolRegistration, ProtocolMetadata, RepairAction, SimConfig, SlipstreamRuntimeFamily,
    SlipstreamSnapshotIdentity, SlipstreamSwapFeeEvidence, SlotChange, SolidlyV2Adapter,
    SolidlyV2Metadata, StateDiff, StateUpdate, StateView, UpdateQuality, V3Metadata,
    overlay_call_raw_with_context,
};
use evm_fork_cache::bulk_storage::{BulkCallConfig, fetch_slots_bulk};
use evm_fork_cache::cache::{EvmCache, EvmOverlay, EvmSnapshot};

alloy_sol_types::sol! {
    interface IMetaquoter {
        function quoteExactInputSingleV3(address pool, address tokenIn, uint256 amountIn, uint160 sqrtPriceLimitX96)
            returns (uint256 amountOut, uint160 sqrtPriceX96After, uint32 initializedTicksCrossed);
        function quoteExactOutputSingleV3(address pool, address tokenIn, uint256 amountOut, uint160 sqrtPriceLimitX96)
            returns (uint256 amountIn, uint160 sqrtPriceX96After, uint32 initializedTicksCrossed);
    }
    interface IAeroPool {
        function getAmountOutWithTotalFee(uint256 amountIn, address tokenIn) returns (uint256);
        function getAmountInWithTotalFee(uint256 amountOut, address tokenOut) returns (uint256);
    }
    event Swap(address indexed sender, address indexed recipient, int256 amount0, int256 amount1, uint160 sqrtPriceX96, uint128 liquidity, int24 tick);
    event Mint(address sender, address indexed owner, int24 indexed tickLower, int24 indexed tickUpper, uint128 amount, uint256 amount0, uint256 amount1);
    event Burn(address indexed owner, int24 indexed tickLower, int24 indexed tickUpper, uint128 amount, uint256 amount0, uint256 amount1);
    event Collect(address indexed owner, address recipient, int24 indexed tickLower, int24 indexed tickUpper, uint128 amount0, uint128 amount1);
    event CollectFees(address indexed recipient, uint128 amount0, uint128 amount1);
    event Deposit(address indexed sender, address indexed owner, uint256 indexed tokenId, uint128 liquidityToStake);
    event Withdraw(address indexed sender, address indexed account, uint256 indexed tokenId, uint128 liquidityToStake);
    event GaugeSettled(uint256 cumulativeRewardShare, uint256 delta);
    event Transfer(address indexed from, address indexed to, uint256 indexed tokenId);
    event Sync(uint256 reserve0, uint256 reserve1);
    event EmissionsClaimed(address indexed account, uint256 indexed tokenId, address indexed recipient, uint256 reward);
    event ClaimFees(address indexed from, uint256 claimed0, uint256 claimed1);
    event Fees(address indexed sender, uint256 amount0, uint256 amount1);
    event CustomFeeSet(address indexed pool, uint24 fee);
    event FeeCapSet(address indexed pool, uint256 feeCap);
    event ScalingFactorSet(address indexed pool, uint256 scalingFactor);
    event DefaultScalingFactorSet(uint256 defaultScalingFactor);
    event DefaultFeeCapSet(uint256 defaultFeeCap);
    event SecondsAgoSet(uint32 secondsAgo);
    event RecordCallGasLimitSet(uint32 gasLimit);
    event MevTaxModuleSet(address indexed mevTaxModule);
    event MultiplierSet(uint64 priorityFeeMultiplier);
    event MinThresholdSet(uint96 minThreshold);
    event BaseFeeFactorSet(uint96 baseFeeFactor);
    event TickSpacingEnabled(int24 indexed tickSpacing, uint24 indexed fee);
    event DefaultUnstakedFeeChanged(uint24 indexed oldUnstakedFee, uint24 indexed newUnstakedFee);
    event PoolCreated(address indexed token0, address indexed token1, int24 indexed tickSpacing, address pool);
    event DiscountSet(address indexed account, uint24 discount);
    event PoolDiscountSet(address indexed pool, address indexed account, uint24 discount);
}

mod volatile_abi {
    alloy_sol_types::sol! {
        event Swap(address indexed sender, address indexed to, uint256 amount0In, uint256 amount1In, uint256 amount0Out, uint256 amount1Out);
    }
}
use volatile_abi::Swap as VolatileSwap;

const METAQUOTER: Address = address!("1dDfeC5058180C207F55BB77D95AcDd0d8227868");
/// The fresh EOA the design evidence quoted from (tx.origin of every quote).
const EOA: Address = address!("1111111111111111111111111111111111111111");
const ETH_POOL: Address = address!("ff35aa95669b5668AC6727FdcFc0D5582cdd78D1");
const ETH_GAUGE: Address = address!("0ee89bdfd62e3a5325416fb93c0f1601474a5b04");
const ETH_FACTORY: Address = address!("f4fd2e6c5b60c5b365cd1b96dd6245e6d60414c2");
const BIFI: Address = address!("B1F1ee126e9c96231Cc3d3fAD7C08b4cf873b1f1");
const ETH_WETH: Address = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
const BASE_POOL: Address = address!("EEBE78d734dC4bAC027061cEE9bc12C5803fE4f8");
const BASE_GAUGE: Address = address!("84c029f26652d37bc153c39deb8767ec54037334");
const BASE_FACTORY: Address = address!("9cb220581b91b0c83223012fe69688b92ad07a76");
const BASE_MEV_MODULE: Address = address!("29f12ce5be3d88b56d245b7f4e969c518f799a4a");
const L2_WETH: Address = address!("4200000000000000000000000000000000000006");
const MOO_BIFI: Address = address!("c55E93C62874D8100dBd2DfE307EDc1036ad5434");
const OP_POOL: Address = address!("C71B571aA9F06BFA096c24e9Df5C45fC4fAB4151");
const OP_FACTORY: Address = address!("8e6a9fcd1d638cc760ffba397605fe3355de76d2");
const OP_FEE_MODULE: Address = address!("7949de8d67e4f6b8509bdbb43ff0503af1247a7a");
const OP_MEV_MODULE: Address = address!("5de55ebe5b789bd309809c7969334c2c16e64eaf");
const AERO_NFT: Address = address!("4857db576ee0e45f10525ec59527bfd10beec60b");
const AERO_HOOK: Address = address!("c0ed888afe87ed1acf01d9f19bb850159e3ce434");
const AERO_DISCOUNT_REGISTRY: Address = address!("aa48aaef655fb7cd7acc04717f66022a4771a345");
const AERO_POOL_TAPE: Address = address!("27d5eb73e7406675f6b0cbd037490ec3dba935a4");
const AERO_VOTER: Address = address!("382409fad8503fbcdbb27a4211551fa03e884078");
const AERO_GAUGE_FACTORY: Address = address!("f437f62053acb121665b8fdbfea8f214fb8e589d");
const AERO_GAUGE_IMPLEMENTATION: Address = address!("89e91a960154b1ed6c093039a8f0725ab67d14e0");
const AERO_UNSTAKED_FEE_MODULE: Address = address!("cc421483cd131a8fd6b68c37e46f244dbec4f797");
const AERO_POOL_PROXY_RUNTIME_CODE_HASH: B256 =
    b256!("55e3735acd886d76160bfd959db31dd476e73e46e8a0d70ac36e4c53ef5a2a45");
const AERO_CL_IMPLEMENTATION: Address = address!("1642634c8cb1c9f1b84c9a1a08534f8c3f4e89f8");
const AERO_CL_IMPLEMENTATION_RUNTIME_CODE_HASH: B256 =
    b256!("041172c7cbffc45627f9dd52727a0124cd6eeb2b5830ab392df54f71a6948693");
const AERO_SETTLEMENT_PATH: [Address; 2] = [
    address!("0575b823c22585f1828bbc133d5e522a8499165c"),
    address!("c7e262e848fca78c3c66ee3c9835e4e65e3e0398"),
];
/// `blockFee` mapping slot on the Aero swap hook.
const HOOK_BLOCK_FEE_SLOT: u64 = 4;
/// `dynamicFeeConfig` mapping slot on the Aero swap hook.
const HOOK_DYNAMIC_FEE_CONFIG_SLOT: u64 = 3;
/// Synthetic dynamic-fee configuration (a live Base configuration: 5% cap,
/// 100 pips per tick of deviation from the 600 s time-weighted tick).
const DYNAMIC_FEE_CAP: u64 = 50_000;
const DYNAMIC_SCALING_FACTOR: u64 = 100_000_000;
const GWEI: u128 = 1_000_000_000;

fn env_u64(name: &str) -> Option<u64> {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
}

#[path = "support/aero_rpc.rs"]
mod aero_rpc;
use aero_rpc::{derived_url, eth_url};

/// An HTTP provider that backs off and retries on rate limits and transient
/// transport errors (a long campaign issues tens of thousands of requests).
fn connect(url: &str) -> Result<RootProvider<AnyNetwork>> {
    let client = alloy_rpc_client::ClientBuilder::default()
        .layer(alloy_transport::layers::RetryBackoffLayer::new(
            12, 400, 500,
        ))
        .http(url.parse().context("parse RPC url")?);
    Ok(RootProvider::<AnyNetwork>::new(client))
}

fn report_dir() -> std::path::PathBuf {
    std::env::var("AERO_CAMPAIGN_REPORT_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("target/aero-campaign"))
}

fn quotes_enabled() -> bool {
    env_u64("AERO_CAMPAIGN_QUOTES").unwrap_or(1) != 0
}

fn stride() -> u64 {
    env_u64("AERO_CAMPAIGN_STRIDE").unwrap_or(100).max(1)
}

async fn window(
    provider: &RootProvider<AnyNetwork>,
    prefix: &str,
    default_blocks: u64,
) -> Result<(u64, u64)> {
    let head = provider.get_block_number().await?;
    let end = env_u64(&format!("AERO_CAMPAIGN_{prefix}_END")).unwrap_or(head.saturating_sub(32));
    let blocks = env_u64(&format!("AERO_CAMPAIGN_{prefix}_BLOCKS")).unwrap_or(default_blocks);
    let start = env_u64(&format!("AERO_CAMPAIGN_{prefix}_START")).unwrap_or(end - blocks);
    ensure!(start < end, "window must be non-empty");
    Ok((start, end))
}

// ---------------------------------------------------------------------------
// Report
// ---------------------------------------------------------------------------

#[derive(Default)]
struct ClassStat {
    first: Option<(u64, String)>,
    divergent_checks: u64,
    cells: u64,
    /// Longest run of blocks a divergence of this class persisted before the
    /// campaign re-synchronised (measured between consecutive checks).
    persistence_max: u64,
}

#[derive(Default)]
struct Report {
    chain: String,
    pool: String,
    start: u64,
    end: u64,
    stride: u64,
    events_by_topic: BTreeMap<String, u64>,
    event_blocks: u64,
    blocks_checked: u64,
    cells_compared: u64,
    adapter_repairs: u64,
    adapter_repair_first: Option<String>,
    evidence_failures: u64,
    evidence_failure_first: Option<String>,
    resyncs: u64,
    causes: BTreeMap<String, u64>,
    classes: BTreeMap<String, ClassStat>,
    external: BTreeMap<String, ClassStat>,
    quotes: u64,
    parity_ok: u64,
    parity_mismatch: u64,
    both_reverted: u64,
    dynamic_quotes: u64,
    dynamic_parity_ok: u64,
    parity_first_mismatch: Option<String>,
    strict_failures: u64,
    strict_misses: BTreeMap<String, u64>,
    strict_miss_first: BTreeMap<String, String>,
    block_fee_checks: u64,
    block_fee_nonzero: u64,
    block_fee_cache_match: u64,
    block_fee_cache_mismatch: u64,
    block_fee_cache_absent: u64,
    notes: Vec<String>,
}

impl Report {
    fn class(&mut self, name: &str, block: u64, detail: String, persistence: u64) {
        let stat = self.classes.entry(name.to_owned()).or_default();
        if stat.first.is_none() {
            stat.first = Some((block, detail));
        }
        stat.cells += 1;
        stat.persistence_max = stat.persistence_max.max(persistence);
    }

    fn external_cell(&mut self, name: &str, block: u64, detail: String, persistence: u64) {
        let stat = self.external.entry(name.to_owned()).or_default();
        if stat.first.is_none() {
            stat.first = Some((block, detail));
        }
        stat.cells += 1;
        stat.persistence_max = stat.persistence_max.max(persistence);
    }

    fn miss(&mut self, class: String, detail: String) {
        *self.strict_misses.entry(class.clone()).or_default() += 1;
        self.strict_miss_first.entry(class).or_insert(detail);
    }

    fn to_json(&self) -> Value {
        let classes: BTreeMap<&str, Value> = self
            .classes
            .iter()
            .map(|(name, stat)| (name.as_str(), stat_json(stat)))
            .collect();
        let external: BTreeMap<&str, Value> = self
            .external
            .iter()
            .map(|(name, stat)| (name.as_str(), stat_json(stat)))
            .collect();
        json!({
            "chain": self.chain,
            "pool": self.pool,
            "window": {"start": self.start, "end": self.end, "blocks": self.end - self.start, "stride": self.stride},
            "events_by_topic": self.events_by_topic,
            "event_blocks": self.event_blocks,
            "blocks_checked": self.blocks_checked,
            "cells_compared": self.cells_compared,
            "adapter_repairs": self.adapter_repairs,
            "adapter_repair_first": self.adapter_repair_first,
            "swap_evidence_failures": self.evidence_failures,
            "swap_evidence_failure_first": self.evidence_failure_first,
            "campaign_resyncs": self.resyncs,
            "divergence_causes": self.causes,
            "divergence_classes": classes,
            "external_cell_divergences": external,
            "quotes": self.quotes,
            "parity_ok": self.parity_ok,
            "parity_mismatch": self.parity_mismatch,
            "both_reverted": self.both_reverted,
            "dynamic_fee_quotes": self.dynamic_quotes,
            "dynamic_fee_parity_ok": self.dynamic_parity_ok,
            "parity_first_mismatch": self.parity_first_mismatch,
            "strict_quote_failures": self.strict_failures,
            "strict_misses": self.strict_misses,
            "strict_miss_first": self.strict_miss_first,
            "block_fee": {
                "checks": self.block_fee_checks,
                "nonzero_on_chain": self.block_fee_nonzero,
                "cache_match": self.block_fee_cache_match,
                "cache_mismatch": self.block_fee_cache_mismatch,
                "cache_absent": self.block_fee_cache_absent,
            },
            "notes": self.notes,
        })
    }

    fn markdown(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "## {} `{}` window {}..={} ({} blocks, stride {})\n\n",
            self.chain,
            self.pool,
            self.start,
            self.end,
            self.end - self.start,
            self.stride
        ));
        out.push_str("| metric | value |\n|---|---|\n");
        out.push_str(&format!("| event blocks | {} |\n", self.event_blocks));
        for (topic, count) in &self.events_by_topic {
            out.push_str(&format!("| events `{topic}` | {count} |\n"));
        }
        out.push_str(&format!("| blocks checked | {} |\n", self.blocks_checked));
        out.push_str(&format!("| cells compared | {} |\n", self.cells_compared));
        out.push_str(&format!("| adapter repairs | {} |\n", self.adapter_repairs));
        out.push_str(&format!(
            "| swap evidence failures | {} |\n",
            self.evidence_failures
        ));
        out.push_str(&format!(
            "| campaign re-syncs (divergence) | {} |\n",
            self.resyncs
        ));
        for (cause, count) in &self.causes {
            out.push_str(&format!("| divergence cause `{cause}` | {count} |\n"));
        }
        out.push_str(&format!("| quotes | {} |\n", self.quotes));
        out.push_str(&format!(
            "| parity ok / mismatch / both reverted | {} / {} / {} |\n",
            self.parity_ok, self.parity_mismatch, self.both_reverted
        ));
        out.push_str(&format!(
            "| dynamic-fee (K≠0) quotes / parity ok | {} / {} |\n",
            self.dynamic_quotes, self.dynamic_parity_ok
        ));
        out.push_str(&format!(
            "| strict quote failures | {} |\n",
            self.strict_failures
        ));
        out.push_str(&format!(
            "| blockFee checks / non-zero / cache match / mismatch / absent | {} / {} / {} / {} / {} |\n",
            self.block_fee_checks,
            self.block_fee_nonzero,
            self.block_fee_cache_match,
            self.block_fee_cache_mismatch,
            self.block_fee_cache_absent
        ));
        if !self.classes.is_empty() {
            out.push_str("\n| divergence class | divergent checks | cells | first block | max persistence (blocks) | first detail |\n|---|---|---|---|---|---|\n");
            for (name, stat) in &self.classes {
                let (block, detail) = stat
                    .first
                    .clone()
                    .map(|(block, detail)| (block.to_string(), detail))
                    .unwrap_or_default();
                out.push_str(&format!(
                    "| {name} | {} | {} | {block} | {} | {detail} |\n",
                    stat.divergent_checks, stat.cells, stat.persistence_max
                ));
            }
        }
        if !self.external.is_empty() {
            out.push_str("\n| external cell divergence | divergent checks | cells | first block | max persistence | first detail |\n|---|---|---|---|---|---|\n");
            for (name, stat) in &self.external {
                let (block, detail) = stat
                    .first
                    .clone()
                    .map(|(block, detail)| (block.to_string(), detail))
                    .unwrap_or_default();
                out.push_str(&format!(
                    "| {name} | {} | {} | {block} | {} | {detail} |\n",
                    stat.divergent_checks, stat.cells, stat.persistence_max
                ));
            }
        }
        if !self.strict_misses.is_empty() {
            out.push_str("\n| strict miss class | count | first |\n|---|---|---|\n");
            for (name, count) in &self.strict_misses {
                out.push_str(&format!(
                    "| {name} | {count} | {} |\n",
                    self.strict_miss_first
                        .get(name)
                        .cloned()
                        .unwrap_or_default()
                ));
            }
        }
        if let Some(first) = &self.parity_first_mismatch {
            out.push_str(&format!("\nFirst parity mismatch: {first}\n"));
        }
        if let Some(first) = &self.adapter_repair_first {
            out.push_str(&format!("\nFirst adapter repair: {first}\n"));
        }
        if let Some(first) = &self.evidence_failure_first {
            out.push_str(&format!("\nFirst swap evidence failure: {first}\n"));
        }
        for note in &self.notes {
            out.push_str(&format!("\n- {note}\n"));
        }
        out
    }

    fn write(&self) -> Result<()> {
        let dir = report_dir();
        std::fs::create_dir_all(&dir)?;
        let stem = format!("aero-campaign-{}", self.chain.to_lowercase());
        std::fs::write(
            dir.join(format!("{stem}.json")),
            serde_json::to_string_pretty(&self.to_json())?,
        )?;
        std::fs::write(dir.join(format!("{stem}.md")), self.markdown())?;
        eprintln!("{}", self.markdown());
        eprintln!("report written to {}", dir.display());
        Ok(())
    }
}

fn stat_json(stat: &ClassStat) -> Value {
    json!({
        "divergent_checks": stat.divergent_checks,
        "cells": stat.cells,
        "first_block": stat.first.as_ref().map(|(block, _)| *block),
        "first_detail": stat.first.as_ref().map(|(_, detail)| detail.clone()),
        "max_persistence_blocks": stat.persistence_max,
    })
}

// ---------------------------------------------------------------------------
// Strict (no provider, no lazy fetch) quote cache over an immutable snapshot
// ---------------------------------------------------------------------------

struct StrictCache {
    snapshot: Arc<EvmSnapshot>,
    overlay: EvmOverlay,
}

impl StrictCache {
    fn new(snapshot: Arc<EvmSnapshot>) -> Self {
        Self {
            overlay: EvmOverlay::new(Arc::clone(&snapshot), None),
            snapshot,
        }
    }
}

impl StateView for StrictCache {
    fn storage(&self, address: Address, slot: U256) -> Option<U256> {
        self.snapshot.storage_value(address, slot)
    }
}

fn strict_error(what: &str) -> CacheError {
    CacheError::Backend(Box::new(std::io::Error::other(format!(
        "{what} is unavailable on the strict campaign cache"
    ))))
}

impl AdapterCache for StrictCache {
    fn cached_storage(&self, address: Address, slot: U256) -> Option<U256> {
        self.snapshot.storage_value(address, slot)
    }

    fn apply_updates(&mut self, _updates: &[StateUpdate]) -> StateDiff {
        StateDiff::default()
    }

    fn verify_slots(&mut self, _slots: &[(Address, U256)]) -> Result<Vec<SlotChange>, CacheError> {
        Err(strict_error("verify_slots"))
    }

    fn purge_storage(&mut self, _address: Address) -> StateDiff {
        StateDiff::default()
    }

    fn purge_slots(&mut self, _address: Address, _slots: &[U256]) -> StateDiff {
        StateDiff::default()
    }

    fn read_storage_slot(&mut self, address: Address, slot: U256) -> Result<U256, CacheError> {
        self.snapshot
            .storage_value(address, slot)
            .ok_or_else(|| strict_error("lazy storage read"))
    }

    fn call_raw(
        &mut self,
        from: Address,
        to: Address,
        calldata: Bytes,
        _commit: bool,
    ) -> Result<CallOutcome, CacheError> {
        self.overlay
            .call_raw(from, to, calldata)
            .map(CallOutcome::from)
            .map_err(|error| CacheError::Backend(Box::new(error)))
    }

    fn call_raw_with_code_overrides(
        &mut self,
        from: Address,
        to: Address,
        calldata: Bytes,
        code_overrides: &[(Address, Bytes)],
        _commit: bool,
    ) -> Result<CallOutcome, CacheError> {
        self.overlay
            .call_raw_with_code_overrides(from, to, calldata, code_overrides)
            .map(CallOutcome::from)
            .map_err(|error| CacheError::Backend(Box::new(error)))
    }

    fn call_raw_with_context(
        &mut self,
        from: Address,
        to: Address,
        calldata: Bytes,
        code_overrides: &[(Address, Bytes)],
        context: &CallTxContext,
        _commit: bool,
    ) -> Result<CallOutcome, CacheError> {
        overlay_call_raw_with_context(
            &mut self.overlay,
            from,
            to,
            calldata,
            code_overrides,
            context,
        )
    }
}

// ---------------------------------------------------------------------------
// Chain plumbing
// ---------------------------------------------------------------------------

struct Header {
    hash: B256,
    parent: B256,
    timestamp: u64,
    basefee: u128,
    beneficiary: Address,
}

/// Fetch the header of `number` and, when `advance` is set, pin the cache to
/// the block's hash with its number, timestamp and basefee, and record the
/// parent hash (the exact-lineage identity the fee evidence binds to) without
/// touching state.
async fn advance_to(
    provider: &RootProvider<AnyNetwork>,
    cache: &mut EvmCache,
    number: u64,
    advance: bool,
) -> Result<Header> {
    let block = provider
        .get_block_by_number(BlockNumberOrTag::Number(number))
        .await?
        .ok_or_else(|| anyhow!("block {number} missing"))?;
    let header = Header {
        hash: block.header.hash,
        parent: block.header.parent_hash,
        timestamp: block.header.timestamp,
        basefee: block.header.base_fee_per_gas.unwrap_or_default() as u128,
        beneficiary: block.header.beneficiary,
    };
    if advance {
        cache.set_block(BlockId::from((header.hash, Some(true))));
        cache.set_timestamp(Some(header.timestamp));
        cache.set_block_context(Some(number), block.header.base_fee_per_gas);
        cache
            .db_mut()
            .cache
            .block_hashes
            .insert(U256::from(number - 1), header.parent);
    }
    // The block's fee recipient is loaded by every simulated call; a strict
    // snapshot quote needs its header resident.
    cache
        .ensure_account(header.beneficiary)
        .await
        .map_err(|error| anyhow!("seed coinbase {}: {error}", header.beneficiary))?;
    Ok(header)
}

/// Fetch every log matching `base` (address / topic filters) over
/// `start..=end`, splitting the range adaptively around the provider's
/// 10 000-result cap.
async fn fetch_logs(
    provider: &RootProvider<AnyNetwork>,
    base: Filter,
    start: u64,
    end: u64,
) -> Result<Vec<RpcLog>> {
    let mut logs = Vec::new();
    let mut pending = vec![(start, end)];
    while let Some((from, to)) = pending.pop() {
        let filter = base.clone().from_block(from).to_block(to);
        match provider.get_logs(&filter).await {
            Ok(chunk) if chunk.len() < 10_000 => logs.extend(chunk),
            Ok(_) | Err(_) if from < to => {
                let middle = from + (to - from) / 2;
                pending.push((middle + 1, to));
                pending.push((from, middle));
            }
            Ok(chunk) => logs.extend(chunk),
            Err(error) => return Err(anyhow!("eth_getLogs {from}..={to}: {error}")),
        }
    }
    logs.sort_by_key(|log| (log.block_number, log.log_index));
    Ok(logs)
}

/// `eth_call` at `block` from the campaign EOA under `gas_price`; `None` when
/// the call reverted (a quote the pool cannot fill), an error otherwise.
async fn eth_call(
    provider: &RootProvider<AnyNetwork>,
    target: Address,
    calldata: Bytes,
    gas_price: u128,
    block: u64,
    overrides: Option<StateOverride>,
) -> Result<Option<Bytes>> {
    let tx = TransactionRequest::default()
        .with_from(EOA)
        .with_to(target)
        .with_input(calldata)
        .with_gas_price(gas_price);
    let call = provider
        .call(tx.into())
        .block(BlockId::Number(BlockNumberOrTag::Number(block)));
    let result = match overrides {
        Some(overrides) => call.overrides(overrides).await,
        None => call.await,
    };
    match result {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.to_string().contains("revert") => Ok(None),
        Err(error) => Err(anyhow!("eth_call at {block}: {error}")),
    }
}

async fn storage_at(
    provider: &RootProvider<AnyNetwork>,
    address: Address,
    slot: U256,
    block: u64,
) -> Result<U256> {
    Ok(provider
        .get_storage_at(address, slot)
        .block_id(BlockId::Number(BlockNumberOrTag::Number(block)))
        .await?)
}

fn mapping_key(key: &[u8], slot: u64) -> U256 {
    let mut preimage = [0_u8; 64];
    preimage[32 - key.len()..32].copy_from_slice(key);
    preimage[32..].copy_from_slice(&U256::from(slot).to_be_bytes::<32>());
    U256::from_be_bytes(alloy_primitives::keccak256(preimage).0)
}

fn block_fee_key(pool: Address, block: u64) -> U256 {
    let outer = mapping_key(pool.as_slice(), HOOK_BLOCK_FEE_SLOT);
    let mut preimage = [0_u8; 64];
    preimage[..32].copy_from_slice(&U256::from(block).to_be_bytes::<32>());
    preimage[32..].copy_from_slice(&outer.to_be_bytes::<32>());
    U256::from_be_bytes(alloy_primitives::keccak256(preimage).0)
}

fn topic_name(topic: &B256) -> String {
    let known: [(B256, &str); 30] = [
        (Swap::SIGNATURE_HASH, "Swap"),
        (Mint::SIGNATURE_HASH, "Mint"),
        (Burn::SIGNATURE_HASH, "Burn"),
        (Collect::SIGNATURE_HASH, "Collect"),
        (CollectFees::SIGNATURE_HASH, "CollectFees"),
        (Deposit::SIGNATURE_HASH, "Deposit"),
        (Withdraw::SIGNATURE_HASH, "Withdraw"),
        (GaugeSettled::SIGNATURE_HASH, "GaugeSettled"),
        (EmissionsClaimed::SIGNATURE_HASH, "EmissionsClaimed"),
        (ClaimFees::SIGNATURE_HASH, "ClaimFees"),
        (Transfer::SIGNATURE_HASH, "Transfer"),
        (Sync::SIGNATURE_HASH, "Sync"),
        (VolatileSwap::SIGNATURE_HASH, "Swap"),
        (Fees::SIGNATURE_HASH, "Fees"),
        (CustomFeeSet::SIGNATURE_HASH, "CustomFeeSet"),
        (FeeCapSet::SIGNATURE_HASH, "FeeCapSet"),
        (ScalingFactorSet::SIGNATURE_HASH, "ScalingFactorSet"),
        (
            DefaultScalingFactorSet::SIGNATURE_HASH,
            "DefaultScalingFactorSet",
        ),
        (DefaultFeeCapSet::SIGNATURE_HASH, "DefaultFeeCapSet"),
        (SecondsAgoSet::SIGNATURE_HASH, "SecondsAgoSet"),
        (
            RecordCallGasLimitSet::SIGNATURE_HASH,
            "RecordCallGasLimitSet",
        ),
        (MevTaxModuleSet::SIGNATURE_HASH, "MevTaxModuleSet"),
        (MultiplierSet::SIGNATURE_HASH, "MultiplierSet"),
        (MinThresholdSet::SIGNATURE_HASH, "MinThresholdSet"),
        (BaseFeeFactorSet::SIGNATURE_HASH, "BaseFeeFactorSet"),
        (TickSpacingEnabled::SIGNATURE_HASH, "TickSpacingEnabled"),
        (
            DefaultUnstakedFeeChanged::SIGNATURE_HASH,
            "DefaultUnstakedFeeChanged",
        ),
        (PoolCreated::SIGNATURE_HASH, "PoolCreated"),
        (DiscountSet::SIGNATURE_HASH, "DiscountSet"),
        (PoolDiscountSet::SIGNATURE_HASH, "PoolDiscountSet"),
    ];
    known
        .iter()
        .find(|(hash, _)| hash == topic)
        .map(|(_, name)| (*name).to_owned())
        .unwrap_or_else(|| {
            format!(
                "{:#x}",
                &topic.as_slice()[..4]
                    .iter()
                    .fold(0_u32, |acc, b| acc << 8 | *b as u32)
            )
        })
}

/// Hook / factory / module events scoped to a pool by `topic1`.
fn pool_scoped_topics() -> [B256; 4] {
    [
        CustomFeeSet::SIGNATURE_HASH,
        FeeCapSet::SIGNATURE_HASH,
        ScalingFactorSet::SIGNATURE_HASH,
        PoolCreated::SIGNATURE_HASH,
    ]
}

fn event_label(log: &RpcLog, pool: Address, gauge: Address) -> String {
    let mut topic = log.topics().first().map(topic_name).unwrap_or_default();
    if log
        .topics()
        .first()
        .is_some_and(|t| pool_scoped_topics().contains(t))
        && log.topics().get(1) != Some(&pool.into_word())
    {
        topic.push_str(" (other pool)");
    }
    let emitter = if log.address() == pool {
        "pool"
    } else if log.address() == gauge {
        "gauge"
    } else if log.address() == AERO_NFT {
        "nft"
    } else if log.address() == AERO_HOOK {
        "hook"
    } else if log.address() == AERO_DISCOUNT_REGISTRY {
        "registry"
    } else if log.address() == BASE_MEV_MODULE || log.address() == OP_MEV_MODULE {
        "mev"
    } else if log.address() == OP_FEE_MODULE {
        "fee-module"
    } else {
        "factory"
    };
    format!("{emitter} {topic}")
}

fn classify_miss(
    address: Address,
    slot: U256,
    pool: Address,
    gauge: Address,
    block: u64,
) -> String {
    if address == AERO_HOOK {
        if slot == block_fee_key(pool, block) {
            "hook blockFee[pool][block]".to_owned()
        } else {
            "hook config".to_owned()
        }
    } else if address == AERO_POOL_TAPE {
        "pool tape".to_owned()
    } else if address == gauge
        || address == AERO_GAUGE_IMPLEMENTATION
        || address == AERO_VOTER
        || address == AERO_GAUGE_FACTORY
        || AERO_SETTLEMENT_PATH.contains(&address)
    {
        "gauge settlement path".to_owned()
    } else if address == AERO_DISCOUNT_REGISTRY {
        "discount registry".to_owned()
    } else if address == pool {
        if slot >= AERO_CL_OBSERVATIONS_SLOT
            && slot < AERO_CL_OBSERVATIONS_SLOT + U256::from(65_535)
        {
            "pool observation ring".to_owned()
        } else {
            "pool other".to_owned()
        }
    } else if address == AERO_UNSTAKED_FEE_MODULE {
        "unstaked fee module".to_owned()
    } else {
        format!("other {address}")
    }
}

fn external_class(address: Address, gauge: Address) -> String {
    if address == AERO_HOOK {
        "hook".to_owned()
    } else if address == AERO_POOL_TAPE {
        "pool tape".to_owned()
    } else if address == AERO_DISCOUNT_REGISTRY {
        "discount registry".to_owned()
    } else if address == BASE_MEV_MODULE || address == OP_MEV_MODULE {
        "mev module".to_owned()
    } else if address == gauge || address == AERO_VOTER || address == AERO_GAUGE_FACTORY {
        "gauge / voter".to_owned()
    } else if address == AERO_UNSTAKED_FEE_MODULE || address == OP_FEE_MODULE {
        "fee module".to_owned()
    } else {
        "factory".to_owned()
    }
}

// ---------------------------------------------------------------------------
// CL campaign
// ---------------------------------------------------------------------------

struct ClChain {
    name: &'static str,
    pool: Address,
    gauge: Address,
    factory: Address,
    mev_module: Option<Address>,
    spacing: i32,
    token0: Address,
    token1: Address,
    /// The BIFI-side token and its mooBIFI-equivalent sizes.
    basis: Address,
    sizes: [u128; 2],
    tips: [u128; 2],
    default_blocks: u64,
    env_prefix: &'static str,
}

fn cl_registration(chain: &ClChain) -> PoolRegistration {
    PoolRegistration::new(PoolKey::Slipstream(chain.pool))
        .with_state_address(chain.pool)
        .with_metadata(ProtocolMetadata::Slipstream(
            V3Metadata::default()
                .with_token0(chain.token0)
                .with_token1(chain.token1)
                .with_tick_spacing(chain.spacing)
                .with_storage_layout(V3StorageLayout::aero_cl(chain.spacing))
                .with_metaquoter(METAQUOTER),
        ))
}

/// Which cell class a pool slot belongs to.
fn pool_cell_class(
    slot: U256,
    layout: V3StorageLayout,
    ticks: &BTreeMap<U256, (i32, usize)>,
    bitmap: &BTreeSet<U256>,
) -> &'static str {
    if slot == layout.slot0_slot {
        "slot0"
    } else if slot == layout.liquidity_slot {
        "liquidity"
    } else if slot == AERO_CL_STAKED_LIQUIDITY_SLOT {
        "staked word"
    } else if slot == AERO_CL_FEE_GROWTH_0_SLOT || slot == AERO_CL_FEE_GROWTH_1_SLOT {
        "feeGrowthGlobal"
    } else if slot == AERO_CL_REWARD_GROWTH_SLOT {
        "rewardGrowthGlobalX128"
    } else if slot == AERO_CL_GAUGE_FEES_SLOT {
        "gaugeFees"
    } else if slot == AERO_CL_ROLLOVER_SLOT {
        "rollover"
    } else if slot == AERO_CL_SECONDS_PER_STAKED_LIQUIDITY_SLOT {
        "secondsPerStakedLiquidityCumulativeX128"
    } else if slot >= AERO_CL_OBSERVATIONS_SLOT
        && slot < AERO_CL_OBSERVATIONS_SLOT + U256::from(65_535)
    {
        "observation ring"
    } else if bitmap.contains(&slot) {
        "tick bitmap"
    } else if let Some((_, word)) = ticks.get(&slot) {
        match word {
            0 => "tick word 0 (liquidityGross|liquidityNet)",
            1 => "tick word 1 (stakedLiquidityNet)",
            2 | 3 => "tick words 2-3 (feeGrowthOutside)",
            4 => "tick word 4 (rewardGrowthOutside)",
            _ => "tick word 5 (seconds|tickCumulative outside, initialized)",
        }
    } else {
        "pool other"
    }
}

/// Compare the whole quote-relevant pool state of `cache` with the chain at
/// `block`; returns the divergent `(class, slot, cached, chain)` cells and the
/// snapshot (for re-synchronisation).
async fn compare_cl_state(
    provider: &RootProvider<AnyNetwork>,
    cache: &EvmCache,
    pool: Address,
    spec: &V3SyncSpec,
    block: u64,
) -> Result<(
    Vec<(&'static str, U256, Option<U256>, U256)>,
    V3PoolSnapshot,
    usize,
)> {
    let snapshot = run_full_sync(
        provider,
        BlockId::Number(BlockNumberOrTag::Number(block)),
        pool,
        spec,
    )
    .await
    .map_err(|error| anyhow!("full sync at {block}: {error}"))?;
    let layout = spec.layout;
    let mut tick_slots = BTreeMap::new();
    for tick in &snapshot.ticks {
        let base = evm_amm_state::adapters::storage::slipstream_tick_info_storage_keys_with_base(
            tick.tick,
            layout.ticks_base_slot,
        );
        for (word, key) in base.iter().enumerate() {
            tick_slots.insert(*key, (tick.tick, word));
        }
    }
    let mut bitmap_slots = BTreeSet::new();
    for word in spec.min_word..=spec.max_word {
        bitmap_slots.insert(
            evm_amm_state::adapters::storage::v3_tick_bitmap_storage_key_with_base(
                word,
                layout.tick_bitmap_base_slot,
            ),
        );
    }
    let entries = snapshot.storage_entries(spec);
    let mut divergent = Vec::new();
    for (slot, chain_value) in &entries {
        let cached = cache.cached_storage_value(pool, *slot);
        if cached != Some(*chain_value) {
            divergent.push((
                pool_cell_class(*slot, layout, &tick_slots, &bitmap_slots),
                *slot,
                cached,
                *chain_value,
            ));
        }
    }
    Ok((divergent, snapshot, entries.len()))
}

async fn compare_external(
    provider: &RootProvider<AnyNetwork>,
    cache: &EvmCache,
    targets: &[(Address, U256)],
    block: u64,
) -> Result<Vec<(Address, U256, Option<U256>, U256)>> {
    // Pool-tape records, the voter's gauge word and the gauge factory's
    // record are written by every swap / settlement and are not read by a
    // quote (the quote path stubs the tape and the gauge); they are warmed
    // for the live path only and never compared here.
    let targets: Vec<(Address, U256)> = targets
        .iter()
        .copied()
        .filter(|(address, _)| {
            ![AERO_POOL_TAPE, AERO_VOTER, AERO_GAUGE_FACTORY].contains(address)
                && !AERO_SETTLEMENT_PATH.contains(address)
        })
        .collect();
    let fetched = fetch_slots_bulk(
        provider,
        targets,
        BlockId::Number(BlockNumberOrTag::Number(block)),
        BulkCallConfig::default(),
    )
    .await;
    let mut divergent = Vec::new();
    for (address, slot, result) in fetched {
        let chain_value = result.map_err(|error| {
            anyhow!("external cell {address}:{slot:#x} unreadable at {block}: {error}")
        })?;
        let cached = cache.cached_storage_value(address, slot);
        if cached != Some(chain_value) {
            divergent.push((address, slot, cached, chain_value));
        }
    }
    Ok(divergent)
}

/// Write the chain's pool snapshot and external cells through both cache
/// layers (an overlay entry would otherwise shadow a layer-2 correction).
fn resync_cl(
    cache: &mut EvmCache,
    pool: Address,
    spec: &V3SyncSpec,
    snapshot: &V3PoolSnapshot,
    external: &[(Address, U256, Option<U256>, U256)],
) {
    let mut updates: Vec<(Address, U256, U256)> = snapshot
        .storage_entries(spec)
        .into_iter()
        .map(|(slot, value)| (pool, slot, value))
        .collect();
    updates.extend(
        external
            .iter()
            .map(|(address, slot, _, value)| (*address, *slot, *value)),
    );
    cache.inject_storage_batch_fresh(&updates);
}

struct Quote {
    label: String,
    config: SimConfig,
    gas_price: u128,
}

fn cl_quote_configs(chain: &ClChain, basefee: u128) -> Vec<Quote> {
    chain
        .tips
        .iter()
        .map(|tip| {
            let config = if *tip == 0 {
                SimConfig::default().with_tx_origin(EOA)
            } else {
                SimConfig::default()
                    .with_tx_origin(EOA)
                    .with_effective_gas_price(basefee, *tip)
            };
            Quote {
                label: format!("tip {tip}"),
                gas_price: config.gas_price.unwrap_or(0),
                config,
            }
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
async fn cl_quotes(
    provider: &RootProvider<AnyNetwork>,
    chain: &ClChain,
    adapter: &ConcentratedLiquidityAdapter,
    registration: &PoolRegistration,
    cache: &mut EvmCache,
    block: u64,
    basefee: u128,
    report: &mut Report,
) -> Result<()> {
    // Block-scoped hook cell: the stored first-swap fee of this block (zero
    // when the pool has not swapped in it). Measure what the replayed cache
    // holds, then hydrate the exact chain value for the strict quote.
    let key = block_fee_key(chain.pool, block);
    let chain_fee = storage_at(provider, AERO_HOOK, key, block).await?;
    report.block_fee_checks += 1;
    if !chain_fee.is_zero() {
        report.block_fee_nonzero += 1;
    }
    match cache.cached_storage_value(AERO_HOOK, key) {
        Some(cached) if cached == chain_fee => report.block_fee_cache_match += 1,
        Some(cached) => {
            report.block_fee_cache_mismatch += 1;
            if report.block_fee_cache_mismatch == 1 {
                report.notes.push(format!(
                    "first blockFee mismatch at {block}: cache {cached} vs chain {chain_fee}"
                ));
            }
        }
        None => report.block_fee_cache_absent += 1,
    }
    cache.inject_storage_batch_fresh(&[(AERO_HOOK, key, chain_fee)]);
    let snapshot = cache.snapshot();
    let quote_token = if chain.basis == chain.token0 {
        chain.token1
    } else {
        chain.token0
    };
    for quote in cl_quote_configs(chain, basefee) {
        for size in chain.sizes {
            let amount = U256::from(size);
            // 1. sell basis, exact input
            let mut strict = StrictCache::new(Arc::clone(&snapshot));
            let sell = adapter.simulate_swap(
                registration,
                &mut strict,
                chain.basis,
                quote_token,
                amount,
                &quote.config,
            );
            let truth = eth_call(
                provider,
                METAQUOTER,
                Bytes::from(
                    IMetaquoter::quoteExactInputSingleV3Call {
                        pool: chain.pool,
                        tokenIn: chain.basis,
                        amountIn: amount,
                        sqrtPriceLimitX96: U256::ZERO.to(),
                    }
                    .abi_encode(),
                ),
                quote.gas_price,
                block,
                None,
            )
            .await?;
            let truth_out = truth
                .as_deref()
                .map(|bytes| {
                    IMetaquoter::quoteExactInputSingleV3Call::abi_decode_returns_validate(bytes)
                })
                .transpose()?
                .map(|r| r.amountOut);
            record_quote(
                report,
                &strict,
                chain,
                block,
                &format!("sell {size} {}", quote.label),
                sell.map(|q| q.amount_out),
                truth_out,
            );
            // 2. buy basis, exact output
            let mut strict = StrictCache::new(Arc::clone(&snapshot));
            let buy = adapter.simulate_swap_exact_output(
                registration,
                &mut strict,
                quote_token,
                chain.basis,
                amount,
                &quote.config,
            );
            let truth = eth_call(
                provider,
                METAQUOTER,
                Bytes::from(
                    IMetaquoter::quoteExactOutputSingleV3Call {
                        pool: chain.pool,
                        tokenIn: quote_token,
                        amountOut: amount,
                        sqrtPriceLimitX96: U256::ZERO.to(),
                    }
                    .abi_encode(),
                ),
                quote.gas_price,
                block,
                None,
            )
            .await?;
            let truth_in = truth
                .as_deref()
                .map(|bytes| {
                    IMetaquoter::quoteExactOutputSingleV3Call::abi_decode_returns_validate(bytes)
                })
                .transpose()?
                .map(|r| r.amountIn);
            record_quote(
                report,
                &strict,
                chain,
                block,
                &format!("buy {size} {}", quote.label),
                buy.map(|q| q.amount_in),
                truth_in,
            );
            // 3. sell the quote token (the chain's sell output), exact input
            if let Some(truth_out) = truth_out.filter(|out| !out.is_zero()) {
                let mut strict = StrictCache::new(Arc::clone(&snapshot));
                let reverse = adapter.simulate_swap(
                    registration,
                    &mut strict,
                    quote_token,
                    chain.basis,
                    truth_out,
                    &quote.config,
                );
                let truth = eth_call(
                    provider,
                    METAQUOTER,
                    Bytes::from(
                        IMetaquoter::quoteExactInputSingleV3Call {
                            pool: chain.pool,
                            tokenIn: quote_token,
                            amountIn: truth_out,
                            sqrtPriceLimitX96: U256::ZERO.to(),
                        }
                        .abi_encode(),
                    ),
                    quote.gas_price,
                    block,
                    None,
                )
                .await?;
                let truth_reverse = truth
                    .as_deref()
                    .map(|bytes| {
                        IMetaquoter::quoteExactInputSingleV3Call::abi_decode_returns_validate(bytes)
                    })
                    .transpose()?
                    .map(|r| r.amountOut);
                record_quote(
                    report,
                    &strict,
                    chain,
                    block,
                    &format!("reverse {size} {}", quote.label),
                    reverse.map(|q| q.amount_out),
                    truth_reverse,
                );
            }
        }
    }
    // 4. Non-zero scaling factor: the hook's dynamic component (`observe` over
    // the ring across `secondsAgo`, scaled, capped) is exercised by
    // overriding the pool's config word in both the replayed snapshot and the
    // chain call (`stateDiff`). The override is temporary.
    let config_key = mapping_key(chain.pool.as_slice(), HOOK_DYNAMIC_FEE_CONFIG_SLOT);
    let original = cache.cached_storage_value(AERO_HOOK, config_key);
    let base_word = original.unwrap_or(U256::ZERO) & U256::from(0xff_ffff_u32);
    let dynamic_word = base_word
        | (U256::from(DYNAMIC_FEE_CAP) << 24)
        | (U256::from(DYNAMIC_SCALING_FACTOR) << 48);
    cache.inject_storage_batch_fresh(&[(AERO_HOOK, config_key, dynamic_word)]);
    let dynamic_snapshot = cache.snapshot();
    let overrides = StateOverridesBuilder::default()
        .append(
            AERO_HOOK,
            AccountOverride::default().with_state_diff([(
                B256::from(config_key.to_be_bytes::<32>()),
                B256::from(dynamic_word.to_be_bytes::<32>()),
            )]),
        )
        .build();
    let amount = U256::from(chain.sizes[0]);
    let config = SimConfig::default().with_tx_origin(EOA);
    let mut strict = StrictCache::new(Arc::clone(&dynamic_snapshot));
    let sell = adapter.simulate_swap(
        registration,
        &mut strict,
        chain.basis,
        quote_token,
        amount,
        &config,
    );
    let truth = eth_call(
        provider,
        METAQUOTER,
        Bytes::from(
            IMetaquoter::quoteExactInputSingleV3Call {
                pool: chain.pool,
                tokenIn: chain.basis,
                amountIn: amount,
                sqrtPriceLimitX96: U256::ZERO.to(),
            }
            .abi_encode(),
        ),
        0,
        block,
        Some(overrides),
    )
    .await?;
    let truth_out = truth
        .as_deref()
        .map(|bytes| IMetaquoter::quoteExactInputSingleV3Call::abi_decode_returns_validate(bytes))
        .transpose()?
        .map(|r| r.amountOut);
    record_quote(
        report,
        &strict,
        chain,
        block,
        &format!("dynamic K={DYNAMIC_SCALING_FACTOR} sell {}", chain.sizes[0]),
        sell.map(|q| q.amount_out),
        truth_out,
    );
    match original {
        Some(word) => cache.inject_storage_batch_fresh(&[(AERO_HOOK, config_key, word)]),
        None => {
            cache.purge_slots(AERO_HOOK, &[config_key]);
        }
    }
    Ok(())
}

fn record_quote(
    report: &mut Report,
    strict: &StrictCache,
    chain: &ClChain,
    block: u64,
    label: &str,
    offline: Result<U256, evm_amm_state::adapters::SimError>,
    truth: Option<U256>,
) {
    report.quotes += 1;
    if label.starts_with("dynamic") {
        report.dynamic_quotes += 1;
    }
    let missing = strict.overlay.missing_state();
    let mut missed = false;
    for (address, slot) in &missing.storage {
        missed = true;
        let class = classify_miss(*address, *slot, chain.pool, chain.gauge, block);
        report.miss(class, format!("block {block} {label}: {address}:{slot:#x}"));
    }
    for address in &missing.accounts {
        missed = true;
        report.miss(
            format!("account {address}"),
            format!("block {block} {label}: account header"),
        );
    }
    for hash in &missing.code_hashes {
        missed = true;
        report.miss(
            "runtime code".to_owned(),
            format!("block {block} {label}: {hash:#x}"),
        );
    }
    match (offline, truth) {
        (Ok(amount), Some(truth)) if amount == truth => {
            report.parity_ok += 1;
            if label.starts_with("dynamic") {
                report.dynamic_parity_ok += 1;
            }
        }
        (Err(_), None) if !missed => report.both_reverted += 1,
        (Ok(amount), truth) => {
            report.parity_mismatch += 1;
            if report.parity_first_mismatch.is_none() {
                report.parity_first_mismatch = Some(format!(
                    "block {block} {label}: offline {amount} vs eth_call {truth:?}"
                ));
            }
        }
        (Err(error), truth) => {
            report.strict_failures += 1;
            if report.parity_first_mismatch.is_none() && truth.is_some() {
                report.parity_first_mismatch = Some(format!(
                    "block {block} {label}: offline error {error} vs eth_call {truth:?}"
                ));
            }
            if !missed {
                report.miss(
                    "quote error without a recorded miss".to_owned(),
                    format!("block {block} {label}: {error}"),
                );
            }
        }
    }
}

/// Build the reviewed-runtime fee evidence for one Aero `Swap` from the cache's
/// current (parent) snapshot: unstaked fee evaluated offline, the
/// transaction's gauge settlement and the event-derived effective fee.
fn aero_swap_evidence(
    family: SlipstreamRuntimeFamily,
    chain: &ClChain,
    cache: &mut EvmCache,
    registration: &PoolRegistration,
    log: &RpcLog,
    context: &AdapterEventContext,
    settlement: U256,
) -> Result<SlipstreamSwapFeeEvidence> {
    let number = context.block_number.expect("block");
    let identity = SlipstreamSnapshotIdentity::new(
        context.chain_id.expect("chain"),
        number,
        context.block_hash.expect("hash"),
        context.parent_hash.expect("parent"),
        context.block_timestamp.expect("timestamp"),
        context.transaction_hash.expect("tx"),
        context.transaction_index.expect("tx index"),
        context.log_index.expect("log index"),
    )
    .map_err(|error| anyhow!("identity: {error}"))?;
    let snapshot = cache.snapshot();
    let evaluation = ConcentratedLiquidityAdapter::evaluate_slipstream_unstaked_fee(
        family, snapshot, identity, context,
    )
    .map_err(|error| anyhow!("unstaked fee: {error}"))?;
    let evidence = SlipstreamSwapFeeEvidence::new(
        family,
        context.chain_id.expect("chain"),
        chain.pool,
        chain.factory,
        AERO_POOL_PROXY_RUNTIME_CODE_HASH,
        AERO_CL_IMPLEMENTATION,
        AERO_CL_IMPLEMENTATION_RUNTIME_CODE_HASH,
        0,
        evaluation.effective_fee(),
        evaluation.proof(),
        number,
        context.block_hash.expect("hash"),
        context.parent_hash.expect("parent"),
        context.block_timestamp.expect("timestamp"),
        context.transaction_hash.expect("tx"),
        context.transaction_index.expect("tx index"),
        context.log_index.expect("log index"),
    )
    .map_err(|error| anyhow!("evidence: {error}"))?
    .with_gauge_settlement_delta(settlement)
    .map_err(|error| anyhow!("settlement delta: {error}"))?;
    let mut provisional = *context;
    provisional.slipstream_fee_evidence = Some(evidence);
    let fee = ConcentratedLiquidityAdapter::infer_slipstream_swap_fee(
        registration,
        &log.inner,
        cache,
        &provisional,
    )
    .map_err(|error| anyhow!("fee inference: {error}"))?;
    evidence
        .with_effective_swap_fee(fee)
        .map_err(|error| anyhow!("effective fee: {error}"))
}

async fn run_cl(url: &str, chain: ClChain) -> Result<()> {
    let provider = Arc::new(connect(url)?);
    let (start, end) = window(&provider, chain.env_prefix, chain.default_blocks).await?;
    let stride = stride();
    let mut report = Report {
        chain: chain.name.to_owned(),
        pool: format!("{}", chain.pool),
        start,
        end,
        stride,
        ..Default::default()
    };
    eprintln!(
        "[{}] window {start}..={end} ({} blocks), stride {stride}",
        chain.name,
        end - start
    );

    let mut cache = EvmCache::at_block(
        Arc::clone(&provider),
        BlockId::Number(BlockNumberOrTag::Number(start)),
    )
    .await;
    let chain_id = cache.chain_id();
    let mut registry = AdapterRegistry::new();
    registry.register_adapter(Arc::new(ConcentratedLiquidityAdapter::default()))?;
    let mut pools = vec![cl_registration(&chain)];
    let outcomes = registry
        .cold_start_many(
            &mut pools,
            &mut cache,
            provider.as_ref(),
            ColdStartPolicy::Eager,
        )
        .await?;
    ensure!(
        matches!(outcomes[0], ColdStartOutcome::Ready(_)),
        "cold start at {start} must be ready: {:?}",
        outcomes[0]
    );
    let registration = pools.remove(0);
    // The quote's tx.origin and the (zero) coinbase of the hash-pinned block
    // context must be resident for a strict (no-fetch) quote, as must the
    // origin's discount-registry cell (the hook applies the better of the
    // caller's and the origin's discount).
    for account in [EOA, Address::ZERO] {
        cache
            .ensure_account(account)
            .await
            .map_err(|error| anyhow!("seed account {account}: {error}"))?;
    }
    if let Some((registry, slot)) =
        ConcentratedLiquidityAdapter::aero_cl_discount_storage_target(&registration, EOA)
    {
        cache
            .read_storage_slot(registry, slot)
            .map_err(|error| anyhow!("hydrate origin discount cell: {error}"))?;
    }
    let family = ConcentratedLiquidityAdapter::aero_cl_runtime_family(&registration)
        .ok_or_else(|| anyhow!("reviewed family"))?;
    ConcentratedLiquidityAdapter::validate_aero_cl_runtime(family, &cache.snapshot())
        .map_err(|error| anyhow!("runtime identity: {error}"))?;
    let adapter = ConcentratedLiquidityAdapter::default();
    let external_targets = adapter.verified_storage_targets(&registration);
    let layout = V3StorageLayout::aero_cl(chain.spacing);
    let spec = V3SyncSpec::aero_cl(layout);
    let sources = registry
        .adapter(registration.protocol())
        .ok_or_else(|| anyhow!("adapter registered"))?
        .event_sources(&registration);
    let registration = registration.with_event_sources(sources);
    registry.register_pool(registration.clone())?;
    let driver = AdapterDriver::new(registry);

    // Every log of the window from the pool, its gauge, the shared NFT
    // manager (custody transfers from the gauge), the hook, the factory, the
    // discount registry and the MEV module.
    let mut addresses = vec![
        chain.pool,
        chain.gauge,
        AERO_HOOK,
        chain.factory,
        AERO_DISCOUNT_REGISTRY,
    ];
    addresses.extend(chain.mev_module);
    let mut fetched = fetch_logs(
        &provider,
        Filter::new().address(addresses.clone()),
        start + 1,
        end,
    )
    .await?;
    // The shared NFT manager is high-volume; subscribe only to custody
    // transfers out of this gauge (the withdrawal apply point).
    fetched.extend(
        fetch_logs(
            &provider,
            Filter::new()
                .address(AERO_NFT)
                .event_signature(Transfer::SIGNATURE_HASH)
                .topic1(chain.gauge.into_word()),
            start + 1,
            end,
        )
        .await?,
    );
    fetched.sort_by_key(|log| (log.block_number, log.log_index));
    let mut routed: BTreeMap<u64, Vec<RpcLog>> = BTreeMap::new();
    let mut stake_txs = BTreeSet::new();
    for log in fetched {
        let topic = log.topics().first().copied();
        if log.address() == chain.gauge
            && matches!(topic, Some(t) if t == Deposit::SIGNATURE_HASH || t == Withdraw::SIGNATURE_HASH)
        {
            stake_txs.insert(log.transaction_hash.expect("tx hash"));
        }
        routed
            .entry(log.block_number.expect("block number"))
            .or_default()
            .push(log);
    }
    // Staking evidence needs the complete ordered receipt of every stake
    // transaction (manager Collect, pool Collect, GaugeSettled).
    let mut evidence: BTreeMap<u64, BTreeMap<u64, RpcLog>> = BTreeMap::new();
    for tx in &stake_txs {
        let receipt = provider
            .get_transaction_receipt(*tx)
            .await?
            .ok_or_else(|| anyhow!("receipt {tx:#x} missing"))?;
        let number = receipt.block_number.expect("block number");
        for log in receipt.inner.inner.logs() {
            if [chain.pool, chain.gauge, AERO_NFT].contains(&log.address()) {
                evidence
                    .entry(number)
                    .or_default()
                    .insert(log.log_index.expect("log index"), log.clone());
            }
        }
    }
    for (number, logs) in &routed {
        for log in logs {
            if [chain.pool, chain.gauge, AERO_NFT].contains(&log.address()) {
                evidence
                    .entry(*number)
                    .or_default()
                    .entry(log.log_index.expect("log index"))
                    .or_insert_with(|| log.clone());
            }
        }
    }
    report.event_blocks = routed.len() as u64;
    eprintln!(
        "[{}] {} routed logs in {} blocks, {} stake transactions",
        chain.name,
        routed.values().map(Vec::len).sum::<usize>(),
        routed.len(),
        stake_txs.len()
    );

    let mut checks: BTreeSet<u64> = BTreeSet::new();
    checks.insert(start);
    checks.insert(end);
    for block in routed.keys() {
        checks.insert(*block);
        if *block < end {
            checks.insert(block + 1);
        }
    }
    let mut block = start.div_ceil(stride) * stride;
    while block <= end {
        if block > start {
            checks.insert(block);
        }
        block += stride;
    }
    let mut schedule: BTreeSet<u64> = checks.iter().copied().collect();
    schedule.extend(routed.keys().copied());

    let quotes = quotes_enabled();
    let mut last_check = start;
    let mut events_since_check: Vec<String> = Vec::new();
    let mut processed = 0_usize;
    for number in schedule {
        let head = advance_to(&provider, &mut cache, number, number > start).await?;
        let mut resynced = false;
        if let Some(logs) = routed.get(&number) {
            let block_ref = StakingBlockRef {
                number,
                hash: head.hash,
                parent_hash: head.parent,
                timestamp: head.timestamp,
            };
            let snapshot = cache.snapshot();
            let batch = SlipstreamStakingBatch::from_block_logs(
                chain.pool,
                block_ref,
                evidence
                    .get(&number)
                    .into_iter()
                    .flat_map(|logs| logs.values()),
                &snapshot,
            );
            for log in logs {
                let label = event_label(log, chain.pool, chain.gauge);
                *report.events_by_topic.entry(label.clone()).or_default() += 1;
                events_since_check.push(label.clone());
                let tx_hash = log.transaction_hash.expect("tx hash");
                let log_index = log.log_index.expect("log index");
                let mut context = AdapterEventContext::for_block(number, head.hash, head.timestamp)
                    .with_chain_id(chain_id)
                    .with_parent_hash(head.parent)
                    .with_transaction_hash(tx_hash)
                    .with_event_order(log.transaction_index.expect("tx index"), log_index);
                context.slipstream_staking_evidence = batch.evidence(chain.pool, &context);
                if log.address() == chain.pool
                    && log.topics().first() == Some(&Swap::SIGNATURE_HASH)
                {
                    // Accounting evidence: the exact unstaked fee of the
                    // parent snapshot and the event-derived effective fee. A
                    // `GaugeSettled` the swap pulled was applied at its own
                    // (earlier) log, so the swap settles silently.
                    match aero_swap_evidence(
                        family,
                        &chain,
                        &mut cache,
                        &registration,
                        log,
                        &context,
                        U256::ZERO,
                    ) {
                        Ok(evidence) => context.slipstream_fee_evidence = Some(evidence),
                        Err(error) => {
                            report.evidence_failures += 1;
                            if report.evidence_failure_first.is_none() {
                                report.evidence_failure_first =
                                    Some(format!("block {number} log {log_index}: {error}"));
                            }
                        }
                    }
                }
                let outcome = driver.apply_log_with_context(&mut cache, &log.inner, &context);
                let (exact, detail) = match outcome {
                    Ok(None) => continue,
                    Ok(Some(result)) => {
                        let exact = result.event.quality == UpdateQuality::Exact
                            && result.repair == RepairAction::None
                            && !result.applied.has_skipped();
                        (
                            exact,
                            format!(
                                "block {number} log {log_index} {label}: quality {:?}, repair {:?}, skipped {}",
                                result.event.quality,
                                result.repair,
                                result.applied.has_skipped()
                            ),
                        )
                    }
                    Err(error) => (
                        false,
                        format!("block {number} log {log_index} {label}: decode error {error}"),
                    ),
                };
                if !exact {
                    report.adapter_repairs += 1;
                    if report.adapter_repair_first.is_none() {
                        report.adapter_repair_first = Some(detail.clone());
                    }
                    eprintln!("  repair: {detail}");
                    // Re-synchronise from the chain so later checks measure
                    // fresh behaviour. The chain state at `number` is
                    // post-block, so the remaining logs of this block are
                    // already reflected and are not re-applied.
                    let (_, snapshot, _) =
                        compare_cl_state(&provider, &cache, chain.pool, &spec, number).await?;
                    let external =
                        compare_external(&provider, &cache, &external_targets, number).await?;
                    resync_cl(&mut cache, chain.pool, &spec, &snapshot, &external);
                    resynced = true;
                    break;
                }
            }
        }
        if checks.contains(&number) {
            let (divergent, snapshot, compared) =
                compare_cl_state(&provider, &cache, chain.pool, &spec, number).await?;
            let external = compare_external(&provider, &cache, &external_targets, number).await?;
            report.blocks_checked += 1;
            report.cells_compared += compared as u64;
            let persistence = number - last_check;
            if !divergent.is_empty() || !external.is_empty() {
                let mut seen = BTreeSet::new();
                for (class, slot, cached, chain_value) in &divergent {
                    if seen.insert(*class) {
                        report
                            .classes
                            .entry((*class).to_owned())
                            .or_default()
                            .divergent_checks += 1;
                    }
                    report.class(
                        class,
                        number,
                        format!("slot {slot:#x}: cache {cached:?} vs chain {chain_value:#x}"),
                        persistence,
                    );
                }
                let mut seen_external = BTreeSet::new();
                for (address, slot, cached, chain_value) in &external {
                    let class = external_class(*address, chain.gauge);
                    if seen_external.insert(class.clone()) {
                        report
                            .external
                            .entry(class.clone())
                            .or_default()
                            .divergent_checks += 1;
                    }
                    report.external_cell(
                        &class,
                        number,
                        format!("{address}:{slot:#x}: cache {cached:?} vs chain {chain_value:#x}"),
                        persistence,
                    );
                }
                if !divergent.is_empty() && !resynced {
                    let cause = classify_cause(&events_since_check, &divergent);
                    *report.causes.entry(cause).or_default() += 1;
                    report.resyncs += 1;
                }
                eprintln!(
                    "  block {number}: {} pool cells and {} external cells diverge ({} events since {last_check})",
                    divergent.len(),
                    external.len(),
                    events_since_check.len()
                );
            }
            if quotes {
                cl_quotes(
                    &provider,
                    &chain,
                    &adapter,
                    &registration,
                    &mut cache,
                    number,
                    head.basefee,
                    &mut report,
                )
                .await?;
            }
            if !divergent.is_empty() || !external.is_empty() {
                resync_cl(&mut cache, chain.pool, &spec, &snapshot, &external);
            }
            last_check = number;
            events_since_check.clear();
        }
        processed += 1;
        if processed.is_multiple_of(50) {
            eprintln!(
                "[{}] {number}: {} checked, {} repairs, {} resyncs, {} quotes ({} ok / {} mismatch), misses {:?}",
                chain.name,
                report.blocks_checked,
                report.adapter_repairs,
                report.resyncs,
                report.quotes,
                report.parity_ok,
                report.parity_mismatch,
                report.strict_misses
            );
        }
    }
    report.write()
}

fn classify_cause(
    events: &[String],
    divergent: &[(&'static str, U256, Option<U256>, U256)],
) -> String {
    let has = |needle: &str| events.iter().any(|event| event.contains(needle));
    let classes: BTreeSet<&str> = divergent.iter().map(|(class, ..)| *class).collect();
    if has("gauge Deposit") || has("gauge Withdraw") {
        "gauge stake/unstake".to_owned()
    } else if has("Mint") || has("Burn") || has("Collect") {
        "CLM mint/burn/collect".to_owned()
    } else if has("Swap")
        && classes.iter().any(|class| {
            class.starts_with("tick word")
                || class.starts_with("feeGrowth")
                || class.starts_with("gaugeFees")
                || class.starts_with("reward")
        })
    {
        "swap accounting (fee growth / tick outside words)".to_owned()
    } else if has("Swap") && classes.iter().all(|class| *class == "observation ring") {
        "observation write".to_owned()
    } else if has("Swap") {
        "swap (quote surface)".to_owned()
    } else if events.is_empty() {
        "no event in window (non-event write)".to_owned()
    } else {
        format!(
            "other ({})",
            events
                .iter()
                .take(3)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

// ---------------------------------------------------------------------------
// Volatile campaign (Optimism)
// ---------------------------------------------------------------------------

async fn run_volatile(url: &str) -> Result<()> {
    let provider = Arc::new(connect(url)?);
    let (start, end) = window(&provider, "OP", 100_000).await?;
    let stride = stride();
    let mut report = Report {
        chain: "Optimism".to_owned(),
        pool: format!("{OP_POOL}"),
        start,
        end,
        stride,
        ..Default::default()
    };
    eprintln!(
        "[Optimism] window {start}..={end} ({} blocks), stride {stride}",
        end - start
    );
    let mut cache = EvmCache::at_block(
        Arc::clone(&provider),
        BlockId::Number(BlockNumberOrTag::Number(start)),
    )
    .await;
    let chain_id = cache.chain_id();
    let mut registry = AdapterRegistry::new();
    registry.register_adapter(Arc::new(SolidlyV2Adapter::default()))?;
    let mut pools = vec![
        PoolRegistration::new(PoolKey::SolidlyV2(OP_POOL))
            .with_state_address(OP_POOL)
            .with_metadata(ProtocolMetadata::SolidlyV2(
                SolidlyV2Metadata::default()
                    .with_token0(L2_WETH)
                    .with_token1(MOO_BIFI)
                    .with_aero_volatile(),
            )),
    ];
    let outcomes = registry
        .cold_start_many(
            &mut pools,
            &mut cache,
            provider.as_ref(),
            ColdStartPolicy::Eager,
        )
        .await?;
    ensure!(
        matches!(outcomes[0], ColdStartOutcome::Ready(_)),
        "cold start at {start} must be ready: {:?}",
        outcomes[0]
    );
    let registration = pools.remove(0);
    for account in [EOA, Address::ZERO] {
        cache
            .ensure_account(account)
            .await
            .map_err(|error| anyhow!("seed account {account}: {error}"))?;
    }
    let adapter = SolidlyV2Adapter::default();
    let external_targets = adapter.verified_storage_targets(&registration);
    let layout = SolidlyStorageLayout::aero_volatile();
    let sources = registry
        .adapter(registration.protocol())
        .ok_or_else(|| anyhow!("adapter registered"))?
        .event_sources(&registration);
    let registration = registration.with_event_sources(sources);
    registry.register_pool(registration.clone())?;
    let driver = AdapterDriver::new(registry);

    let fetched = fetch_logs(
        &provider,
        Filter::new().address(vec![OP_POOL, OP_FACTORY, OP_FEE_MODULE, OP_MEV_MODULE]),
        start + 1,
        end,
    )
    .await?;
    let mut routed: BTreeMap<u64, Vec<RpcLog>> = BTreeMap::new();
    for log in fetched {
        routed
            .entry(log.block_number.expect("block number"))
            .or_default()
            .push(log);
    }
    report.event_blocks = routed.len() as u64;
    let mut checks: BTreeSet<u64> = BTreeSet::new();
    checks.insert(start);
    checks.insert(end);
    for block in routed.keys() {
        checks.insert(*block);
        if *block < end {
            checks.insert(block + 1);
        }
    }
    let mut block = start.div_ceil(stride) * stride;
    while block <= end {
        if block > start {
            checks.insert(block);
        }
        block += stride;
    }
    let mut schedule: BTreeSet<u64> = checks.iter().copied().collect();
    schedule.extend(routed.keys().copied());
    let pool_cells = [
        ("reserve0", layout.reserve0_slot),
        ("reserve1", layout.reserve1_slot),
    ];
    let quotes = quotes_enabled();
    let mut last_check = start;
    let mut events_since_check: Vec<String> = Vec::new();
    let tips: [u128; 3] = [10_000_001, 1_050_000_000, 1_100_000_000];
    let sizes: [u128; 2] = [1_000_000_000_000_000_000, 5_000_000_000_000_000_000];
    let mut processed = 0_usize;
    for number in schedule {
        let head = advance_to(&provider, &mut cache, number, number > start).await?;
        if let Some(logs) = routed.get(&number) {
            for log in logs {
                let label = event_label(log, OP_POOL, Address::ZERO);
                *report.events_by_topic.entry(label.clone()).or_default() += 1;
                events_since_check.push(label.clone());
                let context = AdapterEventContext::for_block(number, head.hash, head.timestamp)
                    .with_chain_id(chain_id)
                    .with_parent_hash(head.parent)
                    .with_transaction_hash(log.transaction_hash.expect("tx hash"))
                    .with_event_order(
                        log.transaction_index.expect("tx index"),
                        log.log_index.expect("log index"),
                    );
                let outcome = driver.apply_log_with_context(&mut cache, &log.inner, &context);
                let (exact, detail) = match outcome {
                    Ok(None) => continue,
                    Ok(Some(result)) => (
                        matches!(
                            result.event.quality,
                            UpdateQuality::Exact | UpdateQuality::ExactIfApplied
                        ) && result.repair == RepairAction::None
                            && !result.applied.has_skipped(),
                        format!(
                            "block {number} log {:?} {label}: quality {:?}, repair {:?}",
                            log.log_index, result.event.quality, result.repair
                        ),
                    ),
                    Err(error) => (
                        false,
                        format!(
                            "block {number} log {:?} {label}: decode error {error}",
                            log.log_index
                        ),
                    ),
                };
                if !exact {
                    report.adapter_repairs += 1;
                    if report.adapter_repair_first.is_none() {
                        report.adapter_repair_first = Some(detail.clone());
                    }
                    eprintln!("  repair: {detail}");
                }
            }
        }
        if checks.contains(&number) {
            report.blocks_checked += 1;
            let persistence = number - last_check;
            let mut divergent = Vec::new();
            for (name, slot) in pool_cells {
                let chain_value = storage_at(&provider, OP_POOL, slot, number).await?;
                report.cells_compared += 1;
                let cached = cache.cached_storage_value(OP_POOL, slot);
                if cached != Some(chain_value) {
                    report
                        .classes
                        .entry(name.to_owned())
                        .or_default()
                        .divergent_checks += 1;
                    report.class(
                        name,
                        number,
                        format!("cache {cached:?} vs chain {chain_value:#x}"),
                        persistence,
                    );
                    divergent.push((OP_POOL, slot, cached, chain_value));
                }
            }
            let external = compare_external(&provider, &cache, &external_targets, number).await?;
            let mut seen = BTreeSet::new();
            for (address, slot, cached, chain_value) in &external {
                let class = external_class(*address, Address::ZERO);
                if seen.insert(class.clone()) {
                    report
                        .external
                        .entry(class.clone())
                        .or_default()
                        .divergent_checks += 1;
                }
                report.external_cell(
                    &class,
                    number,
                    format!("{address}:{slot:#x}: cache {cached:?} vs chain {chain_value:#x}"),
                    persistence,
                );
            }
            if !divergent.is_empty() {
                let cause = if events_since_check.is_empty() {
                    "no event in window (non-event write)".to_owned()
                } else {
                    format!(
                        "after {}",
                        events_since_check
                            .iter()
                            .take(3)
                            .cloned()
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                };
                *report.causes.entry(cause).or_default() += 1;
                report.resyncs += 1;
            }
            if quotes {
                let snapshot = cache.snapshot();
                for tip in tips {
                    let config = SimConfig::default()
                        .with_tx_origin(EOA)
                        .with_effective_gas_price(head.basefee, tip);
                    let gas_price = head.basefee + tip;
                    for size in sizes {
                        let amount = U256::from(size);
                        let mut strict = StrictCache::new(Arc::clone(&snapshot));
                        let sell = adapter.simulate_swap(
                            &registration,
                            &mut strict,
                            MOO_BIFI,
                            L2_WETH,
                            amount,
                            &config,
                        );
                        let truth = eth_call(
                            &provider,
                            OP_POOL,
                            Bytes::from(
                                IAeroPool::getAmountOutWithTotalFeeCall {
                                    amountIn: amount,
                                    tokenIn: MOO_BIFI,
                                }
                                .abi_encode(),
                            ),
                            gas_price,
                            number,
                            None,
                        )
                        .await?;
                        let truth_out = truth
                            .as_deref()
                            .map(|bytes| IAeroPool::getAmountOutWithTotalFeeCall::abi_decode_returns_validate(bytes))
                            .transpose()?;
                        record_volatile_quote(
                            &mut report,
                            &strict,
                            number,
                            &format!("sell {size} tip {tip}"),
                            sell.map(|q| q.amount_out),
                            truth_out,
                        );
                        let mut strict = StrictCache::new(Arc::clone(&snapshot));
                        let buy = adapter.simulate_swap_exact_output(
                            &registration,
                            &mut strict,
                            L2_WETH,
                            MOO_BIFI,
                            amount,
                            &config,
                        );
                        let truth = eth_call(
                            &provider,
                            OP_POOL,
                            Bytes::from(
                                IAeroPool::getAmountInWithTotalFeeCall {
                                    amountOut: amount,
                                    tokenOut: MOO_BIFI,
                                }
                                .abi_encode(),
                            ),
                            gas_price,
                            number,
                            None,
                        )
                        .await?;
                        let truth_in = truth
                            .as_deref()
                            .map(|bytes| {
                                IAeroPool::getAmountInWithTotalFeeCall::abi_decode_returns_validate(
                                    bytes,
                                )
                            })
                            .transpose()?;
                        record_volatile_quote(
                            &mut report,
                            &strict,
                            number,
                            &format!("buy {size} tip {tip}"),
                            buy.map(|q| q.amount_in),
                            truth_in,
                        );
                    }
                }
            }
            if !divergent.is_empty() || !external.is_empty() {
                let updates: Vec<(Address, U256, U256)> = divergent
                    .iter()
                    .chain(external.iter())
                    .map(|(address, slot, _, value)| (*address, *slot, *value))
                    .collect();
                cache.inject_storage_batch_fresh(&updates);
            }
            last_check = number;
            events_since_check.clear();
        }
        processed += 1;
        if processed.is_multiple_of(100) {
            eprintln!(
                "[Optimism] {number}: {} checked, {} repairs, {} resyncs, {} quotes ({} ok / {} mismatch)",
                report.blocks_checked,
                report.adapter_repairs,
                report.resyncs,
                report.quotes,
                report.parity_ok,
                report.parity_mismatch
            );
        }
    }
    report.write()
}

fn record_volatile_quote(
    report: &mut Report,
    strict: &StrictCache,
    block: u64,
    label: &str,
    offline: Result<U256, evm_amm_state::adapters::SimError>,
    truth: Option<U256>,
) {
    report.quotes += 1;
    let missing = strict.overlay.missing_state();
    for (address, slot) in &missing.storage {
        report.miss(
            format!("{address}"),
            format!("block {block} {label}: slot {slot:#x}"),
        );
    }
    for address in &missing.accounts {
        report.miss(
            format!("account {address}"),
            format!("block {block} {label}"),
        );
    }
    match (offline, truth) {
        (Ok(amount), Some(truth)) if amount == truth => report.parity_ok += 1,
        (Err(_), None) => report.both_reverted += 1,
        (Ok(amount), truth) => {
            report.parity_mismatch += 1;
            if report.parity_first_mismatch.is_none() {
                report.parity_first_mismatch = Some(format!(
                    "block {block} {label}: offline {amount} vs eth_call {truth:?}"
                ));
            }
        }
        (Err(error), _) => {
            report.strict_failures += 1;
            report.miss(
                "quote error".to_owned(),
                format!("block {block} {label}: {error}"),
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
#[ignore = "long-window campaign; requires an Ethereum archive RPC (E2E_RPC_URL)"]
async fn ethereum_aero_cl_campaign() -> Result<()> {
    let url = eth_url()?;
    run_cl(
        &url,
        ClChain {
            name: "Ethereum",
            pool: ETH_POOL,
            gauge: ETH_GAUGE,
            factory: ETH_FACTORY,
            mev_module: None,
            spacing: 50,
            token0: BIFI,
            token1: ETH_WETH,
            basis: BIFI,
            // 1 and 5 mooBIFI-equivalent in BIFI (pinned price per share).
            sizes: [1_380_094_473_235_947_155, 6_900_472_366_179_735_775],
            tips: [0, GWEI],
            default_blocks: 20_000,
            env_prefix: "ETH",
        },
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "long-window campaign; requires a Base archive RPC (E2E_BASE_RPC_URL / E2E_RPC_URL on Alchemy)"]
async fn base_aero_cl_campaign() -> Result<()> {
    let url = derived_url("E2E_BASE_RPC_URL", "base-mainnet")?;
    run_cl(
        &url,
        ClChain {
            name: "Base",
            pool: BASE_POOL,
            gauge: BASE_GAUGE,
            factory: BASE_FACTORY,
            mev_module: Some(BASE_MEV_MODULE),
            spacing: 100,
            token0: L2_WETH,
            token1: MOO_BIFI,
            basis: MOO_BIFI,
            sizes: [1_000_000_000_000_000_000, 5_000_000_000_000_000_000],
            tips: [0, GWEI],
            default_blocks: 100_000,
            env_prefix: "BASE",
        },
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "long-window campaign; requires an Optimism archive RPC (E2E_OPTIMISM_RPC_URL / E2E_RPC_URL on Alchemy)"]
async fn optimism_aero_volatile_campaign() -> Result<()> {
    let url = derived_url("E2E_OPTIMISM_RPC_URL", "opt-mainnet")?;
    run_volatile(&url).await
}
