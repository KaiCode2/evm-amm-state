//! Transaction-local Slipstream / Aero CL staking evidence and quote-state
//! transitions.
//!
//! The pool does not emit `stake`. The reviewed gauge and NFT manager jointly
//! attest its range and delta. Evidence is rebuilt for each delivered batch;
//! no token ownership or partially decoded transaction survives a rollback.
//!
//! Two gauge generations are reviewed:
//!
//! - the legacy Velodrome Slipstream gauge on Optimism (`Deposit(user,
//!   tokenId, liquidityToStake)` with every argument indexed, staked word at
//!   slot 15), and
//! - the Aero (Dromos) CL gauge behind the Ethereum BIFI/WETH and Base
//!   WETH/mooBIFI pools (`Deposit(sender, owner, tokenId, liquidityToStake)`,
//!   staked word at slot 12 with a `uint48 lastUpdated`). An Aero `stake`
//!   whose range covers the current tick first settles the pool to the block:
//!   the gauge pulls the LeafVoter's accrued emissions (`GaugeSettled`) and the
//!   pool credits them to `rewardGrowthGlobalX128` (or banks them into
//!   `rollover` with nothing staked), advances
//!   `secondsPerStakedLiquidityCumulativeX128` and `lastUpdated`. Those
//!   writes are derived from the same transaction's `GaugeSettled` log (zero
//!   when the gauge emitted none).

use super::storage::{
    AERO_CL_REWARD_GROWTH_SLOT, AERO_CL_ROLLOVER_SLOT, AERO_CL_SECONDS_PER_STAKED_LIQUIDITY_SLOT,
    AERO_CL_STAKED_LIQUIDITY_SLOT, layout_for, slipstream_tick_info_storage_keys_with_base,
};
use super::{
    AdapterEvent, AdapterEventContext, AdapterEventError, AdapterEventKind, AdapterEventResult,
    AdapterRegistry, AmmPoolReactiveHandler, EventSource, PoolKey, PoolRegistration, ProtocolId,
    PurgeScope, RepairAction, StateUpdate, StateView, UpdateQuality, V3TransitionError,
};
use alloy_primitives::{Address, B256, Log, U256, address, b256};
use alloy_sol_types::SolEvent;
use evm_fork_cache::reactive::{
    ChainStatus, DeliveryAudience, DeliveryScope, ReactiveInput, ReactiveInputBatch,
};
use std::collections::BTreeMap;

#[cfg(feature = "live-runtime")]
pub(crate) mod reads;

mod abi {
    alloy_sol_types::sol! {
        event Deposit(address indexed user, uint256 indexed tokenId, uint128 indexed liquidityToStake);
        event Withdraw(address indexed user, uint256 indexed tokenId, uint128 indexed liquidityToStake);
        event Transfer(address indexed from, address indexed to, uint256 indexed tokenId);
        event Collect(uint256 indexed tokenId, address recipient, uint256 amount0, uint256 amount1);
    }
}

/// Aero (Dromos) CL gauge events (`CLGauge.sol`), confirmed against the Base
/// rebalance receipt `0xcbc5d0a5…` at 53203953: three indexed arguments and
/// the staked liquidity in the data word.
mod aero_abi {
    alloy_sol_types::sol! {
        event Deposit(address indexed sender, address indexed owner, uint256 indexed tokenId, uint128 liquidityToStake);
        event Withdraw(address indexed sender, address indexed account, uint256 indexed tokenId, uint128 liquidityToStake);
        event GaugeSettled(uint256 cumulativeRewardShare, uint256 delta);
        // Emission claims (`claimEmissions`, and the claim step of every
        // withdrawal) run `pool.settleToBlock()` first; only their `topic0`
        // is consumed, so the argument indexing is immaterial.
        event EmissionsClaimed(address indexed account, uint256 indexed tokenId, address indexed recipient, uint256 reward);
        event EmissionsDeferred(address account, uint256 tokenId, uint256 reward);
        event ReferralEmissionsDeferred(address account, uint256 tokenId, address referral, uint256 amount);
        event DeferredEmissionsClaimed(address account, address recipient, uint256 amount);
    }
}

fn aero_claim_topics() -> [B256; 4] {
    [
        aero_abi::EmissionsClaimed::SIGNATURE_HASH,
        aero_abi::EmissionsDeferred::SIGNATURE_HASH,
        aero_abi::ReferralEmissionsDeferred::SIGNATURE_HASH,
        aero_abi::DeferredEmissionsClaimed::SIGNATURE_HASH,
    ]
}

/// Which gauge generation a reviewed staking runtime follows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StakingFamily {
    /// Velodrome Slipstream gauge: all-indexed `Deposit`/`Withdraw`, staked
    /// word at slot 15 (`uint32 lastUpdated`), no settlement on `stake`.
    Legacy,
    /// Aero CL gauge: `Deposit(sender, owner, tokenId, liquidity)`, staked
    /// word at slot 12 (`uint48 lastUpdated`), `stake` settles the pool.
    AeroCl,
}

/// One reviewed pool / gauge / NFT-manager staking runtime.
#[derive(Clone, Copy, Debug)]
pub(crate) struct StakingRuntime {
    family: StakingFamily,
    chain: u64,
    pool: Address,
    gauge: Address,
    nft: Address,
    tick_spacing: i32,
    staked_slot: U256,
    /// Bit offset of the packed `tickSpacing` in the staked word.
    spacing_shift: usize,
    /// Pinned runtime identities (`address → runtime keccak`) that must be
    /// resident before any evidence is built for this runtime.
    identities: &'static [(Address, B256)],
    /// Code the cold start warms beyond the pool itself.
    code_targets: &'static [Address],
}

const LEGACY_POOL: Address = address!("173cdc71e29d5cffa6d090ad99f555a24b8831f9");
const LEGACY_GAUGE: Address = address!("41160e66fcaa10cbb148ace60bc2a22d609ec519");
const LEGACY_NFT: Address = address!("416b433906b1b72fa758e166e239c43d68dc6f29");
const LEGACY_IMPLEMENTATION: Address = address!("7155b84a704f0657975827c65ff6fe42e3a962bb");
const LEGACY_IDENTITIES: [(Address, B256); 5] = [
    (
        LEGACY_POOL,
        b256!("063ca35333cb7f2463f087d40ff9485475550abf4858a2f63c387d4d102b0f4f"),
    ),
    (
        address!("c28ad28853a547556780bebf7847628501a3bcbb"),
        b256!("36c3da904ca0b58544254cd0d978fe4801c32dc1f9e3b3e644487ef541299794"),
    ),
    (
        LEGACY_GAUGE,
        b256!("576a24f21989bca966fb1698aa060e9957ceabe9a970dadb98ba8dc740e845f1"),
    ),
    (
        LEGACY_IMPLEMENTATION,
        b256!("eb344e9f4bd301360b265a45cf417685e7ba46f7c53e03b592e7fc1fad53abdb"),
    ),
    (
        LEGACY_NFT,
        b256!("e46c8b86983505f15a6598c270339e696f5465478a32f17f1026d76e36fd69de"),
    ),
];
const LEGACY_CODE_TARGETS: [Address; 3] = [LEGACY_GAUGE, LEGACY_IMPLEMENTATION, LEGACY_NFT];

/// Aero CL pool proxy (EIP-1167) and shared implementation.
const AERO_POOL_PROXY_RUNTIME_CODE_HASH: B256 =
    b256!("55e3735acd886d76160bfd959db31dd476e73e46e8a0d70ac36e4c53ef5a2a45");
const AERO_IMPLEMENTATION: Address = address!("1642634c8cb1c9f1b84c9a1a08534f8c3f4e89f8");
const AERO_IMPLEMENTATION_RUNTIME_CODE_HASH: B256 =
    b256!("041172c7cbffc45627f9dd52727a0124cd6eeb2b5830ab392df54f71a6948693");
/// Aero CL gauge proxy and shared implementation.
const AERO_GAUGE_PROXY_RUNTIME_CODE_HASH: B256 =
    b256!("b03fb822ffeb7154c5d94f0206d76b417f4f97b2b0a744e07e289cfb5fd0c639");
const AERO_GAUGE_IMPLEMENTATION: Address = address!("89e91a960154b1ed6c093039a8f0725ab67d14e0");
const AERO_GAUGE_IMPLEMENTATION_RUNTIME_CODE_HASH: B256 =
    b256!("fdb516e0cf5b1c3f42789c4d7607a16e16b0ae5e73894e21e52a29c93b1ecee3");
/// Aero CL NonfungiblePositionManager (same address on Ethereum and Base,
/// distinct deployed runtimes).
const AERO_NFT: Address = address!("4857db576ee0e45f10525ec59527bfd10beec60b");
const AERO_ETHEREUM_POOL: Address = address!("ff35aa95669b5668ac6727fdcfc0d5582cdd78d1");
const AERO_ETHEREUM_GAUGE: Address = address!("0ee89bdfd62e3a5325416fb93c0f1601474a5b04");
const AERO_ETHEREUM_IDENTITIES: [(Address, B256); 5] = [
    (AERO_ETHEREUM_POOL, AERO_POOL_PROXY_RUNTIME_CODE_HASH),
    (AERO_IMPLEMENTATION, AERO_IMPLEMENTATION_RUNTIME_CODE_HASH),
    (AERO_ETHEREUM_GAUGE, AERO_GAUGE_PROXY_RUNTIME_CODE_HASH),
    (
        AERO_GAUGE_IMPLEMENTATION,
        AERO_GAUGE_IMPLEMENTATION_RUNTIME_CODE_HASH,
    ),
    (
        AERO_NFT,
        b256!("0e52c540913f0de73b9063379637246c59eff6e75ebad5ad7ed23f0bd591ab04"),
    ),
];
const AERO_ETHEREUM_CODE_TARGETS: [Address; 3] =
    [AERO_ETHEREUM_GAUGE, AERO_GAUGE_IMPLEMENTATION, AERO_NFT];
const AERO_BASE_POOL: Address = address!("eebe78d734dc4bac027061cee9bc12c5803fe4f8");
const AERO_BASE_GAUGE: Address = address!("84c029f26652d37bc153c39deb8767ec54037334");
const AERO_BASE_IDENTITIES: [(Address, B256); 5] = [
    (AERO_BASE_POOL, AERO_POOL_PROXY_RUNTIME_CODE_HASH),
    (AERO_IMPLEMENTATION, AERO_IMPLEMENTATION_RUNTIME_CODE_HASH),
    (AERO_BASE_GAUGE, AERO_GAUGE_PROXY_RUNTIME_CODE_HASH),
    (
        AERO_GAUGE_IMPLEMENTATION,
        AERO_GAUGE_IMPLEMENTATION_RUNTIME_CODE_HASH,
    ),
    (
        AERO_NFT,
        b256!("b2d0d8278354542651e2db288b71292635d9fd7bd28784f736373da801974599"),
    ),
];
const AERO_BASE_CODE_TARGETS: [Address; 3] = [AERO_BASE_GAUGE, AERO_GAUGE_IMPLEMENTATION, AERO_NFT];

const RUNTIMES: [StakingRuntime; 3] = [
    StakingRuntime {
        family: StakingFamily::Legacy,
        chain: 10,
        pool: LEGACY_POOL,
        gauge: LEGACY_GAUGE,
        nft: LEGACY_NFT,
        tick_spacing: 200,
        staked_slot: U256::from_limbs([15, 0, 0, 0]),
        spacing_shift: 160,
        identities: &LEGACY_IDENTITIES,
        code_targets: &LEGACY_CODE_TARGETS,
    },
    StakingRuntime {
        family: StakingFamily::AeroCl,
        chain: 1,
        pool: AERO_ETHEREUM_POOL,
        gauge: AERO_ETHEREUM_GAUGE,
        nft: AERO_NFT,
        tick_spacing: 50,
        staked_slot: AERO_CL_STAKED_LIQUIDITY_SLOT,
        spacing_shift: 176,
        identities: &AERO_ETHEREUM_IDENTITIES,
        code_targets: &AERO_ETHEREUM_CODE_TARGETS,
    },
    StakingRuntime {
        family: StakingFamily::AeroCl,
        chain: 8_453,
        pool: AERO_BASE_POOL,
        gauge: AERO_BASE_GAUGE,
        nft: AERO_NFT,
        tick_spacing: 100,
        staked_slot: AERO_CL_STAKED_LIQUIDITY_SLOT,
        spacing_shift: 176,
        identities: &AERO_BASE_IDENTITIES,
        code_targets: &AERO_BASE_CODE_TARGETS,
    },
];

pub(crate) fn runtime_for_pool(pool: Address) -> Option<StakingRuntime> {
    RUNTIMES
        .iter()
        .copied()
        .find(|runtime| runtime.pool == pool)
}

pub(crate) fn supports_pool(pool: Address) -> bool {
    runtime_for_pool(pool).is_some()
}

pub(crate) fn code_targets(pool: Address) -> Vec<Address> {
    runtime_for_pool(pool)
        .map(|runtime| runtime.code_targets.to_vec())
        .unwrap_or_default()
}

/// The gauge `topic0`s a reviewed runtime routes to its pool.
fn gauge_topics(runtime: StakingRuntime) -> Vec<B256> {
    match runtime.family {
        StakingFamily::Legacy => vec![abi::Deposit::SIGNATURE_HASH, abi::Withdraw::SIGNATURE_HASH],
        StakingFamily::AeroCl => {
            let mut topics = vec![
                aero_abi::Deposit::SIGNATURE_HASH,
                aero_abi::Withdraw::SIGNATURE_HASH,
                aero_abi::GaugeSettled::SIGNATURE_HASH,
            ];
            topics.extend(aero_claim_topics());
            topics
        }
    }
}

fn runtime_matches(runtime: StakingRuntime, snapshot: &evm_fork_cache::EvmSnapshot) -> bool {
    runtime
        .identities
        .iter()
        .all(|(address, hash)| snapshot.account_code_hash(*address) == Some(*hash))
        && snapshot.storage_value(runtime.pool, U256::from(3))
            == Some(U256::from_be_slice(runtime.gauge.as_slice()))
        && snapshot.storage_value(runtime.pool, U256::from(4))
            == Some(U256::from_be_slice(runtime.nft.as_slice()))
}

pub(crate) fn prepare(
    cache: &mut evm_fork_cache::EvmCache,
    batch: &ReactiveInputBatch,
    ownership: &super::AmmOwnershipIndex,
) -> SlipstreamStakingBatch {
    let chain = cache.chain_id();
    if batch.records().len() > MAX_RECORDS {
        return SlipstreamStakingBatch::default();
    }
    let mut output = SlipstreamStakingBatch::default();
    let mut snapshot = None;
    for runtime in RUNTIMES.iter().filter(|runtime| runtime.chain == chain) {
        let Some(instance) = ownership.active_pool(&PoolKey::Slipstream(runtime.pool)) else {
            continue;
        };
        // Ordinary swaps never allocate an extra cache snapshot.
        if !batch.records().iter().any(
            |record| matches!(&record.input, ReactiveInput::Log(log) if log.address() == runtime.gauge),
        ) {
            continue;
        }
        let snapshot = snapshot.get_or_insert_with(|| cache.snapshot());
        output.merge(SlipstreamStakingBatch::from_batch(
            batch, instance, snapshot,
        ));
    }
    output
}

const MAX_RECORDS: usize = 16_384;
const MASK: U256 = U256::from_limbs([u64::MAX, u64::MAX, 0, 0]);
const MASK_48: U256 = U256::from_limbs([0xffff_ffff_ffff, 0, 0, 0]);
const MASK_160: U256 = U256::from_limbs([u64::MAX, u64::MAX, u32::MAX as u64, 0]);
const Q128: U256 = U256::from_limbs([0, 0, 1, 0]);

/// Batch-derived evidence for a staking transition at one exact log position.
/// Fields are private: callers cannot manufacture a range/delta association.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SlipstreamStakingEvidence {
    key: EventKey,
    payload: B256,
    lower: i32,
    upper: i32,
    delta: i128,
    /// Emissions the gauge settled into the pool at this log
    /// (`GaugeSettled.delta`); zero for stakes and silent settlements.
    settlement: U256,
    /// The settlement ran through `pool.settleToBlock()` (a claim, or the
    /// claim step of a withdrawal), which also clears `rollover`.
    clears_rollover: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct TransactionKey {
    chain: u64,
    number: u64,
    hash: B256,
    parent: B256,
    timestamp: u64,
    transaction: B256,
    index: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct EventKey {
    transaction: TransactionKey,
    log: u64,
}

impl EventKey {
    fn context(context: &AdapterEventContext) -> Option<Self> {
        Some(Self {
            transaction: TransactionKey {
                chain: context.chain_id?,
                number: context.block_number?,
                hash: context.block_hash?,
                parent: context.parent_hash?,
                timestamp: context.block_timestamp?,
                transaction: context.transaction_hash?,
                index: context.transaction_index?,
            },
            log: context.log_index?,
        })
    }
}

/// One exact block the evidence is built for (driver / replay path).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StakingBlockRef {
    /// Block number.
    pub number: u64,
    /// Block hash.
    pub hash: B256,
    /// Parent hash.
    pub parent_hash: B256,
    /// Block timestamp.
    pub timestamp: u64,
}

/// Immutable proof index for one delivery batch. Build it before replaying that
/// batch; use `evidence` with the exact event context. It performs no I/O.
#[derive(Clone, Debug, Default)]
pub struct SlipstreamStakingBatch(BTreeMap<(Address, EventKey), SlipstreamStakingEvidence>);

impl SlipstreamStakingBatch {
    fn merge(&mut self, other: Self) {
        self.0.extend(other.0);
    }

    /// Build evidence only from logs visible to this pool generation, with the
    /// reviewed immutable runtime code and pool bindings present in `snapshot`.
    pub fn from_batch(
        batch: &ReactiveInputBatch,
        instance: &super::PoolInstanceId,
        snapshot: &evm_fork_cache::EvmSnapshot,
    ) -> Self {
        let output = Self::default();
        let Some(runtime) = instance.key().address().and_then(runtime_for_pool) else {
            return output;
        };
        if instance.key() != &PoolKey::Slipstream(runtime.pool)
            || snapshot.chain_id() != runtime.chain
            || !runtime_matches(runtime, snapshot)
        {
            return output;
        }
        if batch.records().len() > MAX_RECORDS {
            return output;
        }
        let handler = AmmPoolReactiveHandler::handler_id(instance);
        let mut transactions: BTreeMap<TransactionKey, BTreeMap<u64, &Log>> = BTreeMap::new();
        let mut invalid = std::collections::BTreeSet::new();
        let mut scopes = BTreeMap::new();
        for (index, record) in batch.records().iter().enumerate() {
            let visible = match batch.record_audience(index) {
                Some(DeliveryAudience::All) => true,
                Some(DeliveryAudience::Owners(owners)) => owners.contains(&handler),
                Some(DeliveryAudience::AllExcept(owners)) => !owners.contains(&handler),
                _ => false,
            };
            if !visible
                || !matches!(
                    batch.record_delivery_scope(index),
                    Some(DeliveryScope::Canonical | DeliveryScope::OwnerCatchup)
                )
            {
                continue;
            }
            let ReactiveInput::Log(log) = &record.input else {
                continue;
            };
            if ![runtime.pool, runtime.gauge, runtime.nft].contains(&log.address()) {
                continue;
            }
            let Some(block) = record.context.block else {
                continue;
            };
            let (Some(parent), Some(timestamp), Some(transaction), Some(tx_index), Some(log_index)) = (
                block.parent_hash,
                block.timestamp,
                log.transaction_hash,
                log.transaction_index,
                log.log_index,
            ) else {
                continue;
            };
            if record.context.chain_id != Some(runtime.chain)
                || log.removed
                || !matches!(record.context.chain_status, ChainStatus::Included { block: b, .. } | ChainStatus::Safe { block: b, .. } | ChainStatus::Finalized { block: b } if b == block)
                || log.block_hash != Some(block.hash)
                || log.block_number != Some(block.number)
                || log.block_timestamp.is_some_and(|value| value != timestamp)
                || record.context.transaction_index != Some(tx_index)
                || record.context.log_index != Some(log_index)
            {
                continue;
            }
            let key = TransactionKey {
                chain: runtime.chain,
                number: block.number,
                hash: block.hash,
                parent,
                timestamp,
                transaction,
                index: tx_index,
            };
            if let Some(previous) = scopes.insert(key, batch.record_delivery_scope(index))
                && previous != batch.record_delivery_scope(index)
            {
                invalid.insert(key);
            }
            if transactions
                .entry(key)
                .or_default()
                .insert(log_index, &log.inner)
                .is_some()
            {
                invalid.insert(key);
            }
        }
        Self::build(runtime, transactions, &invalid)
    }

    /// Build evidence for a reviewed pool from the complete, ordered logs of
    /// one exact block (the driver / historical replay path, where no
    /// delivery batch exists). Every log must carry its transaction hash,
    /// transaction index and log index and belong to `block`; logs that do
    /// not are ignored. The reviewed runtime identities and pool bindings
    /// must be resident in `snapshot`.
    pub fn from_block_logs<'a>(
        pool: Address,
        block: StakingBlockRef,
        logs: impl IntoIterator<Item = &'a alloy_rpc_types_eth::Log>,
        snapshot: &evm_fork_cache::EvmSnapshot,
    ) -> Self {
        let output = Self::default();
        let Some(runtime) = runtime_for_pool(pool) else {
            return output;
        };
        if snapshot.chain_id() != runtime.chain || !runtime_matches(runtime, snapshot) {
            return output;
        }
        let mut transactions: BTreeMap<TransactionKey, BTreeMap<u64, &Log>> = BTreeMap::new();
        let mut invalid = std::collections::BTreeSet::new();
        for log in logs {
            if ![runtime.pool, runtime.gauge, runtime.nft].contains(&log.address()) {
                continue;
            }
            let (Some(transaction), Some(tx_index), Some(log_index)) =
                (log.transaction_hash, log.transaction_index, log.log_index)
            else {
                continue;
            };
            if log.removed
                || log.block_hash.is_some_and(|hash| hash != block.hash)
                || log
                    .block_number
                    .is_some_and(|number| number != block.number)
                || log
                    .block_timestamp
                    .is_some_and(|timestamp| timestamp != block.timestamp)
            {
                continue;
            }
            let key = TransactionKey {
                chain: runtime.chain,
                number: block.number,
                hash: block.hash,
                parent: block.parent_hash,
                timestamp: block.timestamp,
                transaction,
                index: tx_index,
            };
            if transactions
                .entry(key)
                .or_default()
                .insert(log_index, &log.inner)
                .is_some()
            {
                invalid.insert(key);
            }
        }
        Self::build(runtime, transactions, &invalid)
    }

    fn build(
        runtime: StakingRuntime,
        transactions: BTreeMap<TransactionKey, BTreeMap<u64, &Log>>,
        invalid: &std::collections::BTreeSet<TransactionKey>,
    ) -> Self {
        let mut output = Self::default();
        for (transaction, logs) in transactions {
            if invalid.contains(&transaction) {
                continue;
            }
            let mut candidates = Vec::new();
            let mut used_transfers = std::collections::BTreeSet::new();
            let mut ambiguous = false;
            let mut transfers: BTreeMap<(U256, Address, Address), BTreeMap<u64, ()>> =
                BTreeMap::new();
            let mut collects: BTreeMap<U256, BTreeMap<u64, Address>> = BTreeMap::new();
            let mut settlements: BTreeMap<u64, U256> = BTreeMap::new();
            let mut claims: Vec<u64> = Vec::new();
            let mut deposit_windows: Vec<(u64, u64)> = Vec::new();
            for (&position, log) in &logs {
                if log.address == runtime.gauge
                    && runtime.family == StakingFamily::AeroCl
                    && log
                        .topics()
                        .first()
                        .is_some_and(|topic| aero_claim_topics().contains(topic))
                {
                    claims.push(position);
                }
                if log.address == runtime.nft {
                    if let Some(event) = canonical_event::<abi::Transfer>(log) {
                        transfers
                            .entry((event.tokenId, event.from, event.to))
                            .or_default()
                            .insert(position, ());
                    } else if let Some(event) = canonical_event::<abi::Collect>(log) {
                        collects
                            .entry(event.tokenId)
                            .or_default()
                            .insert(position, event.recipient);
                    }
                } else if log.address == runtime.gauge
                    && runtime.family == StakingFamily::AeroCl
                    && let Some(event) = canonical_event::<aero_abi::GaugeSettled>(log)
                {
                    settlements.insert(position, event.delta);
                }
            }
            for (&index, log) in &logs {
                if log.address != runtime.gauge {
                    continue;
                }
                let Some(stake) = decode_gauge_stake(runtime, log) else {
                    continue;
                };
                let Ok(amount) = i128::try_from(stake.amount) else {
                    continue;
                };
                let custody = if stake.deposit {
                    (stake.token, stake.sender, runtime.gauge)
                } else {
                    (stake.token, runtime.gauge, stake.sender)
                };
                let transfer = transfers
                    .get(&custody)
                    .and_then(|positions| positions.range(..index).next_back())
                    .map(|(&position, _)| position);
                let Some(transfer) = transfer else {
                    continue;
                };
                let collect = collects
                    .get(&stake.token)
                    .and_then(|positions| positions.range(..transfer).next_back())
                    .map(|(&position, &recipient)| (position, recipient));
                let Some((collect, recipient)) = collect else {
                    continue;
                };
                // A deposit collects to the position's owner; a withdrawal
                // collects to the claim recipient (the caller, or the account
                // when the caller is not approved for its claims).
                let recipient_ok = if stake.deposit {
                    recipient == stake.owner
                } else {
                    recipient == stake.sender || recipient == stake.owner
                };
                if !recipient_ok {
                    continue;
                }
                let Some(pool_log) = collect
                    .checked_sub(2)
                    .and_then(|position| logs.get(&position))
                else {
                    continue;
                };
                if pool_log.address != runtime.pool {
                    continue;
                }
                // Pool Collect and manager Collect have different signatures.
                // Decode the former through its canonical ABI name.
                let Some(range) = canonical_event::<pool_abi::Collect>(pool_log) else {
                    continue;
                };
                if range.recipient != recipient
                    || range.owner
                        != if stake.deposit {
                            runtime.nft
                        } else {
                            runtime.gauge
                        }
                {
                    continue;
                }
                if !used_transfers.insert(transfer) {
                    ambiguous = true;
                    break;
                }
                let apply_index = if stake.deposit { index } else { transfer };
                if stake.deposit {
                    // `pool.stake` runs after the manager's collect and before
                    // the Deposit event: a `GaugeSettled` in that window was
                    // emitted by this stake's own settlement.
                    deposit_windows.push((collect + 1, apply_index));
                }
                let proof = SlipstreamStakingEvidence {
                    payload: fingerprint(logs[&apply_index]),
                    key: EventKey {
                        transaction,
                        log: apply_index,
                    },
                    lower: range.tickLower.as_i32(),
                    upper: range.tickUpper.as_i32(),
                    delta: if stake.deposit { amount } else { -amount },
                    settlement: U256::ZERO,
                    // A withdrawal's claim step settled through
                    // `settleToBlock()` before the stake.
                    clears_rollover: !stake.deposit,
                };
                candidates.push(proof);
                if !stake.deposit {
                    candidates.push(SlipstreamStakingEvidence {
                        key: EventKey {
                            transaction,
                            log: index,
                        },
                        payload: fingerprint(log),
                        delta: 0,
                        clears_rollover: false,
                        ..proof
                    });
                }
            }
            // Every `GaugeSettled` is the pool's settlement at that log. It
            // ran through `settleToBlock()` (and cleared `rollover`) unless a
            // deposit's own stake or the swap that follows it pulled it
            // through `_settleToBlock`.
            for (&position, &delta) in &settlements {
                let by_deposit = deposit_windows
                    .iter()
                    .any(|(from, to)| (*from..*to).contains(&position));
                let by_swap = logs
                    .range(position + 1..)
                    .find(|(_, log)| log.address == runtime.pool || log.address == runtime.gauge)
                    .is_some_and(|(_, log)| {
                        log.address == runtime.pool
                            && log.topics().first() == Some(&pool_abi::Swap::SIGNATURE_HASH)
                    });
                candidates.push(SlipstreamStakingEvidence {
                    key: EventKey {
                        transaction,
                        log: position,
                    },
                    payload: fingerprint(logs[&position]),
                    lower: 0,
                    upper: 0,
                    delta: 0,
                    settlement: delta,
                    clears_rollover: !(by_deposit || by_swap),
                });
            }
            // A claim settles silently when the gauge emitted no
            // `GaugeSettled` (nothing accrued); `settleToBlock()` still
            // advances `lastUpdated` and clears `rollover`.
            for position in claims {
                candidates.push(SlipstreamStakingEvidence {
                    key: EventKey {
                        transaction,
                        log: position,
                    },
                    payload: fingerprint(logs[&position]),
                    lower: 0,
                    upper: 0,
                    delta: 0,
                    settlement: U256::ZERO,
                    clears_rollover: true,
                });
            }
            // Ambiguous associations invalidate the entire transaction proof set.
            let mut unique = std::collections::BTreeSet::new();
            if !ambiguous && candidates.iter().all(|proof| unique.insert(proof.key)) {
                output.0.extend(
                    candidates
                        .into_iter()
                        .map(|proof| ((runtime.pool, proof.key), proof)),
                );
            }
        }
        output
    }

    /// Find evidence for a reviewed pool and exact context in this batch.
    pub fn evidence(
        &self,
        pool: Address,
        context: &AdapterEventContext,
    ) -> Option<SlipstreamStakingEvidence> {
        self.0.get(&(pool, EventKey::context(context)?)).copied()
    }

    /// Number of attested staking transitions in this batch.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether this batch attests no staking transition.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

struct GaugeStake {
    deposit: bool,
    sender: Address,
    owner: Address,
    token: U256,
    amount: u128,
}

fn decode_gauge_stake(runtime: StakingRuntime, log: &Log) -> Option<GaugeStake> {
    let topic = log.topics().first()?;
    match runtime.family {
        StakingFamily::Legacy => {
            if *topic == abi::Deposit::SIGNATURE_HASH {
                canonical_event::<abi::Deposit>(log).map(|event| GaugeStake {
                    deposit: true,
                    sender: event.user,
                    owner: event.user,
                    token: event.tokenId,
                    amount: event.liquidityToStake,
                })
            } else if *topic == abi::Withdraw::SIGNATURE_HASH {
                canonical_event::<abi::Withdraw>(log).map(|event| GaugeStake {
                    deposit: false,
                    sender: event.user,
                    owner: event.user,
                    token: event.tokenId,
                    amount: event.liquidityToStake,
                })
            } else {
                None
            }
        }
        StakingFamily::AeroCl => {
            if *topic == aero_abi::Deposit::SIGNATURE_HASH {
                canonical_event::<aero_abi::Deposit>(log).map(|event| GaugeStake {
                    deposit: true,
                    sender: event.sender,
                    owner: event.owner,
                    token: event.tokenId,
                    amount: event.liquidityToStake,
                })
            } else if *topic == aero_abi::Withdraw::SIGNATURE_HASH {
                canonical_event::<aero_abi::Withdraw>(log).map(|event| GaugeStake {
                    deposit: false,
                    sender: event.sender,
                    owner: event.account,
                    token: event.tokenId,
                    amount: event.liquidityToStake,
                })
            } else {
                None
            }
        }
    }
}

mod pool_abi {
    alloy_sol_types::sol! {
        event Collect(address indexed owner, address recipient, int24 indexed tickLower, int24 indexed tickUpper, uint128 amount0, uint128 amount1);
        event Swap(address indexed sender, address indexed recipient, int256 amount0, int256 amount1, uint160 sqrtPriceX96, uint128 liquidity, int24 tick);
    }
}

pub(crate) fn sources(pool: &PoolRegistration, topics: Vec<B256>) -> Option<Vec<EventSource>> {
    let runtime = pool.key.address().and_then(runtime_for_pool)?;
    if pool.key != PoolKey::Slipstream(runtime.pool) {
        return None;
    }
    // NFT evidence is fetched by the complete-block driver only for token IDs
    // named by this gauge. Retain only transfers FROM the gauge as a standing
    // NFT interest: withdrawal state must apply before receiver callbacks.
    Some(vec![
        EventSource::direct(runtime.pool, topics),
        EventSource::adapter_defined(runtime.gauge, gauge_topics(runtime)),
        EventSource::adapter_defined(runtime.nft, vec![abi::Transfer::SIGNATURE_HASH])
            .with_indexed_topics([vec![runtime.gauge.into_word()], Vec::new(), Vec::new()]),
    ])
}

pub(crate) fn route(log: &Log, registry: &AdapterRegistry) -> Option<PoolKey> {
    RUNTIMES.iter().find_map(|runtime| {
        let key = PoolKey::Slipstream(runtime.pool);
        registry.pool(&key)?;
        let routed = log.address == runtime.gauge
            || (log.address == runtime.nft
                && match runtime.family {
                    StakingFamily::Legacy => true,
                    StakingFamily::AeroCl => log
                        .topics()
                        .get(1)
                        .is_some_and(|topic| *topic == runtime.gauge.into_word()),
                });
        routed.then_some(key)
    })
}

fn canonical_event<E: SolEvent>(log: &Log) -> Option<Log<E>> {
    let decoded = E::decode_log_validate(log).ok()?;
    // Alloy's type validation permits trailing data. Reject non-canonical
    // payloads and topic encodings before they can attest another event.
    (decoded.data.encode_log_data() == log.data).then_some(decoded)
}

fn fingerprint(log: &Log) -> B256 {
    let mut bytes = Vec::with_capacity(21 + log.topics().len() * 32 + log.data.data.len());
    bytes.extend_from_slice(log.address.as_slice());
    bytes.push(log.topics().len() as u8);
    for topic in log.topics() {
        bytes.extend_from_slice(topic.as_slice());
    }
    bytes.extend_from_slice(&log.data.data);
    alloy_primitives::keccak256(bytes)
}

fn failure(reason: &'static str) -> AdapterEventError {
    AdapterEventError::V3Transition(V3TransitionError::SlipstreamStakingEvidence(reason))
}

pub(crate) fn decode(
    pool: &PoolRegistration,
    log: &Log,
    state: &dyn StateView,
    context: &AdapterEventContext,
) -> Option<AdapterEventResult> {
    let runtime = pool.key.address().and_then(runtime_for_pool)?;
    if pool.protocol() != ProtocolId::Slipstream
        || ![runtime.gauge, runtime.nft].contains(&log.address)
    {
        return None;
    }
    let topic = log.topics().first().copied()?;
    let withdrawal_transfer = log.address == runtime.nft
        && topic == abi::Transfer::SIGNATURE_HASH
        && log
            .topics()
            .get(1)
            .is_some_and(|value| value.as_slice()[12..] == runtime.gauge.as_slice()[..]);
    let (deposit_topic, withdraw_topic) = match runtime.family {
        StakingFamily::Legacy => (abi::Deposit::SIGNATURE_HASH, abi::Withdraw::SIGNATURE_HASH),
        StakingFamily::AeroCl => (
            aero_abi::Deposit::SIGNATURE_HASH,
            aero_abi::Withdraw::SIGNATURE_HASH,
        ),
    };
    let deposit = log.address == runtime.gauge && topic == deposit_topic;
    let withdrawal = log.address == runtime.gauge && topic == withdraw_topic;
    let settlement = log.address == runtime.gauge
        && runtime.family == StakingFamily::AeroCl
        && (topic == aero_abi::GaugeSettled::SIGNATURE_HASH
            || aero_claim_topics().contains(&topic));
    if !deposit && !withdrawal && !withdrawal_transfer && !settlement {
        return Some(AdapterEventResult::ignored());
    }
    let kind = if deposit {
        AdapterEventKind::Deposit
    } else if withdrawal || withdrawal_transfer {
        AdapterEventKind::Withdraw
    } else {
        AdapterEventKind::Unknown
    };
    let result = context
        .slipstream_staking_evidence
        .ok_or_else(|| failure("missing transaction-local staking evidence"))
        .and_then(|evidence| {
            if evidence.payload != fingerprint(log) {
                return Err(failure("staking evidence payload mismatch"));
            }
            if settlement {
                derive_settlement(runtime, pool, state, context, evidence)
            } else {
                derive(runtime, pool, state, context, evidence)
            }
        });
    Some(match result {
        Ok(updates) => AdapterEventResult::event(
            AdapterEvent::new(
                pool.key.clone(),
                log.address,
                topic,
                kind,
                UpdateQuality::Exact,
            )
            .with_updates(updates),
        ),
        Err(error) => AdapterEventResult::event_with_error(
            AdapterEvent::new(
                pool.key.clone(),
                log.address,
                topic,
                kind,
                UpdateQuality::RequiresRepair,
            )
            .with_updates([StateUpdate::purge(runtime.pool, PurgeScope::AllStorage)])
            .with_repair(RepairAction::PurgeStorage(runtime.pool)),
            error,
        ),
    })
}

fn derive(
    runtime: StakingRuntime,
    pool: &PoolRegistration,
    state: &dyn StateView,
    context: &AdapterEventContext,
    evidence: SlipstreamStakingEvidence,
) -> Result<Vec<StateUpdate>, AdapterEventError> {
    if Some(evidence.key) != EventKey::context(context) {
        return Err(failure("staking event identity mismatch"));
    }
    let layout = layout_for(pool).ok_or_else(|| failure("missing staking layout"))?;
    super::v3_transition::validate_reviewed_slipstream_event(runtime.pool, layout, context)?;
    if layout.tick_spacing != runtime.tick_spacing {
        return Err(failure(
            "registration tick spacing does not match the staking runtime",
        ));
    }
    if evidence.lower < -887_272
        || evidence.upper > 887_272
        || evidence.lower >= evidence.upper
        || evidence.lower.rem_euclid(layout.tick_spacing) != 0
        || evidence.upper.rem_euclid(layout.tick_spacing) != 0
    {
        return Err(failure("invalid staking tick range"));
    }
    if evidence.delta == 0 {
        return Ok(Vec::new());
    }
    if !evidence.settlement.is_zero() {
        return Err(failure("a stake carries no settlement of its own"));
    }
    let address = runtime.pool;
    let read = |slot| {
        state
            .storage(address, slot)
            .ok_or(AdapterEventError::MissingState { address, slot })
    };
    let slot0 = read(layout.slot0_slot)?;
    let raw_tick = ((slot0 >> 160_usize) & U256::from(0xff_ffff)).to::<u32>();
    let tick = ((raw_tick << 8) as i32) >> 8;
    let word = read(runtime.staked_slot)?;
    let staked = word & MASK;
    let active = read(layout.liquidity_slot)? & MASK;
    if staked > active {
        return Err(failure("parent staked liquidity exceeds active liquidity"));
    }
    if ((word >> runtime.spacing_shift) & U256::from(0xff_ffff)) != U256::from(runtime.tick_spacing)
        || ((slot0 >> 232_usize) & U256::from(255)) != U256::from(1)
        || !(-887_272..=887_272).contains(&tick)
    {
        return Err(failure("invalid parent staking geometry"));
    }
    let mut updates = Vec::with_capacity(8);
    let in_range = tick >= evidence.lower && tick < evidence.upper;
    let mut staked_word = word;
    if runtime.family == StakingFamily::AeroCl {
        // A withdrawal's claim step runs `settleToBlock()` unconditionally; a
        // deposit's `stake` settles only when its range covers the current
        // tick. Emissions the gauge pulled were applied at their
        // `GaugeSettled` log, so this settlement is the silent one (nothing
        // accrued, or the block already settled); it advances the staked
        // seconds accumulator and `lastUpdated` for the liquidity staked
        // before this stake.
        if evidence.delta < 0 || in_range {
            staked_word = settle_aero_to_block(
                address,
                staked_word,
                staked,
                U256::ZERO,
                context
                    .block_timestamp
                    .ok_or_else(|| failure("missing block timestamp"))?,
                &read,
                &mut updates,
            )?;
        }
        if evidence.clears_rollover {
            updates.push(StateUpdate::slot(
                address,
                AERO_CL_ROLLOVER_SLOT,
                U256::ZERO,
            ));
        }
    }
    if in_range {
        let next = if evidence.delta > 0 {
            staked.checked_add(U256::from(evidence.delta as u128))
        } else {
            staked.checked_sub(U256::from(evidence.delta.unsigned_abs()))
        }
        .filter(|value| *value <= MASK && *value <= active)
        .ok_or_else(|| failure("staking delta exceeds active liquidity"))?;
        staked_word = (staked_word & !MASK) | next;
    }
    if staked_word != word {
        updates.push(StateUpdate::slot(address, runtime.staked_slot, staked_word));
    }
    for (tick, delta) in [
        (evidence.lower, evidence.delta),
        (evidence.upper, -evidence.delta),
    ] {
        let keys = slipstream_tick_info_storage_keys_with_base(tick, layout.ticks_base_slot);
        let flag = (read(keys[5])? >> 248_usize) & U256::from(255);
        if flag > U256::from(1) {
            return Err(failure("invalid staking tick initialization flag"));
        }
        let initialized = flag == U256::from(1);
        if !initialized {
            continue;
        }
        let word = read(keys[1])?;
        let net = (word & MASK).to::<u128>() as i128;
        let next = net
            .checked_add(delta)
            .ok_or_else(|| failure("staking tick delta overflows int128"))?;
        if U256::from(next.unsigned_abs()) > (read(keys[0])? & MASK) {
            return Err(failure("staked tick net exceeds gross liquidity"));
        }
        updates.push(StateUpdate::slot(
            address,
            keys[1],
            (word & !MASK) | U256::from(next as u128),
        ));
    }
    Ok(updates)
}

/// The pool settlement a gauge log attests: `GaugeSettled.delta` credited
/// to the liquidity staked at that moment (or banked into `rollover`), the
/// staked-seconds accumulator and `lastUpdated` advanced, and `rollover`
/// cleared when the settlement ran through `settleToBlock()`.
fn derive_settlement(
    runtime: StakingRuntime,
    pool: &PoolRegistration,
    state: &dyn StateView,
    context: &AdapterEventContext,
    evidence: SlipstreamStakingEvidence,
) -> Result<Vec<StateUpdate>, AdapterEventError> {
    if Some(evidence.key) != EventKey::context(context) {
        return Err(failure("settlement event identity mismatch"));
    }
    if evidence.delta != 0 {
        return Err(failure("a settlement carries no staked-liquidity delta"));
    }
    let layout = layout_for(pool).ok_or_else(|| failure("missing staking layout"))?;
    super::v3_transition::validate_reviewed_slipstream_event(runtime.pool, layout, context)?;
    let address = runtime.pool;
    let read = |slot| {
        state
            .storage(address, slot)
            .ok_or(AdapterEventError::MissingState { address, slot })
    };
    let word = read(runtime.staked_slot)?;
    if ((word >> runtime.spacing_shift) & U256::from(0xff_ffff)) != U256::from(runtime.tick_spacing)
    {
        return Err(failure("invalid parent staking geometry"));
    }
    let mut updates = Vec::with_capacity(4);
    let settled = settle_aero_to_block(
        address,
        word,
        word & MASK,
        evidence.settlement,
        context
            .block_timestamp
            .ok_or_else(|| failure("missing block timestamp"))?,
        &read,
        &mut updates,
    )?;
    if settled != word {
        updates.push(StateUpdate::slot(address, runtime.staked_slot, settled));
    }
    if evidence.clears_rollover {
        updates.push(StateUpdate::slot(
            address,
            AERO_CL_ROLLOVER_SLOT,
            U256::ZERO,
        ));
    }
    Ok(updates)
}

/// Apply the Aero CL pool's `_settleToBlock` to the exact parent accounting:
/// credit `delta` emissions to the liquidity staked before this stake (or bank
/// them into `rollover`), advance the staked-seconds accumulator and
/// `lastUpdated`. Returns the staked word with its `lastUpdated` bits updated.
pub(crate) fn settle_aero_to_block(
    address: Address,
    staked_word: U256,
    staked_before: U256,
    delta: U256,
    block_timestamp: u64,
    read: &dyn Fn(U256) -> Result<U256, AdapterEventError>,
    updates: &mut Vec<StateUpdate>,
) -> Result<U256, AdapterEventError> {
    let now = U256::from(block_timestamp) & MASK_48;
    let last_updated = (staked_word >> 128_usize) & MASK_48;
    if now == last_updated {
        if !delta.is_zero() {
            return Err(failure(
                "settlement emitted inside an already settled block",
            ));
        }
        return Ok(staked_word);
    }
    if now < last_updated {
        return Err(failure("block timestamp precedes the pool's lastUpdated"));
    }
    if staked_before.is_zero() {
        let rollover = read(AERO_CL_ROLLOVER_SLOT)?
            .checked_add(delta)
            .ok_or_else(|| failure("rollover overflows"))?;
        updates.push(StateUpdate::slot(address, AERO_CL_ROLLOVER_SLOT, rollover));
    } else {
        if !delta.is_zero() {
            let growth = read(AERO_CL_REWARD_GROWTH_SLOT)?
                .wrapping_add(super::v3_transition::mul_div(delta, Q128, staked_before)?);
            updates.push(StateUpdate::slot(
                address,
                AERO_CL_REWARD_GROWTH_SLOT,
                growth,
            ));
        }
        let elapsed = now - last_updated;
        let per_staked = ((elapsed << 128_usize) / staked_before) & MASK_160;
        let cumulative = read(AERO_CL_SECONDS_PER_STAKED_LIQUIDITY_SLOT)?;
        let next = (cumulative & MASK_160)
            .checked_add(per_staked)
            .filter(|value| *value <= MASK_160)
            .ok_or_else(|| failure("staked seconds accumulator overflows uint160"))?;
        updates.push(StateUpdate::slot(
            address,
            AERO_CL_SECONDS_PER_STAKED_LIQUIDITY_SLOT,
            (cumulative & !MASK_160) | next,
        ));
    }
    Ok((staked_word & !(MASK_48 << 128_usize)) | (now << 128_usize))
}
