//! Transaction-local Slipstream staking evidence and quote-state transitions.
//!
//! The pool does not emit `stake`. The reviewed gauge and NFT manager jointly
//! attest its range and delta. Evidence is rebuilt for each delivered batch;
//! no token ownership or partially decoded transaction survives a rollback.

use super::storage::{layout_for, slipstream_tick_info_storage_keys_with_base};
use super::{
    AdapterEvent, AdapterEventContext, AdapterEventError, AdapterEventKind, AdapterEventResult,
    AdapterRegistry, AmmPoolReactiveHandler, EventSource, PoolKey, PoolRegistration, ProtocolId,
    PurgeScope, RepairAction, StateUpdate, StateView, UpdateQuality, V3TransitionError,
};
use alloy_primitives::{Address, B256, Log, U256, address};
use alloy_sol_types::SolEvent;
use evm_fork_cache::reactive::{
    ChainStatus, DeliveryAudience, DeliveryScope, ReactiveInput, ReactiveInputBatch,
};
use std::collections::BTreeMap;

mod abi {
    alloy_sol_types::sol! {
        event Deposit(address indexed user, uint256 indexed tokenId, uint128 indexed liquidityToStake);
        event Withdraw(address indexed user, uint256 indexed tokenId, uint128 indexed liquidityToStake);
        event Transfer(address indexed from, address indexed to, uint256 indexed tokenId);
        event Collect(uint256 indexed tokenId, address recipient, uint256 amount0, uint256 amount1);
    }
}

const POOL: Address = address!("173cdc71e29d5cffa6d090ad99f555a24b8831f9");
const GAUGE: Address = address!("41160e66fcaa10cbb148ace60bc2a22d609ec519");
const NFT: Address = address!("416b433906b1b72fa758e166e239c43d68dc6f29");

const IMPLEMENTATION: Address = address!("7155b84a704f0657975827c65ff6fe42e3a962bb");

pub(crate) fn supports_pool(pool: Address) -> bool {
    pool == POOL
}

pub(crate) fn code_targets(pool: Address) -> Vec<Address> {
    if pool == POOL {
        vec![GAUGE, IMPLEMENTATION, NFT]
    } else {
        Vec::new()
    }
}

fn runtime_matches(snapshot: &evm_fork_cache::EvmSnapshot) -> bool {
    use alloy_primitives::b256;
    let identities = [
        (
            POOL,
            b256!("063ca35333cb7f2463f087d40ff9485475550abf4858a2f63c387d4d102b0f4f"),
        ),
        (
            address!("c28ad28853a547556780bebf7847628501a3bcbb"),
            b256!("36c3da904ca0b58544254cd0d978fe4801c32dc1f9e3b3e644487ef541299794"),
        ),
        (
            GAUGE,
            b256!("576a24f21989bca966fb1698aa060e9957ceabe9a970dadb98ba8dc740e845f1"),
        ),
        (
            IMPLEMENTATION,
            b256!("eb344e9f4bd301360b265a45cf417685e7ba46f7c53e03b592e7fc1fad53abdb"),
        ),
        (
            NFT,
            b256!("e46c8b86983505f15a6598c270339e696f5465478a32f17f1026d76e36fd69de"),
        ),
    ];
    identities
        .into_iter()
        .all(|(address, hash)| snapshot.account_code_hash(address) == Some(hash))
        && snapshot.storage_value(POOL, U256::from(3))
            == Some(U256::from_be_slice(GAUGE.as_slice()))
        && snapshot.storage_value(POOL, U256::from(4)) == Some(U256::from_be_slice(NFT.as_slice()))
}

pub(crate) fn prepare(
    cache: &mut evm_fork_cache::EvmCache,
    batch: &ReactiveInputBatch,
    ownership: &super::AmmOwnershipIndex,
) -> SlipstreamStakingBatch {
    if cache.chain_id() != 10 || batch.records().len() > MAX_RECORDS {
        return SlipstreamStakingBatch::default();
    }
    let Some(instance) = ownership.active_pool(&PoolKey::Slipstream(POOL)) else {
        return SlipstreamStakingBatch::default();
    };
    // Ordinary swaps never allocate an extra cache snapshot.
    if !batch
        .records()
        .iter()
        .any(|record| matches!(&record.input, ReactiveInput::Log(log) if log.address() == GAUGE))
    {
        return SlipstreamStakingBatch::default();
    }
    SlipstreamStakingBatch::from_batch(batch, instance, &cache.snapshot())
}

const MAX_RECORDS: usize = 16_384;
const MASK: U256 = U256::from_limbs([u64::MAX, u64::MAX, 0, 0]);

/// Batch-derived evidence for a staking transition at one exact log position.
/// Fields are private: callers cannot manufacture a range/delta association.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SlipstreamStakingEvidence {
    key: EventKey,
    payload: B256,
    lower: i32,
    upper: i32,
    delta: i128,
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

/// Immutable proof index for one delivery batch. Build it before replaying that
/// batch; use `evidence` with the exact event context. It performs no I/O.
#[derive(Clone, Debug, Default)]
pub struct SlipstreamStakingBatch(BTreeMap<EventKey, SlipstreamStakingEvidence>);

impl SlipstreamStakingBatch {
    /// Build evidence only from logs visible to this pool generation, with the
    /// reviewed immutable runtime code and pool bindings present in `snapshot`.
    pub fn from_batch(
        batch: &ReactiveInputBatch,
        instance: &super::PoolInstanceId,
        snapshot: &evm_fork_cache::EvmSnapshot,
    ) -> Self {
        let mut output = Self::default();
        if instance.key() != &PoolKey::Slipstream(POOL)
            || snapshot.chain_id() != 10
            || !runtime_matches(snapshot)
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
            if ![POOL, GAUGE, NFT].contains(&log.address()) {
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
            if record.context.chain_id != Some(10)
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
                chain: 10,
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
            for (&position, log) in &logs {
                if log.address != NFT {
                    continue;
                }
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
            }
            for (&index, log) in &logs {
                if log.address != GAUGE {
                    continue;
                }
                let decoded = if log.topics().first() == Some(&abi::Deposit::SIGNATURE_HASH) {
                    canonical_event::<abi::Deposit>(log)
                        .map(|event| (true, event.user, event.tokenId, event.liquidityToStake))
                } else if log.topics().first() == Some(&abi::Withdraw::SIGNATURE_HASH) {
                    canonical_event::<abi::Withdraw>(log)
                        .map(|event| (false, event.user, event.tokenId, event.liquidityToStake))
                } else {
                    None
                };
                let Some((deposit, owner, token, amount)) = decoded else {
                    continue;
                };
                let Ok(amount) = i128::try_from(amount) else {
                    continue;
                };
                let custody = if deposit {
                    (token, owner, GAUGE)
                } else {
                    (token, GAUGE, owner)
                };
                let transfer = transfers
                    .get(&custody)
                    .and_then(|positions| positions.range(..index).next_back())
                    .map(|(&position, _)| position);
                let Some(transfer) = transfer else {
                    continue;
                };
                let collect = collects
                    .get(&token)
                    .and_then(|positions| positions.range(..transfer).next_back())
                    .map(|(&position, &recipient)| (position, recipient));
                let Some((collect, recipient)) = collect else {
                    continue;
                };
                if recipient != owner {
                    continue;
                }
                let Some(pool_log) = collect
                    .checked_sub(2)
                    .and_then(|position| logs.get(&position))
                else {
                    continue;
                };
                if pool_log.address != POOL {
                    continue;
                }
                // Pool Collect and manager Collect have different signatures.
                // Decode the former through its canonical ABI name.
                let Some(range) = canonical_event::<pool_abi::Collect>(pool_log) else {
                    continue;
                };
                if range.recipient != owner || range.owner != if deposit { NFT } else { GAUGE } {
                    continue;
                }
                if !used_transfers.insert(transfer) {
                    ambiguous = true;
                    break;
                }
                let apply_index = if deposit { index } else { transfer };
                let proof = SlipstreamStakingEvidence {
                    payload: fingerprint(logs[&apply_index]),
                    key: EventKey {
                        transaction,
                        log: apply_index,
                    },
                    lower: range.tickLower.as_i32(),
                    upper: range.tickUpper.as_i32(),
                    delta: if deposit { amount } else { -amount },
                };
                candidates.push(proof);
                if !deposit {
                    candidates.push(SlipstreamStakingEvidence {
                        key: EventKey {
                            transaction,
                            log: index,
                        },
                        payload: fingerprint(log),
                        delta: 0,
                        ..proof
                    });
                }
            }
            // Ambiguous associations invalidate the entire transaction proof set.
            let mut unique = std::collections::BTreeSet::new();
            if !ambiguous && candidates.iter().all(|proof| unique.insert(proof.key)) {
                output
                    .0
                    .extend(candidates.into_iter().map(|proof| (proof.key, proof)));
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
        (pool == POOL)
            .then(|| self.0.get(&EventKey::context(context)?).copied())
            .flatten()
    }
}

mod pool_abi {
    alloy_sol_types::sol! {
        event Collect(address indexed owner, address recipient, int24 indexed tickLower, int24 indexed tickUpper, uint128 amount0, uint128 amount1);
    }
}

pub(crate) fn sources(pool: &PoolRegistration, mut topics: Vec<B256>) -> Option<Vec<EventSource>> {
    if pool.key != PoolKey::Slipstream(POOL) {
        return None;
    }
    // One shared topic set lets the subscriber consolidate all three emitters
    // into one provider filter. Runtime-qualified emitters and local decoding
    // retain the exact semantic boundary after this small provider superset.
    topics.extend([
        abi::Deposit::SIGNATURE_HASH,
        abi::Withdraw::SIGNATURE_HASH,
        abi::Collect::SIGNATURE_HASH,
        abi::Transfer::SIGNATURE_HASH,
    ]);
    Some(vec![
        EventSource::direct(POOL, topics.clone()),
        EventSource::adapter_defined(GAUGE, topics.clone()),
        EventSource::adapter_defined(NFT, topics),
    ])
}

pub(crate) fn route(log: &Log, registry: &AdapterRegistry) -> Option<PoolKey> {
    let key = PoolKey::Slipstream(POOL);
    registry.pool(&key)?;
    ([GAUGE, NFT].contains(&log.address)).then_some(key)
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
    if pool.protocol() != ProtocolId::Slipstream
        || pool.key.address() != Some(POOL)
        || ![GAUGE, NFT].contains(&log.address)
    {
        return None;
    }
    let topic = log.topics().first().copied()?;
    let withdrawal_transfer = log.address == NFT
        && topic == abi::Transfer::SIGNATURE_HASH
        && log
            .topics()
            .get(1)
            .is_some_and(|value| value.as_slice()[12..] == GAUGE.as_slice()[..]);
    let deposit = log.address == GAUGE && topic == abi::Deposit::SIGNATURE_HASH;
    let withdrawal = log.address == GAUGE && topic == abi::Withdraw::SIGNATURE_HASH;
    if !deposit && !withdrawal && !withdrawal_transfer {
        return Some(AdapterEventResult::ignored());
    }
    let kind = if deposit {
        AdapterEventKind::Deposit
    } else {
        AdapterEventKind::Withdraw
    };
    let result = context
        .slipstream_staking_evidence
        .ok_or_else(|| failure("missing transaction-local staking evidence"))
        .and_then(|evidence| {
            if evidence.payload != fingerprint(log) {
                return Err(failure("staking evidence payload mismatch"));
            }
            derive(pool, state, context, evidence)
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
            .with_updates([StateUpdate::purge(POOL, PurgeScope::AllStorage)])
            .with_repair(RepairAction::PurgeStorage(POOL)),
            error,
        ),
    })
}

fn derive(
    pool: &PoolRegistration,
    state: &dyn StateView,
    context: &AdapterEventContext,
    evidence: SlipstreamStakingEvidence,
) -> Result<Vec<StateUpdate>, AdapterEventError> {
    if Some(evidence.key) != EventKey::context(context) {
        return Err(failure("staking event identity mismatch"));
    }
    let layout = layout_for(pool).ok_or_else(|| failure("missing staking layout"))?;
    super::v3_transition::validate_reviewed_slipstream_event(POOL, layout, context)?;
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
    let read = |slot| {
        state
            .storage(POOL, slot)
            .ok_or(AdapterEventError::MissingState {
                address: POOL,
                slot,
            })
    };
    let slot0 = read(layout.slot0_slot)?;
    let raw_tick = ((slot0 >> 160_usize) & U256::from(0xff_ffff)).to::<u32>();
    let tick = ((raw_tick << 8) as i32) >> 8;
    let word = read(U256::from(15))?;
    let staked = word & MASK;
    let active = read(layout.liquidity_slot)? & MASK;
    if staked > active {
        return Err(failure("parent staked liquidity exceeds active liquidity"));
    }
    if ((word >> 160_usize) & U256::from(0xff_ffff)) != U256::from(200)
        || ((slot0 >> 232_usize) & U256::from(255)) != U256::from(1)
        || !(-887_272..=887_272).contains(&tick)
    {
        return Err(failure("invalid parent staking geometry"));
    }
    let mut updates = Vec::with_capacity(3);
    if tick >= evidence.lower && tick < evidence.upper {
        let next = if evidence.delta > 0 {
            staked.checked_add(U256::from(evidence.delta as u128))
        } else {
            staked.checked_sub(U256::from(evidence.delta.unsigned_abs()))
        }
        .filter(|value| *value <= MASK && *value <= active)
        .ok_or_else(|| failure("staking delta exceeds active liquidity"))?;
        updates.push(StateUpdate::slot(
            POOL,
            U256::from(15),
            (word & !MASK) | next,
        ));
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
            POOL,
            keys[1],
            (word & !MASK) | U256::from(next as u128),
        ));
    }
    Ok(updates)
}
