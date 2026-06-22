//! Temporary compatibility layer for legacy cache-sync code.
//!
//! The new `evm-fork-cache` release intentionally removed protocol-specific
//! metadata and V3 tick snapshot storage from `EvmCache`. The legacy
//! `cache_sync` modules still expect those helpers while we migrate protocol
//! ownership into this crate. This module restores the old call surface over a
//! process-local sidecar store so the existing paths keep compiling. Treat this
//! as transitional glue, not the final persistence model for AMM metadata.

use std::collections::HashMap;
use std::sync::{OnceLock, RwLock, RwLockReadGuard, RwLockWriteGuard};

use alloy_primitives::{Address, B256, U256};
use anyhow::Result;
use evm_fork_cache::cache::{EvmCache, ImmutableDataCache};
use evm_fork_cache::{PurgeScope, StateUpdate};
use serde::{Deserialize, Serialize};

use crate::adapters::storage::{
    v3_tick_bitmap_storage_key_with_base, v3_tick_info_storage_keys_with_base,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct V2PoolMetadata {
    pub token0: Address,
    pub token1: Address,
    pub last_block_timestamp: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct V3PoolMetadata {
    pub token0: Address,
    pub token1: Address,
    pub fee: u32,
    pub tick_spacing: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct BalancerPoolMetadata {
    pub tokens: Vec<Address>,
    pub weights: Vec<U256>,
    pub swap_fee: U256,
    pub last_change_block: U256,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct TickInfo {
    pub liquidity_gross: u128,
    pub liquidity_net: i128,
    pub initialized: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct SerializableTickInfo {
    pub liquidity_gross: u128,
    pub liquidity_net: i128,
    pub initialized: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct V3PoolTickSnapshot {
    pub tick_bitmap: HashMap<String, U256>,
    pub ticks: HashMap<String, SerializableTickInfo>,
    pub last_liquidity: u128,
    pub last_tick: i32,
}

impl V3PoolTickSnapshot {
    pub fn from_pool_data(
        tick_bitmap: &HashMap<i16, U256>,
        ticks: &HashMap<i32, TickInfo>,
        liquidity: u128,
        tick: i32,
    ) -> Self {
        Self {
            tick_bitmap: tick_bitmap
                .iter()
                .map(|(key, value)| (key.to_string(), *value))
                .collect(),
            ticks: ticks
                .iter()
                .map(|(key, value)| {
                    (
                        key.to_string(),
                        SerializableTickInfo {
                            liquidity_gross: value.liquidity_gross,
                            liquidity_net: value.liquidity_net,
                            initialized: value.initialized,
                        },
                    )
                })
                .collect(),
            last_liquidity: liquidity,
            last_tick: tick,
        }
    }

    pub fn to_tick_bitmap(&self) -> HashMap<i16, U256> {
        self.tick_bitmap
            .iter()
            .filter_map(|(key, value)| key.parse::<i16>().ok().map(|key| (key, *value)))
            .collect()
    }

    pub fn to_ticks(&self) -> HashMap<i32, TickInfo> {
        self.ticks
            .iter()
            .filter_map(|(key, value)| {
                key.parse::<i32>().ok().map(|key| {
                    (
                        key,
                        TickInfo {
                            liquidity_gross: value.liquidity_gross,
                            liquidity_net: value.liquidity_net,
                            initialized: value.initialized,
                        },
                    )
                })
            })
            .collect()
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct V3TickSnapshotCache {
    snapshots: HashMap<Address, V3PoolTickSnapshot>,
}

impl V3TickSnapshotCache {
    fn get(&self, address: Address) -> Option<V3PoolTickSnapshot> {
        self.snapshots.get(&address).cloned()
    }

    fn set(&mut self, address: Address, snapshot: V3PoolTickSnapshot) {
        self.snapshots.insert(address, snapshot);
    }
}

pub(crate) struct TickSnapshotCacheReadGuard(RwLockReadGuard<'static, V3TickSnapshotCache>);

impl TickSnapshotCacheReadGuard {
    pub(crate) fn get(&self, address: Address) -> Option<V3PoolTickSnapshot> {
        self.0.get(address)
    }
}

pub(crate) struct TickSnapshotCacheWriteGuard(RwLockWriteGuard<'static, V3TickSnapshotCache>);

impl TickSnapshotCacheWriteGuard {
    pub(crate) fn set(&mut self, address: Address, snapshot: V3PoolTickSnapshot) {
        self.0.set(address, snapshot);
    }
}

#[derive(Default)]
struct ProtocolMetadataStore {
    v2_pools: HashMap<Address, V2PoolMetadata>,
    v3_pools: HashMap<Address, V3PoolMetadata>,
    balancer_pools: HashMap<B256, BalancerPoolMetadata>,
}

static PROTOCOL_METADATA: OnceLock<RwLock<ProtocolMetadataStore>> = OnceLock::new();
static TICK_SNAPSHOTS: OnceLock<RwLock<V3TickSnapshotCache>> = OnceLock::new();

fn protocol_metadata() -> &'static RwLock<ProtocolMetadataStore> {
    PROTOCOL_METADATA.get_or_init(|| RwLock::new(ProtocolMetadataStore::default()))
}

fn tick_snapshots() -> &'static RwLock<V3TickSnapshotCache> {
    TICK_SNAPSHOTS.get_or_init(|| RwLock::new(V3TickSnapshotCache::default()))
}

pub(crate) trait ImmutableDataCacheProtocolExt {
    fn get_v2_pool(&self, address: Address) -> Option<V2PoolMetadata>;
    fn set_v2_pool(&mut self, address: Address, metadata: V2PoolMetadata);
    fn get_v3_pool(&self, address: Address) -> Option<V3PoolMetadata>;
    fn set_v3_pool(&mut self, address: Address, metadata: V3PoolMetadata);
    fn get_balancer_pool(&self, pool_id: B256) -> Option<BalancerPoolMetadata>;
    fn set_balancer_pool(&mut self, pool_id: B256, metadata: BalancerPoolMetadata);
}

impl ImmutableDataCacheProtocolExt for ImmutableDataCache {
    fn get_v2_pool(&self, address: Address) -> Option<V2PoolMetadata> {
        protocol_metadata()
            .read()
            .ok()?
            .v2_pools
            .get(&address)
            .cloned()
    }

    fn set_v2_pool(&mut self, address: Address, metadata: V2PoolMetadata) {
        if let Ok(mut store) = protocol_metadata().write() {
            store.v2_pools.insert(address, metadata);
        }
    }

    fn get_v3_pool(&self, address: Address) -> Option<V3PoolMetadata> {
        protocol_metadata()
            .read()
            .ok()?
            .v3_pools
            .get(&address)
            .cloned()
    }

    fn set_v3_pool(&mut self, address: Address, metadata: V3PoolMetadata) {
        if let Ok(mut store) = protocol_metadata().write() {
            store.v3_pools.insert(address, metadata);
        }
    }

    fn get_balancer_pool(&self, pool_id: B256) -> Option<BalancerPoolMetadata> {
        protocol_metadata()
            .read()
            .ok()?
            .balancer_pools
            .get(&pool_id)
            .cloned()
    }

    fn set_balancer_pool(&mut self, pool_id: B256, metadata: BalancerPoolMetadata) {
        if let Ok(mut store) = protocol_metadata().write() {
            store.balancer_pools.insert(pool_id, metadata);
        }
    }
}

pub(crate) trait EvmCacheProtocolExt {
    fn has_pool_storage(&self, address: Address) -> bool;
    fn pool_storage_slot_count(&self, address: Address) -> usize;
    fn purge_pool_storage(&mut self, address: Address) -> usize;
    fn purge_pool_slots(&mut self, address: Address, slots: &[U256]) -> usize;
    fn inject_v2_pool_metadata(
        &mut self,
        pool_address: Address,
        metadata: &V2PoolMetadata,
    ) -> Result<()>;
    fn inject_v3_tick_bitmap_with_base(
        &mut self,
        pool_address: Address,
        tick_bitmap: &HashMap<i16, U256>,
        base_slot: U256,
    ) -> Result<usize>;
    fn inject_v3_ticks_with_base(
        &mut self,
        pool_address: Address,
        ticks: &HashMap<i32, TickInfo>,
        ticks_slot: U256,
    ) -> Result<usize>;
    fn tick_snapshot_cache(&self) -> TickSnapshotCacheReadGuard;
    fn tick_snapshot_cache_mut(&mut self) -> TickSnapshotCacheWriteGuard;
}

impl EvmCacheProtocolExt for EvmCache {
    fn has_pool_storage(&self, address: Address) -> bool {
        self.has_contract_storage(address)
    }

    fn pool_storage_slot_count(&self, address: Address) -> usize {
        self.contract_storage_slot_count(address)
    }

    fn purge_pool_storage(&mut self, address: Address) -> usize {
        self.apply_update(&StateUpdate::purge(address, PurgeScope::AllStorage))
            .purged
            .first()
            .map(|record| record.slots_removed)
            .unwrap_or(0)
    }

    fn purge_pool_slots(&mut self, address: Address, slots: &[U256]) -> usize {
        self.apply_update(&StateUpdate::purge(
            address,
            PurgeScope::Slots(slots.to_vec()),
        ))
        .purged
        .first()
        .map(|record| record.slots_removed)
        .unwrap_or(0)
    }

    fn inject_v2_pool_metadata(
        &mut self,
        pool_address: Address,
        metadata: &V2PoolMetadata,
    ) -> Result<()> {
        const TOKEN0_SLOT: U256 = U256::from_limbs([6, 0, 0, 0]);
        const TOKEN1_SLOT: U256 = U256::from_limbs([7, 0, 0, 0]);

        self.apply_updates(&[
            StateUpdate::slot(
                pool_address,
                TOKEN0_SLOT,
                U256::from_be_slice(metadata.token0.as_slice()),
            ),
            StateUpdate::slot(
                pool_address,
                TOKEN1_SLOT,
                U256::from_be_slice(metadata.token1.as_slice()),
            ),
        ]);
        Ok(())
    }

    fn inject_v3_tick_bitmap_with_base(
        &mut self,
        pool_address: Address,
        tick_bitmap: &HashMap<i16, U256>,
        base_slot: U256,
    ) -> Result<usize> {
        let updates: Vec<_> = tick_bitmap
            .iter()
            .map(|(&word_position, &bitmap)| {
                StateUpdate::slot(
                    pool_address,
                    v3_tick_bitmap_storage_key_with_base(word_position, base_slot),
                    bitmap,
                )
            })
            .collect();
        let injected = updates.len();
        self.apply_updates(&updates);
        Ok(injected)
    }

    fn inject_v3_ticks_with_base(
        &mut self,
        pool_address: Address,
        ticks: &HashMap<i32, TickInfo>,
        ticks_slot: U256,
    ) -> Result<usize> {
        let mut updates = Vec::with_capacity(ticks.len() * 2);
        for (&tick, info) in ticks {
            let keys = v3_tick_info_storage_keys_with_base(tick, ticks_slot);
            let packed_liquidity =
                U256::from(info.liquidity_gross) | (i128_to_u256(info.liquidity_net) << 128);
            let initialized = if info.initialized {
                U256::from(1u64) << 248
            } else {
                U256::ZERO
            };
            updates.push(StateUpdate::slot(pool_address, keys[0], packed_liquidity));
            updates.push(StateUpdate::slot(pool_address, keys[3], initialized));
        }

        let injected = ticks.len();
        self.apply_updates(&updates);
        Ok(injected)
    }

    fn tick_snapshot_cache(&self) -> TickSnapshotCacheReadGuard {
        TickSnapshotCacheReadGuard(
            tick_snapshots()
                .read()
                .expect("V3 tick snapshot cache lock poisoned"),
        )
    }

    fn tick_snapshot_cache_mut(&mut self) -> TickSnapshotCacheWriteGuard {
        TickSnapshotCacheWriteGuard(
            tick_snapshots()
                .write()
                .expect("V3 tick snapshot cache lock poisoned"),
        )
    }
}

fn i128_to_u256(value: i128) -> U256 {
    U256::from(value as u128)
}
