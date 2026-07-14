//! Generation-scoped ownership indexes for AMM runtime resources.
//!
//! The index distinguishes address association, whole-account coverage, exact
//! slot ownership, and event emission. That separation is required for shared
//! vaults: an account purge can affect every pool associated with the vault,
//! while an ordinary slot write belongs only to pools that declared that slot.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;

use alloy_primitives::{Address, U256};
use evm_fork_cache::reactive::{HandlerId, ResyncId};

use super::{
    AdapterGeneration, AdapterInstanceId, AdapterKey, AdapterRegistry, AmmPoolReactiveHandler,
    DiscoveryOwnerId, DiscoveryOwnerKey, PoolGeneration, PoolInstanceId, PoolKey, PoolRegistration,
    RuntimeOwnerId, RuntimeWorkId,
};

/// One exact EVM storage location.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct StateSlot {
    address: Address,
    slot: U256,
}

impl StateSlot {
    /// Construct an exact storage location.
    pub const fn new(address: Address, slot: U256) -> Self {
        Self { address, slot }
    }

    /// Storage-owning contract.
    pub const fn address(self) -> Address {
        self.address
    }

    /// Storage key.
    pub const fn slot(self) -> U256 {
        self.slot
    }
}

/// Complete state-ownership declaration for one pool.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PoolStateDependencies {
    associated_addresses: Vec<Address>,
    whole_accounts: Vec<Address>,
    slots: Vec<StateSlot>,
}

impl PoolStateDependencies {
    /// Add state-associated addresses.
    pub fn with_associated_addresses(
        mut self,
        addresses: impl IntoIterator<Item = Address>,
    ) -> Self {
        self.associated_addresses.extend(addresses);
        self.normalize();
        self
    }

    /// Add addresses whose arbitrary storage slots belong to this pool.
    pub fn with_whole_accounts(mut self, addresses: impl IntoIterator<Item = Address>) -> Self {
        self.whole_accounts.extend(addresses);
        self.normalize();
        self
    }

    /// Add exact owned storage slots.
    pub fn with_slots(mut self, slots: impl IntoIterator<Item = StateSlot>) -> Self {
        self.slots.extend(slots);
        self.normalize();
        self
    }

    /// Canonically ordered state-associated addresses.
    pub fn associated_addresses(&self) -> &[Address] {
        &self.associated_addresses
    }

    /// Canonically ordered whole-account storage owners.
    pub fn whole_accounts(&self) -> &[Address] {
        &self.whole_accounts
    }

    /// Canonically ordered exact storage owners.
    pub fn slots(&self) -> &[StateSlot] {
        &self.slots
    }

    fn normalize(&mut self) {
        self.whole_accounts.sort_unstable();
        self.whole_accounts.dedup();
        self.slots.sort_unstable();
        self.slots.dedup();
        self.associated_addresses
            .extend(self.whole_accounts.iter().copied());
        self.associated_addresses
            .extend(self.slots.iter().map(|slot| slot.address));
        self.associated_addresses.sort_unstable();
        self.associated_addresses.dedup();
    }
}

/// All reverse-indexable resources owned by one pool instance.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PoolOwnership {
    instance: PoolInstanceId,
    handler: HandlerId,
    adapter: AdapterInstanceId,
    dependencies: PoolStateDependencies,
    event_emitters: Vec<Address>,
}

impl PoolOwnership {
    /// Construct a canonical ownership record.
    pub fn new(
        instance: PoolInstanceId,
        adapter: AdapterInstanceId,
        dependencies: PoolStateDependencies,
        event_emitters: impl IntoIterator<Item = Address>,
    ) -> Result<Self, AmmOwnershipError> {
        if !adapter
            .key()
            .protocols()
            .contains(&instance.key().protocol())
        {
            return Err(AmmOwnershipError::AdapterProtocolMismatch {
                pool: instance,
                adapter,
            });
        }
        let mut event_emitters: Vec<_> = event_emitters.into_iter().collect();
        event_emitters.sort_unstable();
        event_emitters.dedup();
        Ok(Self {
            handler: AmmPoolReactiveHandler::handler_id(&instance),
            instance,
            adapter,
            dependencies,
            event_emitters,
        })
    }

    /// Pool registration instance.
    pub const fn instance(&self) -> &PoolInstanceId {
        &self.instance
    }

    /// Generation-scoped handler id.
    pub const fn handler(&self) -> &HandlerId {
        &self.handler
    }

    /// Adapter-family instance.
    pub const fn adapter(&self) -> &AdapterInstanceId {
        &self.adapter
    }

    /// Declared state dependencies.
    pub const fn dependencies(&self) -> &PoolStateDependencies {
        &self.dependencies
    }

    /// Canonically ordered event emitters.
    pub fn event_emitters(&self) -> &[Address] {
        &self.event_emitters
    }
}

/// Rejected ownership-index mutation.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum AmmOwnershipError {
    /// An adapter family already has an active generation.
    DuplicateAdapter(AdapterKey),
    /// A pool references an adapter instance not registered in the index.
    UnknownAdapter(AdapterInstanceId),
    /// A registered pool has no protocol adapter in the source registry.
    MissingAdapter(PoolKey),
    /// The pool protocol is outside the adapter family's declared protocols.
    AdapterProtocolMismatch {
        /// Rejected pool instance.
        pool: PoolInstanceId,
        /// Rejected adapter instance.
        adapter: AdapterInstanceId,
    },
    /// A logical pool key already has an active generation.
    DuplicatePool(PoolKey),
    /// A logical discovery-owner key already has an active generation.
    DuplicateDiscovery(DiscoveryOwnerKey),
    /// A concrete discovery-owner generation is already indexed.
    DuplicateDiscoveryInstance(DiscoveryOwnerId),
    /// A concrete pool instance is already indexed.
    DuplicatePoolInstance(PoolInstanceId),
    /// A generated handler id is already owned by another pool.
    DuplicateHandler(HandlerId),
    /// A work item is already indexed.
    DuplicateWork(RuntimeWorkId),
    /// A work owner is not active in this index.
    UnknownWorkOwner(RuntimeOwnerId),
    /// A resync request id is already owned.
    DuplicateResync(ResyncId),
    /// A same-generation replacement changed immutable handler/adapter identity.
    PoolReplacementIdentity(PoolInstanceId),
    /// A resync request references an unknown pool instance.
    UnknownPool(PoolInstanceId),
    /// Adapter removal was attempted while pools still depend on it.
    AdapterInUse(AdapterInstanceId),
}

impl fmt::Display for AmmOwnershipError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "AMM ownership mutation rejected: {self:?}")
    }
}

impl std::error::Error for AmmOwnershipError {}

/// Resources detached by one exact pool-instance removal.
#[derive(Clone, Debug)]
pub struct PoolOwnershipRemoval {
    ownership: PoolOwnership,
    work: Vec<RuntimeWorkId>,
    resyncs: Vec<ResyncId>,
}

/// Adapter-scoped ownership of one exact discovery-source generation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveryOwnership {
    owner: DiscoveryOwnerId,
    adapter: AdapterInstanceId,
}

impl DiscoveryOwnership {
    /// Bind one discovery-source generation to the adapter generation it serves.
    pub const fn new(owner: DiscoveryOwnerId, adapter: AdapterInstanceId) -> Self {
        Self { owner, adapter }
    }

    /// Exact discovery-source generation.
    pub const fn owner(&self) -> &DiscoveryOwnerId {
        &self.owner
    }

    /// Exact adapter generation targeted by this discovery source.
    pub const fn adapter(&self) -> &AdapterInstanceId {
        &self.adapter
    }
}

/// Resources detached by one exact discovery-owner removal.
#[derive(Clone, Debug)]
pub struct DiscoveryOwnershipRemoval {
    ownership: DiscoveryOwnership,
    work: Vec<RuntimeWorkId>,
}

impl DiscoveryOwnershipRemoval {
    /// Removed discovery ownership record.
    pub const fn ownership(&self) -> &DiscoveryOwnership {
        &self.ownership
    }

    /// Canonically ordered exact-owner work cancelled by removal.
    pub fn cancelled_work(&self) -> &[RuntimeWorkId] {
        &self.work
    }
}

impl PoolOwnershipRemoval {
    /// Removed pool ownership record.
    pub const fn ownership(&self) -> &PoolOwnership {
        &self.ownership
    }

    /// Canonically ordered work detached with the pool.
    pub fn work(&self) -> &[RuntimeWorkId] {
        &self.work
    }

    /// Deterministically ordered resync requests detached with the pool.
    pub fn resyncs(&self) -> &[ResyncId] {
        &self.resyncs
    }
}

/// Bidirectional generation-scoped AMM runtime ownership indexes.
#[derive(Clone, Debug, Default)]
pub struct AmmOwnershipIndex {
    adapters: BTreeSet<AdapterInstanceId>,
    active_adapters: BTreeMap<AdapterKey, AdapterInstanceId>,
    active_pools: BTreeMap<PoolKey, PoolInstanceId>,
    pools: BTreeMap<PoolInstanceId, PoolOwnership>,
    handler_pools: HashMap<HandlerId, PoolInstanceId>,
    adapter_pools: BTreeMap<AdapterInstanceId, BTreeSet<PoolInstanceId>>,
    active_discovery: BTreeMap<DiscoveryOwnerKey, DiscoveryOwnerId>,
    discovery: BTreeMap<DiscoveryOwnerId, DiscoveryOwnership>,
    adapter_discovery: BTreeMap<AdapterInstanceId, BTreeSet<DiscoveryOwnerId>>,
    address_pools: BTreeMap<Address, BTreeSet<PoolInstanceId>>,
    whole_account_pools: BTreeMap<Address, BTreeSet<PoolInstanceId>>,
    slot_pools: BTreeMap<StateSlot, BTreeSet<PoolInstanceId>>,
    emitter_pools: BTreeMap<Address, BTreeSet<PoolInstanceId>>,
    owner_work: BTreeMap<RuntimeOwnerId, BTreeSet<RuntimeWorkId>>,
    resync_owners: HashMap<ResyncId, PoolInstanceId>,
    pool_resyncs: BTreeMap<PoolInstanceId, Vec<ResyncId>>,
}

impl AmmOwnershipIndex {
    /// Build the synchronous compatibility index using generation zero.
    ///
    /// The actor lifecycle introduced later supplies retained generations to
    /// [`insert_adapter`](Self::insert_adapter) and
    /// [`insert_pool`](Self::insert_pool) instead.
    pub fn from_registry(registry: &AdapterRegistry) -> Result<Self, AmmOwnershipError> {
        let mut index = Self::default();
        let mut families: BTreeMap<AdapterKey, _> = BTreeMap::new();
        for adapter in registry.adapters() {
            let key = AdapterKey::new(adapter.protocol(), adapter.protocols());
            families.entry(key).or_insert_with(|| adapter.clone());
        }

        for key in families.keys() {
            index.insert_adapter(AdapterInstanceId::new(
                key.clone(),
                AdapterGeneration::new(0),
            ))?;
        }

        let mut pools: Vec<&PoolRegistration> = registry.pools().collect();
        pools.sort_by(|left, right| left.key.cmp(&right.key));
        for pool in pools {
            let adapter = registry
                .adapter(pool.protocol())
                .ok_or_else(|| AmmOwnershipError::MissingAdapter(pool.key.clone()))?;
            let adapter_key = AdapterKey::new(adapter.protocol(), adapter.protocols());
            let adapter_instance = index
                .active_adapters
                .get(&adapter_key)
                .cloned()
                .expect("adapter family inserted above");
            let ownership = PoolOwnership::new(
                PoolInstanceId::new(pool.key.clone(), PoolGeneration::new(0)),
                adapter_instance,
                adapter.state_dependencies(pool),
                registry
                    .event_sources_for(pool)
                    .into_iter()
                    .map(|source| source.emitter),
            )?;
            index.insert_pool(ownership)?;
        }
        Ok(index)
    }

    /// Register one active adapter-family generation.
    pub fn insert_adapter(&mut self, adapter: AdapterInstanceId) -> Result<(), AmmOwnershipError> {
        if self.active_adapters.contains_key(adapter.key()) {
            return Err(AmmOwnershipError::DuplicateAdapter(adapter.key().clone()));
        }
        self.active_adapters
            .insert(adapter.key().clone(), adapter.clone());
        self.adapters.insert(adapter);
        Ok(())
    }

    /// Insert one complete pool ownership record atomically.
    pub fn insert_pool(&mut self, ownership: PoolOwnership) -> Result<(), AmmOwnershipError> {
        let instance = ownership.instance.clone();
        if !self.adapters.contains(&ownership.adapter) {
            return Err(AmmOwnershipError::UnknownAdapter(ownership.adapter.clone()));
        }
        if !ownership
            .adapter
            .key()
            .protocols()
            .contains(&instance.key().protocol())
        {
            return Err(AmmOwnershipError::AdapterProtocolMismatch {
                pool: instance,
                adapter: ownership.adapter,
            });
        }
        if self.active_pools.contains_key(instance.key()) {
            return Err(AmmOwnershipError::DuplicatePool(instance.key().clone()));
        }
        if self.pools.contains_key(&instance) {
            return Err(AmmOwnershipError::DuplicatePoolInstance(instance));
        }
        if self.handler_pools.contains_key(&ownership.handler) {
            return Err(AmmOwnershipError::DuplicateHandler(
                ownership.handler.clone(),
            ));
        }

        self.active_pools
            .insert(instance.key().clone(), instance.clone());
        self.handler_pools
            .insert(ownership.handler.clone(), instance.clone());
        insert_owner(
            &mut self.adapter_pools,
            ownership.adapter.clone(),
            instance.clone(),
        );
        for address in ownership.dependencies.associated_addresses() {
            insert_owner(&mut self.address_pools, *address, instance.clone());
        }
        for address in ownership.dependencies.whole_accounts() {
            insert_owner(&mut self.whole_account_pools, *address, instance.clone());
        }
        for slot in ownership.dependencies.slots() {
            insert_owner(&mut self.slot_pools, *slot, instance.clone());
        }
        for emitter in &ownership.event_emitters {
            insert_owner(&mut self.emitter_pools, *emitter, instance.clone());
        }
        self.pools.insert(instance, ownership);
        Ok(())
    }

    /// Replace one exact active pool's indexable resources without detaching its
    /// generation-owned work or resync requests.
    pub(crate) fn replace_pool(
        &mut self,
        ownership: PoolOwnership,
    ) -> Result<PoolOwnership, AmmOwnershipError> {
        let instance = ownership.instance().clone();
        if self.active_pool(instance.key()) != Some(&instance) {
            return Err(AmmOwnershipError::UnknownPool(instance));
        }
        let previous = self
            .pools
            .get(&instance)
            .cloned()
            .ok_or_else(|| AmmOwnershipError::UnknownPool(instance.clone()))?;
        if previous.handler != ownership.handler
            || previous.adapter != ownership.adapter
            || self.handler_pools.get(&previous.handler) != Some(&instance)
        {
            return Err(AmmOwnershipError::PoolReplacementIdentity(instance));
        }

        for address in previous.dependencies.associated_addresses() {
            remove_owner(&mut self.address_pools, address, &instance);
        }
        for address in previous.dependencies.whole_accounts() {
            remove_owner(&mut self.whole_account_pools, address, &instance);
        }
        for slot in previous.dependencies.slots() {
            remove_owner(&mut self.slot_pools, slot, &instance);
        }
        for emitter in &previous.event_emitters {
            remove_owner(&mut self.emitter_pools, emitter, &instance);
        }
        for address in ownership.dependencies.associated_addresses() {
            insert_owner(&mut self.address_pools, *address, instance.clone());
        }
        for address in ownership.dependencies.whole_accounts() {
            insert_owner(&mut self.whole_account_pools, *address, instance.clone());
        }
        for slot in ownership.dependencies.slots() {
            insert_owner(&mut self.slot_pools, *slot, instance.clone());
        }
        for emitter in &ownership.event_emitters {
            insert_owner(&mut self.emitter_pools, *emitter, instance.clone());
        }
        self.pools.insert(instance, ownership);
        Ok(previous)
    }

    /// Insert one exact discovery-owner generation atomically.
    pub fn insert_discovery(
        &mut self,
        ownership: DiscoveryOwnership,
    ) -> Result<(), AmmOwnershipError> {
        if !self.adapters.contains(&ownership.adapter) {
            return Err(AmmOwnershipError::UnknownAdapter(ownership.adapter));
        }
        if self.active_discovery.contains_key(ownership.owner.key()) {
            return Err(AmmOwnershipError::DuplicateDiscovery(
                ownership.owner.key().clone(),
            ));
        }
        if self.discovery.contains_key(&ownership.owner) {
            return Err(AmmOwnershipError::DuplicateDiscoveryInstance(
                ownership.owner,
            ));
        }
        self.active_discovery
            .insert(ownership.owner.key().clone(), ownership.owner.clone());
        self.adapter_discovery
            .entry(ownership.adapter.clone())
            .or_default()
            .insert(ownership.owner.clone());
        self.discovery.insert(ownership.owner.clone(), ownership);
        Ok(())
    }

    /// Current generation for a logical pool key.
    pub fn active_pool(&self, pool: &PoolKey) -> Option<&PoolInstanceId> {
        self.active_pools.get(pool)
    }

    /// Current generation for a logical adapter-family key.
    pub fn active_adapter(&self, adapter: &AdapterKey) -> Option<&AdapterInstanceId> {
        self.active_adapters.get(adapter)
    }

    /// Current generation for a logical discovery-owner key.
    pub fn active_discovery(&self, owner: &DiscoveryOwnerKey) -> Option<&DiscoveryOwnerId> {
        self.active_discovery.get(owner)
    }

    /// Canonically ordered active adapter-family instances, including families
    /// that currently own no pools.
    pub fn adapters(&self) -> impl Iterator<Item = &AdapterInstanceId> {
        self.active_adapters.values()
    }

    /// Ownership record for a concrete pool instance.
    pub fn pool(&self, pool: &PoolInstanceId) -> Option<&PoolOwnership> {
        self.pools.get(pool)
    }

    /// Pool instance owning a handler id.
    pub fn pool_for_handler(&self, handler: &HandlerId) -> Option<&PoolInstanceId> {
        self.handler_pools.get(handler)
    }

    /// Adapter instance owning a pool.
    pub fn adapter_for_pool(&self, pool: &PoolInstanceId) -> Option<&AdapterInstanceId> {
        self.pools.get(pool).map(|ownership| &ownership.adapter)
    }

    /// Adapter generation targeted by one exact discovery-owner generation.
    pub fn adapter_for_discovery(&self, owner: &DiscoveryOwnerId) -> Option<&AdapterInstanceId> {
        self.discovery.get(owner).map(DiscoveryOwnership::adapter)
    }

    /// Canonically ordered pools depending on an adapter instance.
    pub fn pools_for_adapter(&self, adapter: &AdapterInstanceId) -> Vec<PoolInstanceId> {
        owners(&self.adapter_pools, adapter)
    }

    /// Canonically ordered discovery owners targeting an adapter generation.
    pub fn discovery_for_adapter(&self, adapter: &AdapterInstanceId) -> Vec<DiscoveryOwnerId> {
        owners(&self.adapter_discovery, adapter)
    }

    /// Canonically ordered active discovery-owner generations.
    pub fn discovery_owners(&self) -> impl Iterator<Item = &DiscoveryOwnerId> {
        self.discovery.keys()
    }

    /// Canonically ordered pools associated with an address.
    pub fn pools_for_address(&self, address: Address) -> Vec<PoolInstanceId> {
        owners(&self.address_pools, &address)
    }

    /// Canonically ordered pools covering one exact slot.
    pub fn pools_for_slot(&self, slot: StateSlot) -> Vec<PoolInstanceId> {
        let mut pools = self.slot_pools.get(&slot).cloned().unwrap_or_default();
        if let Some(whole) = self.whole_account_pools.get(&slot.address) {
            pools.extend(whole.iter().cloned());
        }
        pools.into_iter().collect()
    }

    /// Canonically ordered pools consuming logs from an emitter.
    pub fn pools_for_emitter(&self, emitter: Address) -> Vec<PoolInstanceId> {
        owners(&self.emitter_pools, &emitter)
    }

    /// Canonically ordered active pool instances.
    pub fn pools(&self) -> impl Iterator<Item = &PoolInstanceId> {
        self.pools.keys()
    }

    /// Track one generation-scoped runtime work attempt.
    pub fn track_work(&mut self, work: RuntimeWorkId) -> Result<(), AmmOwnershipError> {
        self.validate_owner(work.owner())?;
        let entries = self.owner_work.entry(work.owner().clone()).or_default();
        if !entries.insert(work.clone()) {
            return Err(AmmOwnershipError::DuplicateWork(work));
        }
        Ok(())
    }

    /// Stop tracking one runtime work attempt.
    pub fn untrack_work(&mut self, work: &RuntimeWorkId) -> bool {
        let Some(entries) = self.owner_work.get_mut(work.owner()) else {
            return false;
        };
        let removed = entries.remove(work);
        if entries.is_empty() {
            self.owner_work.remove(work.owner());
        }
        removed
    }

    /// Canonically ordered work owned by one runtime owner.
    pub fn work_for_owner(&self, owner: &RuntimeOwnerId) -> Vec<RuntimeWorkId> {
        owners(&self.owner_work, owner)
    }

    /// Associate an upstream resync request with its exact pool generation.
    pub fn track_resync(
        &mut self,
        pool: PoolInstanceId,
        resync: ResyncId,
    ) -> Result<(), AmmOwnershipError> {
        if !self.pools.contains_key(&pool) {
            return Err(AmmOwnershipError::UnknownPool(pool));
        }
        if self.resync_owners.contains_key(&resync) {
            return Err(AmmOwnershipError::DuplicateResync(resync));
        }
        self.resync_owners.insert(resync.clone(), pool.clone());
        self.pool_resyncs.entry(pool).or_default().push(resync);
        Ok(())
    }

    /// Pool generation owning an upstream resync request.
    pub fn resync_owner(&self, resync: &ResyncId) -> Option<&PoolInstanceId> {
        self.resync_owners.get(resync)
    }

    /// Stop tracking a completed, cancelled, or superseded resync request.
    ///
    /// Returns the exact pool generation that owned the request. Removing one
    /// request never disturbs other requests owned by that pool or by pools
    /// sharing the same state address.
    pub fn untrack_resync(&mut self, resync: &ResyncId) -> Option<PoolInstanceId> {
        let pool = self.resync_owners.remove(resync)?;
        let remove_pool_entry = if let Some(resyncs) = self.pool_resyncs.get_mut(&pool) {
            resyncs.retain(|tracked| tracked != resync);
            resyncs.is_empty()
        } else {
            false
        };
        if remove_pool_entry {
            self.pool_resyncs.remove(&pool);
        }
        Some(pool)
    }

    /// Deterministically ordered resync requests owned by one pool.
    pub fn resyncs_for_pool(&self, pool: &PoolInstanceId) -> Vec<ResyncId> {
        let mut ids = self.pool_resyncs.get(pool).cloned().unwrap_or_default();
        sort_resyncs(&mut ids);
        ids
    }

    /// Remove one exact pool generation and all of its reverse ownership.
    pub fn remove_pool(&mut self, pool: &PoolInstanceId) -> Option<PoolOwnershipRemoval> {
        let ownership = self.pools.remove(pool)?;
        if self.active_pools.get(pool.key()) == Some(pool) {
            self.active_pools.remove(pool.key());
        }
        self.handler_pools.remove(&ownership.handler);
        remove_owner(&mut self.adapter_pools, &ownership.adapter, pool);
        for address in ownership.dependencies.associated_addresses() {
            remove_owner(&mut self.address_pools, address, pool);
        }
        for address in ownership.dependencies.whole_accounts() {
            remove_owner(&mut self.whole_account_pools, address, pool);
        }
        for slot in ownership.dependencies.slots() {
            remove_owner(&mut self.slot_pools, slot, pool);
        }
        for emitter in &ownership.event_emitters {
            remove_owner(&mut self.emitter_pools, emitter, pool);
        }

        let owner = RuntimeOwnerId::Pool(pool.clone());
        let work = self.owner_work.remove(&owner).unwrap_or_default();
        let mut resyncs = self.pool_resyncs.remove(pool).unwrap_or_default();
        for resync in &resyncs {
            self.resync_owners.remove(resync);
        }
        sort_resyncs(&mut resyncs);
        Some(PoolOwnershipRemoval {
            ownership,
            work: work.into_iter().collect(),
            resyncs,
        })
    }

    /// Remove one exact discovery-owner generation and only its work.
    pub fn remove_discovery(
        &mut self,
        owner: &DiscoveryOwnerId,
    ) -> Option<DiscoveryOwnershipRemoval> {
        let ownership = self.discovery.remove(owner)?;
        if self.active_discovery.get(owner.key()) == Some(owner) {
            self.active_discovery.remove(owner.key());
        }
        remove_discovery_owner(&mut self.adapter_discovery, ownership.adapter(), owner);
        let runtime_owner = RuntimeOwnerId::Discovery(owner.clone());
        let work = self.owner_work.remove(&runtime_owner).unwrap_or_default();
        Some(DiscoveryOwnershipRemoval {
            ownership,
            work: work.into_iter().collect(),
        })
    }

    /// Remove an unused exact adapter generation.
    pub fn remove_adapter(
        &mut self,
        adapter: &AdapterInstanceId,
    ) -> Result<bool, AmmOwnershipError> {
        if self
            .adapter_pools
            .get(adapter)
            .is_some_and(|pools| !pools.is_empty())
            || self
                .adapter_discovery
                .get(adapter)
                .is_some_and(|owners| !owners.is_empty())
        {
            return Err(AmmOwnershipError::AdapterInUse(adapter.clone()));
        }
        Ok(self.remove_adapter_prevalidated(adapter))
    }

    /// Detach an adapter generation after dependency ownership was preflighted.
    pub(crate) fn remove_adapter_prevalidated(&mut self, adapter: &AdapterInstanceId) -> bool {
        if !self.adapters.remove(adapter) {
            return false;
        }
        if self.active_adapters.get(adapter.key()) == Some(adapter) {
            self.active_adapters.remove(adapter.key());
        }
        self.owner_work
            .remove(&RuntimeOwnerId::Adapter(adapter.clone()));
        true
    }

    fn validate_owner(&self, owner: &RuntimeOwnerId) -> Result<(), AmmOwnershipError> {
        match owner {
            RuntimeOwnerId::Pool(pool) if !self.pools.contains_key(pool) => {
                Err(AmmOwnershipError::UnknownWorkOwner(owner.clone()))
            }
            RuntimeOwnerId::Adapter(adapter) if !self.adapters.contains(adapter) => {
                Err(AmmOwnershipError::UnknownWorkOwner(owner.clone()))
            }
            RuntimeOwnerId::Discovery(discovery) if !self.discovery.contains_key(discovery) => {
                Err(AmmOwnershipError::UnknownWorkOwner(owner.clone()))
            }
            RuntimeOwnerId::Pool(_) | RuntimeOwnerId::Adapter(_) | RuntimeOwnerId::Discovery(_) => {
                Ok(())
            }
        }
    }
}

fn remove_discovery_owner(
    index: &mut BTreeMap<AdapterInstanceId, BTreeSet<DiscoveryOwnerId>>,
    adapter: &AdapterInstanceId,
    owner: &DiscoveryOwnerId,
) {
    let remove_adapter = if let Some(owners) = index.get_mut(adapter) {
        owners.remove(owner);
        owners.is_empty()
    } else {
        false
    };
    if remove_adapter {
        index.remove(adapter);
    }
}

fn insert_owner<K: Ord>(
    index: &mut BTreeMap<K, BTreeSet<PoolInstanceId>>,
    key: K,
    pool: PoolInstanceId,
) {
    index.entry(key).or_default().insert(pool);
}

fn remove_owner<K: Ord>(
    index: &mut BTreeMap<K, BTreeSet<PoolInstanceId>>,
    key: &K,
    pool: &PoolInstanceId,
) {
    let Some(owners) = index.get_mut(key) else {
        return;
    };
    owners.remove(pool);
    if owners.is_empty() {
        index.remove(key);
    }
}

fn owners<K: Ord, V: Ord + Clone>(index: &BTreeMap<K, BTreeSet<V>>, key: &K) -> Vec<V> {
    index
        .get(key)
        .map(|owners| owners.iter().cloned().collect())
        .unwrap_or_default()
}

fn sort_resyncs(resyncs: &mut [ResyncId]) {
    resyncs.sort_by_cached_key(|id| format!("{id:?}"));
}
