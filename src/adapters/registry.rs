use std::collections::HashMap;
use std::fmt;

use alloy_primitives::{Address, B256, Log};

use super::{EventRoute, EventSource, PoolKey, PoolRegistration};

/// Registry of tracked AMM pools and their event-routing metadata.
#[derive(Clone, Debug, Default)]
pub struct AdapterRegistry {
    pools: HashMap<PoolKey, PoolRegistration>,
}

impl AdapterRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register_pool(&mut self, registration: PoolRegistration) -> Result<(), RegistryError> {
        if self.pools.contains_key(&registration.key) {
            return Err(RegistryError::DuplicatePool(registration.key));
        }

        self.pools.insert(registration.key.clone(), registration);
        Ok(())
    }

    pub fn pool(&self, key: &PoolKey) -> Option<&PoolRegistration> {
        self.pools.get(key)
    }

    pub fn pools(&self) -> impl Iterator<Item = &PoolRegistration> {
        self.pools.values()
    }

    pub fn route_log(&self, log: &Log) -> Option<&PoolRegistration> {
        let topics = log.topics();
        let topic0 = *topics.first()?;

        self.pools.values().find(|registration| {
            registration.event_sources.iter().any(|source| {
                source_matches_filter(source, log.address, topic0)
                    && source_routes_pool(source, &registration.key, topics)
            })
        })
    }

    pub fn subscription_topics(&self) -> Vec<B256> {
        let mut topics: Vec<B256> = self
            .pools
            .values()
            .flat_map(|pool| pool.event_sources.iter())
            .flat_map(|source| source.topics.iter().copied())
            .collect();

        topics.sort_unstable_by(|a, b| a.as_slice().cmp(b.as_slice()));
        topics.dedup();
        topics
    }

    pub fn len(&self) -> usize {
        self.pools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pools.is_empty()
    }
}

fn source_matches_filter(source: &EventSource, emitter: Address, topic0: B256) -> bool {
    source.emitter == emitter && (source.topics.is_empty() || source.topics.contains(&topic0))
}

fn source_routes_pool(source: &EventSource, key: &PoolKey, topics: &[B256]) -> bool {
    match source.route {
        EventRoute::Direct => true,
        EventRoute::IndexedAddress { topic_index } => topics
            .get(topic_index)
            .map(topic_address)
            .is_some_and(|address| key.address() == Some(address)),
        EventRoute::IndexedBytes32 { topic_index } => topics
            .get(topic_index)
            .is_some_and(|topic| key.bytes32() == Some(*topic)),
        EventRoute::AdapterDefined => false,
    }
}

fn topic_address(topic: &B256) -> Address {
    Address::from_slice(&topic.as_slice()[12..])
}

/// Errors raised while mutating an [`AdapterRegistry`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RegistryError {
    DuplicatePool(PoolKey),
}

impl fmt::Display for RegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicatePool(key) => write!(f, "pool is already registered: {key:?}"),
        }
    }
}

impl std::error::Error for RegistryError {}
