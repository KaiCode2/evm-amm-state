use std::any::Any;
use std::fmt;
use std::sync::Arc;

use alloy_primitives::{Address, B256, U256};

use super::cache::{SlotChange, StateDiff, StateUpdate};
use super::storage::{SolidlyStorageLayout, V3StorageLayout};

/// Protocol family identifier for adapter registrations.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ProtocolId {
    UniswapV2,
    UniswapV3,
    PancakeV3,
    Slipstream,
    SolidlyV2,
    BalancerV2,
    BalancerV3,
    Curve,
    Erc4626,
    UniswapV4,
    Custom(&'static str),
}

/// Protocol-specific pool identity.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum PoolKey {
    UniswapV2(Address),
    UniswapV3(Address),
    PancakeV3(Address),
    Slipstream(Address),
    SolidlyV2(Address),
    BalancerV2(B256),
    BalancerV3(Address),
    Curve(Address),
    Erc4626(Address),
    UniswapV4(B256),
    Custom(CustomPoolKey),
}

impl PoolKey {
    /// Return the protocol family for this pool key.
    pub fn protocol(&self) -> ProtocolId {
        match self {
            Self::UniswapV2(_) => ProtocolId::UniswapV2,
            Self::UniswapV3(_) => ProtocolId::UniswapV3,
            Self::PancakeV3(_) => ProtocolId::PancakeV3,
            Self::Slipstream(_) => ProtocolId::Slipstream,
            Self::SolidlyV2(_) => ProtocolId::SolidlyV2,
            Self::BalancerV2(_) => ProtocolId::BalancerV2,
            Self::BalancerV3(_) => ProtocolId::BalancerV3,
            Self::Curve(_) => ProtocolId::Curve,
            Self::Erc4626(_) => ProtocolId::Erc4626,
            Self::UniswapV4(_) => ProtocolId::UniswapV4,
            Self::Custom(key) => key.protocol(),
        }
    }

    /// Return the address identity for address-keyed pools.
    pub fn address(&self) -> Option<Address> {
        match self {
            Self::UniswapV2(address)
            | Self::UniswapV3(address)
            | Self::PancakeV3(address)
            | Self::Slipstream(address)
            | Self::SolidlyV2(address)
            | Self::BalancerV3(address)
            | Self::Curve(address)
            | Self::Erc4626(address) => Some(*address),
            Self::Custom(key) => key.address(),
            Self::BalancerV2(_) | Self::UniswapV4(_) => None,
        }
    }

    /// Return the bytes32 identity for bytes32-keyed pools.
    pub fn bytes32(&self) -> Option<B256> {
        match self {
            Self::BalancerV2(id) | Self::UniswapV4(id) => Some(*id),
            Self::Custom(key) => key.bytes32(),
            Self::UniswapV2(_)
            | Self::UniswapV3(_)
            | Self::PancakeV3(_)
            | Self::Slipstream(_)
            | Self::SolidlyV2(_)
            | Self::BalancerV3(_)
            | Self::Curve(_)
            | Self::Erc4626(_) => None,
        }
    }
}

/// Extension point for protocol-specific pool key shapes.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum CustomPoolKey {
    Address {
        protocol: &'static str,
        address: Address,
    },
    Bytes32 {
        protocol: &'static str,
        id: B256,
    },
    Composite {
        protocol: &'static str,
        address: Address,
        id: B256,
    },
}

impl CustomPoolKey {
    pub fn protocol(&self) -> ProtocolId {
        match self {
            Self::Address { protocol, .. }
            | Self::Bytes32 { protocol, .. }
            | Self::Composite { protocol, .. } => ProtocolId::Custom(protocol),
        }
    }

    pub fn address(&self) -> Option<Address> {
        match self {
            Self::Address { address, .. } | Self::Composite { address, .. } => Some(*address),
            Self::Bytes32 { .. } => None,
        }
    }

    pub fn bytes32(&self) -> Option<B256> {
        match self {
            Self::Bytes32 { id, .. } | Self::Composite { id, .. } => Some(*id),
            Self::Address { .. } => None,
        }
    }
}

/// One log emitter and routing rule for a tracked pool.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EventSource {
    pub emitter: Address,
    pub topics: Vec<B256>,
    pub route: EventRoute,
}

impl EventSource {
    pub fn direct(emitter: Address, topics: Vec<B256>) -> Self {
        Self {
            emitter,
            topics,
            route: EventRoute::Direct,
        }
    }

    pub fn indexed_address(emitter: Address, topics: Vec<B256>, topic_index: usize) -> Self {
        Self {
            emitter,
            topics,
            route: EventRoute::IndexedAddress { topic_index },
        }
    }

    pub fn indexed_bytes32(emitter: Address, topics: Vec<B256>, topic_index: usize) -> Self {
        Self {
            emitter,
            topics,
            route: EventRoute::IndexedBytes32 { topic_index },
        }
    }

    pub fn adapter_defined(emitter: Address, topics: Vec<B256>) -> Self {
        Self {
            emitter,
            topics,
            route: EventRoute::AdapterDefined,
        }
    }
}

/// Generic routing rule for a log emitted by an [`EventSource`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EventRoute {
    Direct,
    IndexedAddress { topic_index: usize },
    IndexedBytes32 { topic_index: usize },
    AdapterDefined,
}

/// Per-pool sidecar registration owned by `evm-amm-state`.
#[derive(Clone, Debug)]
pub struct PoolRegistration {
    pub key: PoolKey,
    pub state_addresses: Vec<Address>,
    pub event_sources: Vec<EventSource>,
    pub metadata: ProtocolMetadata,
    pub status: PoolStatus,
}

impl PoolRegistration {
    pub fn new(key: PoolKey) -> Self {
        Self {
            key,
            state_addresses: Vec::new(),
            event_sources: Vec::new(),
            metadata: ProtocolMetadata::Unknown,
            status: PoolStatus::Pending,
        }
    }

    pub fn protocol(&self) -> ProtocolId {
        self.key.protocol()
    }

    pub fn with_state_address(mut self, address: Address) -> Self {
        self.state_addresses.push(address);
        self
    }

    pub fn with_state_addresses(mut self, addresses: impl IntoIterator<Item = Address>) -> Self {
        self.state_addresses.extend(addresses);
        self
    }

    pub fn with_event_source(mut self, source: EventSource) -> Self {
        self.event_sources.push(source);
        self
    }

    pub fn with_event_sources(mut self, sources: impl IntoIterator<Item = EventSource>) -> Self {
        self.event_sources.extend(sources);
        self
    }

    pub fn with_metadata(mut self, metadata: ProtocolMetadata) -> Self {
        self.metadata = metadata;
        self
    }

    pub fn with_status(mut self, status: PoolStatus) -> Self {
        self.status = status;
        self
    }
}

/// Protocol metadata known for a tracked pool.
#[derive(Clone, Default)]
pub enum ProtocolMetadata {
    #[default]
    Unknown,
    UniswapV2(UniswapV2Metadata),
    UniswapV3(V3Metadata),
    PancakeV3(V3Metadata),
    Slipstream(V3Metadata),
    BalancerV2(BalancerV2Metadata),
    SolidlyV2(SolidlyV2Metadata),
    Curve(CurveMetadata),
    Custom(Arc<dyn Any + Send + Sync>),
}

impl fmt::Debug for ProtocolMetadata {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unknown => f.write_str("Unknown"),
            Self::UniswapV2(metadata) => f.debug_tuple("UniswapV2").field(metadata).finish(),
            Self::UniswapV3(metadata) => f.debug_tuple("UniswapV3").field(metadata).finish(),
            Self::PancakeV3(metadata) => f.debug_tuple("PancakeV3").field(metadata).finish(),
            Self::Slipstream(metadata) => f.debug_tuple("Slipstream").field(metadata).finish(),
            Self::BalancerV2(metadata) => f.debug_tuple("BalancerV2").field(metadata).finish(),
            Self::SolidlyV2(metadata) => f.debug_tuple("SolidlyV2").field(metadata).finish(),
            Self::Curve(metadata) => f.debug_tuple("Curve").field(metadata).finish(),
            Self::Custom(_) => f.write_str("Custom(..)"),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UniswapV2Metadata {
    pub token0: Option<Address>,
    pub token1: Option<Address>,
    pub fee_bps: Option<u32>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct V3Metadata {
    pub token0: Option<Address>,
    pub token1: Option<Address>,
    pub fee: Option<u32>,
    pub tick_spacing: Option<i32>,
    pub storage_layout: Option<V3StorageLayout>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SolidlyV2Metadata {
    pub token0: Option<Address>,
    pub token1: Option<Address>,
    /// `true` for stable (x³y+y³x) pools, `false` for volatile (xy=k). Config-
    /// supplied; preserved across cold-start.
    pub stable: Option<bool>,
    pub storage_layout: Option<SolidlyStorageLayout>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BalancerV2Metadata {
    pub vault: Option<Address>,
    pub pool_address: Option<Address>,
    pub tokens: Vec<Address>,
    /// Vault balance storage slots discovered during cold-start (the `(vault,
    /// slot)` pairs the `getPoolTokens` view-call SLOADed; recorded slot-only
    /// since they all live on `vault`).
    ///
    /// Persisting them here lets the reactive `Swap` path refresh (re-verify)
    /// exactly these slots — keeping the cached vault balances fresh for a
    /// subsequent `simulate_swap` — without reverse-engineering the vault's
    /// balance-mapping layout or doing lossy event-delta arithmetic. Empty
    /// until the discover→verify cold-start runs.
    pub balance_slots: Vec<U256>,
}

/// Which Curve pool dialect a pool speaks — selects the `get_dy` / `TokenExchange`
/// index ABI (the slice-1 vs slice-2 axis).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CurveVariant {
    /// Classic StableSwap **and** StableSwap-NG: `get_dy(int128,int128,uint256)`
    /// and `TokenExchange(address,int128,uint256,int128,uint256)`.
    #[default]
    StableSwap,
    /// CryptoSwap (Curve v2, e.g. tricrypto): `get_dy(uint256,uint256,uint256)`
    /// and `TokenExchange(address,uint256,uint256,uint256,uint256)`.
    CryptoSwap,
}

/// Metadata for a Curve plain pool.
///
/// `coins` is config-supplied (the pool's static coin ordering); it drives the
/// `simulate_swap` token→index mapping for `get_dy`. `discovered_slots` is the
/// storage read-set the cold-start discover pass captured from a `get_dy` call
/// (balances + amplification + fee, wherever the Vyper build placed them) — a
/// real Curve pool has no predictable balance-slot layout, so discovery, not a
/// hand-coded layout, identifies them. Persisting them lets the reactive
/// `TokenExchange`/liquidity path re-verify exactly those slots (a resync),
/// keeping cached state fresh for a later `simulate_swap`. Slot-only; all live
/// on the pool address. Empty until cold-start runs.
///
/// `variant` selects the index ABI (`StableSwap`/NG use `int128`; `CryptoSwap`
/// uses `uint256`). Defaults to `StableSwap` (slice-1 + NG behavior).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CurveMetadata {
    pub coins: Vec<Address>,
    pub discovered_slots: Vec<U256>,
    pub variant: CurveVariant,
}

/// Lifecycle status for a tracked pool registration.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum PoolStatus {
    #[default]
    Pending,
    Cold,
    Ready,
    Degraded,
    Disabled,
    Unsupported,
}

/// Adapter-derived semantic event and cache mutations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdapterEvent {
    pub pool: PoolKey,
    pub emitter: Address,
    pub topic0: B256,
    pub kind: AdapterEventKind,
    pub updates: Vec<StateUpdate>,
    pub quality: UpdateQuality,
    pub repair: RepairAction,
}

/// Structured result of routing, decoding, and applying one adapter event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdapterEventReport {
    pub pool: PoolKey,
    pub event: AdapterEvent,
    pub applied: StateDiff,
    pub repair: RepairAction,
}

/// High-level AMM event class.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AdapterEventKind {
    Swap,
    LiquidityAdded,
    LiquidityRemoved,
    Sync,
    Deposit,
    Withdraw,
    Unknown,
}

/// Result of protocol adapter log decoding.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AdapterEventResult {
    pub event: Option<AdapterEvent>,
    pub error: Option<AdapterEventError>,
}

impl AdapterEventResult {
    pub fn event(event: AdapterEvent) -> Self {
        Self {
            event: Some(event),
            error: None,
        }
    }

    pub fn ignored() -> Self {
        Self::default()
    }

    pub fn error(error: AdapterEventError) -> Self {
        Self {
            event: None,
            error: Some(error),
        }
    }
}

/// Decode-time adapter error vocabulary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AdapterEventError {
    MalformedLog(&'static str),
    MissingState { address: Address, slot: U256 },
    Unsupported(UnsupportedReason),
    Custom(String),
}

/// Quality of the cache update emitted for an adapter event.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum UpdateQuality {
    Exact,
    ExactIfApplied,
    RequiresRepair,
    ConservativeInvalidation,
    Ignored,
}

/// Adapter-level follow-up work after cold-start or event application.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum RepairAction {
    #[default]
    None,
    VerifySlots(Vec<(Address, U256)>),
    PurgeStorage(Address),
    PurgeSlots {
        address: Address,
        slots: Vec<U256>,
    },
    ColdStart {
        pool: PoolKey,
        policy: ColdStartPolicy,
    },
    V3TickRange {
        pool: PoolKey,
        tick_lower: i32,
        tick_upper: i32,
    },
    V3Incremental {
        pool: PoolKey,
    },
    V3Full {
        pool: PoolKey,
    },
}

impl RepairAction {
    /// Merge two repair intentions into one, preferring `other` on conflict.
    ///
    /// `None` is absorbing (`x.combine(None) == x`, `None.combine(x) == x`),
    /// matching same-shape variants are unioned (`VerifySlots` by slot,
    /// same-address `PurgeSlots` by slot), and any other pairing falls through
    /// to `other`.
    pub(crate) fn combine(self, other: RepairAction) -> RepairAction {
        match (self, other) {
            (RepairAction::None, repair) | (repair, RepairAction::None) => repair,
            (RepairAction::VerifySlots(mut left), RepairAction::VerifySlots(right)) => {
                for slot in right {
                    if !left.contains(&slot) {
                        left.push(slot);
                    }
                }
                RepairAction::VerifySlots(left)
            }
            (
                RepairAction::PurgeSlots {
                    address: left_address,
                    slots: mut left_slots,
                },
                RepairAction::PurgeSlots {
                    address: right_address,
                    slots: right_slots,
                },
            ) if left_address == right_address => {
                for slot in right_slots {
                    if !left_slots.contains(&slot) {
                        left_slots.push(slot);
                    }
                }
                RepairAction::PurgeSlots {
                    address: left_address,
                    slots: left_slots,
                }
            }
            (_, other) => other,
        }
    }
}

/// Cold-start strictness and cost policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ColdStartPolicy {
    Strict,
    Eager,
    Lazy,
    HotSlotsOnly,
}

/// Result of attempting to cold-start a tracked pool.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ColdStartOutcome {
    Ready(ColdStartReport),
    ReadyWithDeferred(ColdStartReport, Vec<DeferredWork>),
    NeedsRepair(ColdStartReport, RepairAction),
    Unsupported(UnsupportedReason),
}

/// Inspectable summary of cold-start work performed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColdStartReport {
    pub pool: PoolKey,
    pub policy: ColdStartPolicy,
    pub status: PoolStatus,
    pub verified_slots: Vec<(Address, U256)>,
    pub changed_slots: Vec<SlotChange>,
    pub applied: StateDiff,
    pub deferred: Vec<DeferredWork>,
}

impl ColdStartReport {
    pub fn new(pool: PoolKey, policy: ColdStartPolicy) -> Self {
        Self {
            pool,
            policy,
            status: PoolStatus::Pending,
            verified_slots: Vec::new(),
            changed_slots: Vec::new(),
            applied: StateDiff::default(),
            deferred: Vec::new(),
        }
    }
}

/// Deferred adapter work that can be scheduled after cold-start.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeferredWork {
    VerifySlots(Vec<(Address, U256)>),
    Repair(RepairAction),
    ColdStart {
        pool: PoolKey,
        policy: ColdStartPolicy,
    },
    Custom(String),
}

/// Result of running deferred cold-start work via
/// [`AdapterRegistry::run_deferred`](super::AdapterRegistry::run_deferred).
///
/// `verified` accumulates the [`SlotChange`]s produced by warming
/// [`DeferredWork::VerifySlots`] (and `Repair(VerifySlots)`) entries.
/// `unhandled` collects, verbatim, any deferred work the driver does not execute
/// in this item (`ColdStart`, `Custom`, and non-`VerifySlots` repairs) so callers
/// can route them onward rather than have them silently dropped.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeferredOutcome {
    pub verified: Vec<SlotChange>,
    pub unhandled: Vec<DeferredWork>,
}

impl DeferredOutcome {
    /// Whether every deferred item was executed (nothing was deferred onward).
    pub fn is_fully_handled(&self) -> bool {
        self.unhandled.is_empty()
    }
}

/// Why a protocol state, event, or policy is not supported by the current adapter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UnsupportedReason {
    Protocol(ProtocolId),
    MissingMetadata(&'static str),
    AdapterDefinedRouting,
    Custom(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(byte: u8) -> Address {
        Address::repeat_byte(byte)
    }

    #[test]
    fn combine_none_is_absorbing() {
        let verify = RepairAction::VerifySlots(vec![(addr(0x11), U256::from(1))]);
        assert_eq!(RepairAction::None.combine(verify.clone()), verify);
        assert_eq!(verify.clone().combine(RepairAction::None), verify);
        assert_eq!(
            RepairAction::None.combine(RepairAction::None),
            RepairAction::None
        );
    }

    #[test]
    fn combine_verify_slots_unions_and_dedupes() {
        let a = addr(0x11);
        let left = RepairAction::VerifySlots(vec![(a, U256::from(1)), (a, U256::from(2))]);
        let right = RepairAction::VerifySlots(vec![(a, U256::from(2)), (a, U256::from(3))]);
        assert_eq!(
            left.combine(right),
            RepairAction::VerifySlots(vec![
                (a, U256::from(1)),
                (a, U256::from(2)),
                (a, U256::from(3)),
            ])
        );
    }

    #[test]
    fn combine_purge_slots_same_address_unions() {
        let a = addr(0x22);
        let left = RepairAction::PurgeSlots {
            address: a,
            slots: vec![U256::from(1), U256::from(2)],
        };
        let right = RepairAction::PurgeSlots {
            address: a,
            slots: vec![U256::from(2), U256::from(3)],
        };
        assert_eq!(
            left.combine(right),
            RepairAction::PurgeSlots {
                address: a,
                slots: vec![U256::from(1), U256::from(2), U256::from(3)],
            }
        );
    }

    #[test]
    fn combine_purge_slots_different_address_prefers_other() {
        let left = RepairAction::PurgeSlots {
            address: addr(0x22),
            slots: vec![U256::from(1)],
        };
        let right = RepairAction::PurgeSlots {
            address: addr(0x33),
            slots: vec![U256::from(9)],
        };
        assert_eq!(left.combine(right.clone()), right);
    }

    #[test]
    fn combine_fallthrough_prefers_other() {
        let left = RepairAction::VerifySlots(vec![(addr(0x11), U256::from(1))]);
        let right = RepairAction::PurgeStorage(addr(0x44));
        assert_eq!(left.combine(right.clone()), right);
    }
}
