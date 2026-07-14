//! Versioned warm-resume persistence for immutable AMM registrations.
//!
//! This sidecar deliberately excludes cache state, runtime lineage, generations,
//! and lifecycle work. Loaded queueable registrations are reset to
//! [`PoolStatus::Pending`] and must pass the current runtime's hash-pinned
//! cold-start verification before becoming searchable.
//!
//! The checksum detects accidental corruption and torn/manual edits; it is not a
//! signature. Archives belong in a caller-controlled local cache directory, not
//! across an untrusted distribution boundary, because an attacker able to rewrite
//! the file can recompute its checksum. Persisted token identities, factories, and
//! storage layouts also rely on the adapter's immutable-contract assumption. A
//! deployment that can upgrade those facts must invalidate its sidecar when it
//! upgrades. Current-baseline cold-start verification refreshes state/read-set
//! values, but cannot prove that caller-classified immutable metadata stayed fixed.

use std::collections::BTreeSet;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use alloy_primitives::{Address, B256, U256, keccak256};
use serde::{Deserialize, Serialize};

use super::{
    AdapterRegistrySnapshot, BalancerTokenBalance, BalancerV2Metadata, CurveMetadata, CurveVariant,
    EventRoute, EventSource, PoolKey, PoolRegistration, PoolStatus, ProtocolMetadata,
    SolidlyStorageLayout, SolidlyV2Metadata, UniswapV2Metadata, V3Metadata, V3StorageLayout,
};

const MAGIC: &[u8; 8] = b"EASREG\0\0";
const VERSION: u32 = 1;
const VERSION_HEADER_BYTES: usize = MAGIC.len() + size_of::<u32>();
const CHECKSUM_BYTES: usize = 32;
const HEADER_BYTES: usize = VERSION_HEADER_BYTES + CHECKSUM_BYTES;
const MAX_FILE_BYTES: usize = 64 * 1024 * 1024;
const MAX_REGISTRATIONS: usize = 50_000;
const MAX_STATE_ADDRESSES: usize = 64;
const MAX_EVENT_SOURCES: usize = 64;
const MAX_EVENT_TOPICS: usize = 64;
const MAX_POOL_TOKENS: usize = 256;
const MAX_READ_SET_SLOTS: usize = 65_536;
const MAX_BALANCER_TOKEN_CASH: usize = 256;
const MAX_CODE_SEED_BYTES: usize = 1024 * 1024;
static NEXT_TEMP_FILE: AtomicU64 = AtomicU64::new(0);

/// A coherent set of pool registrations captured for warm resume.
///
/// The archive contains only registration configuration and reusable read-set
/// hints. It does not preserve runtime identity, generations, state revisions,
/// pending work, or EVM state.
#[derive(Clone, Debug)]
pub struct AmmRegistrationArchive {
    chain_id: u64,
    registrations: Vec<PoolRegistration>,
}

impl AmmRegistrationArchive {
    /// Capture built-in registrations from one immutable registry snapshot.
    ///
    /// Registrations are canonicalized so equivalent snapshots produce
    /// deterministic files. Opaque custom registrations are rejected explicitly.
    pub fn capture(
        chain_id: u64,
        snapshot: &AdapterRegistrySnapshot,
    ) -> Result<Self, AmmRegistrationPersistenceError> {
        check_limit("registrations", snapshot.pool_count(), MAX_REGISTRATIONS)?;
        let mut registrations = Vec::with_capacity(snapshot.pool_count());
        for (_, instance) in snapshot.pools() {
            let registration = snapshot.pool(instance).ok_or_else(|| {
                AmmRegistrationPersistenceError::Corrupt(
                    "registry snapshot lost an active pool registration".to_owned(),
                )
            })?;
            let mut registration = registration.clone();
            normalize_registration(&mut registration)?;
            validate_registration_limits(&registration)?;
            registrations.push(registration);
        }
        registrations.sort_by(|left, right| left.key.cmp(&right.key));
        Ok(Self {
            chain_id,
            registrations,
        })
    }

    /// Chain namespace this archive belongs to.
    pub const fn chain_id(&self) -> u64 {
        self.chain_id
    }

    /// Canonically ordered warm-resume registrations.
    pub fn registrations(&self) -> &[PoolRegistration] {
        &self.registrations
    }

    /// Consume the archive into its canonically ordered registrations.
    pub fn into_registrations(self) -> Vec<PoolRegistration> {
        self.registrations
    }

    /// Atomically save this archive to `path`.
    ///
    /// Encoding completes before any filesystem mutation. The implementation
    /// writes and fsyncs a unique same-directory temporary file, atomically
    /// renames it over the destination, then attempts a parent-directory fsync on
    /// platforms that support it. A directory-sync failure after the atomic rename
    /// is not reported as a failed save because the destination has already changed.
    pub fn save(&self, path: impl AsRef<Path>) -> Result<(), AmmRegistrationPersistenceError> {
        let path = path.as_ref();
        let persisted = PersistedArchive::try_from(self)?;
        let payload =
            serde_json::to_vec(&persisted).map_err(AmmRegistrationPersistenceError::Encode)?;
        let total = HEADER_BYTES.checked_add(payload.len()).ok_or(
            AmmRegistrationPersistenceError::LimitExceeded {
                field: "file bytes",
                limit: MAX_FILE_BYTES,
                actual: usize::MAX,
            },
        )?;
        check_limit("file bytes", total, MAX_FILE_BYTES)?;

        let mut bytes = Vec::with_capacity(total);
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&VERSION.to_le_bytes());
        bytes.extend_from_slice(keccak256(&payload).as_slice());
        bytes.extend_from_slice(&payload);
        atomic_write(path, &bytes)
    }

    /// Load and validate an archive for `expected_chain_id`.
    ///
    /// Queueable lifecycle states are reset to [`PoolStatus::Pending`]. A missing
    /// file is reported as an I/O error so callers can distinguish absence from an
    /// incompatible or corrupt warm-resume file.
    pub fn load(
        path: impl AsRef<Path>,
        expected_chain_id: u64,
    ) -> Result<Self, AmmRegistrationPersistenceError> {
        let path = path.as_ref();
        let file = File::open(path)
            .map_err(|source| AmmRegistrationPersistenceError::io("open", path, source))?;
        let metadata = file
            .metadata()
            .map_err(|source| AmmRegistrationPersistenceError::io("read metadata", path, source))?;
        let file_len = usize::try_from(metadata.len()).unwrap_or(usize::MAX);
        check_limit("file bytes", file_len, MAX_FILE_BYTES)?;
        let mut bytes = Vec::with_capacity(file_len);
        file.take((MAX_FILE_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|source| AmmRegistrationPersistenceError::io("read", path, source))?;
        check_limit("file bytes", bytes.len(), MAX_FILE_BYTES)?;
        if bytes.len() < HEADER_BYTES {
            return Err(AmmRegistrationPersistenceError::Corrupt(
                "registration archive is missing its version header".to_owned(),
            ));
        }
        if &bytes[..MAGIC.len()] != MAGIC {
            return Err(AmmRegistrationPersistenceError::InvalidMagic);
        }
        let version = u32::from_le_bytes(
            bytes[MAGIC.len()..VERSION_HEADER_BYTES]
                .try_into()
                .expect("version header has a fixed length"),
        );
        if version != VERSION {
            return Err(AmmRegistrationPersistenceError::IncompatibleVersion {
                expected: VERSION,
                actual: version,
            });
        }
        let expected_checksum = B256::from_slice(&bytes[VERSION_HEADER_BYTES..HEADER_BYTES]);
        let payload = &bytes[HEADER_BYTES..];
        if keccak256(payload) != expected_checksum {
            return Err(AmmRegistrationPersistenceError::ChecksumMismatch);
        }
        let persisted: PersistedArchive =
            serde_json::from_slice(payload).map_err(AmmRegistrationPersistenceError::Decode)?;
        if persisted.chain_id != expected_chain_id {
            return Err(AmmRegistrationPersistenceError::ChainMismatch {
                expected: expected_chain_id,
                actual: persisted.chain_id,
            });
        }
        check_limit(
            "registrations",
            persisted.registrations.len(),
            MAX_REGISTRATIONS,
        )?;

        let mut seen = BTreeSet::new();
        let mut registrations = Vec::with_capacity(persisted.registrations.len());
        for persisted in persisted.registrations {
            let mut registration = PoolRegistration::try_from(persisted)?;
            if !seen.insert(registration.key.clone()) {
                return Err(AmmRegistrationPersistenceError::DuplicatePool(
                    registration.key,
                ));
            }
            registration.status = resume_status(registration.status);
            normalize_registration(&mut registration)?;
            registrations.push(registration);
        }
        registrations.sort_by(|left, right| left.key.cmp(&right.key));
        Ok(Self {
            chain_id: expected_chain_id,
            registrations,
        })
    }
}

/// Failure capturing, encoding, validating, or atomically storing registrations.
#[derive(Debug)]
#[non_exhaustive]
pub enum AmmRegistrationPersistenceError {
    /// A filesystem operation failed.
    Io {
        /// Operation being attempted.
        operation: &'static str,
        /// Affected path.
        path: PathBuf,
        /// Underlying I/O failure.
        source: io::Error,
    },
    /// A registration could not be encoded.
    Encode(serde_json::Error),
    /// A recognized payload could not be decoded.
    Decode(serde_json::Error),
    /// The payload does not match the checksum written in its envelope.
    ChecksumMismatch,
    /// The file did not carry this crate's registration magic header.
    InvalidMagic,
    /// The file uses a schema version this crate does not understand.
    IncompatibleVersion {
        /// Current schema version.
        expected: u32,
        /// Version found on disk.
        actual: u32,
    },
    /// The archive belongs to another chain namespace.
    ChainMismatch {
        /// Requested chain identifier.
        expected: u64,
        /// Persisted chain identifier.
        actual: u64,
    },
    /// A corrupt payload declared the same pool more than once.
    DuplicatePool(PoolKey),
    /// Opaque custom registrations have no crate-owned persistence codec.
    UnsupportedCustom(PoolKey),
    /// A recognized built-in registration was internally inconsistent.
    InvalidRegistration {
        /// Rejected pool key.
        pool: PoolKey,
        /// Stable reason for rejection.
        reason: &'static str,
    },
    /// A file or collection exceeded a defensive decoding bound.
    LimitExceeded {
        /// Bounded field.
        field: &'static str,
        /// Maximum accepted value.
        limit: usize,
        /// Observed value.
        actual: usize,
    },
    /// A recognized file was structurally incomplete or internally divergent.
    Corrupt(String),
}

impl AmmRegistrationPersistenceError {
    fn io(operation: &'static str, path: &Path, source: io::Error) -> Self {
        Self::Io {
            operation,
            path: path.to_owned(),
            source,
        }
    }
}

impl fmt::Display for AmmRegistrationPersistenceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io {
                operation,
                path,
                source,
            } => write!(f, "failed to {operation} {}: {source}", path.display()),
            Self::Encode(error) => write!(f, "failed to encode registration archive: {error}"),
            Self::Decode(error) => write!(f, "failed to decode registration archive: {error}"),
            Self::ChecksumMismatch => write!(f, "registration archive checksum mismatch"),
            Self::InvalidMagic => write!(f, "unrecognized registration archive magic"),
            Self::IncompatibleVersion { expected, actual } => write!(
                f,
                "incompatible registration archive version: expected {expected}, found {actual}"
            ),
            Self::ChainMismatch { expected, actual } => write!(
                f,
                "registration archive chain mismatch: expected {expected}, found {actual}"
            ),
            Self::DuplicatePool(pool) => {
                write!(f, "registration archive contains duplicate pool {pool:?}")
            }
            Self::UnsupportedCustom(pool) => {
                write!(f, "custom registration has no persistence codec: {pool:?}")
            }
            Self::InvalidRegistration { pool, reason } => {
                write!(f, "invalid persisted registration {pool:?}: {reason}")
            }
            Self::LimitExceeded {
                field,
                limit,
                actual,
            } => write!(
                f,
                "registration archive {field} exceeds limit {limit}: {actual}"
            ),
            Self::Corrupt(reason) => write!(f, "corrupt registration archive: {reason}"),
        }
    }
}

impl std::error::Error for AmmRegistrationPersistenceError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Encode(source) | Self::Decode(source) => Some(source),
            _ => None,
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedArchive {
    chain_id: u64,
    registrations: Vec<PersistedRegistration>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedRegistration {
    key: PersistedPoolKey,
    state_addresses: Vec<Address>,
    event_sources: Vec<PersistedEventSource>,
    metadata: PersistedMetadata,
    status: PersistedStatus,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "protocol", rename_all = "snake_case")]
enum PersistedPoolKey {
    UniswapV2 { address: Address },
    UniswapV3 { address: Address },
    PancakeV3 { address: Address },
    Slipstream { address: Address },
    SolidlyV2 { address: Address },
    BalancerV2 { id: B256 },
    Curve { address: Address },
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedEventSource {
    emitter: Address,
    topics: Vec<B256>,
    route: PersistedEventRoute,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum PersistedEventRoute {
    Direct,
    IndexedAddress { topic_index: u64 },
    IndexedBytes32 { topic_index: u64 },
    AdapterDefined,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "protocol", rename_all = "snake_case")]
enum PersistedMetadata {
    Unknown,
    UniswapV2 {
        token0: Option<Address>,
        token1: Option<Address>,
        fee_bps: Option<u32>,
    },
    UniswapV3 {
        metadata: PersistedV3Metadata,
    },
    PancakeV3 {
        metadata: PersistedV3Metadata,
    },
    Slipstream {
        metadata: PersistedV3Metadata,
    },
    SolidlyV2 {
        token0: Option<Address>,
        token1: Option<Address>,
        stable: Option<bool>,
        storage_layout: Option<PersistedSolidlyLayout>,
    },
    BalancerV2 {
        vault: Option<Address>,
        pool_address: Option<Address>,
        tokens: Vec<Address>,
        balance_slots: Vec<U256>,
        token_cash: Vec<PersistedBalancerTokenBalance>,
    },
    Curve {
        coins: Vec<Address>,
        discovered_slots: Vec<U256>,
        variant: PersistedCurveVariant,
        code_seed: Option<Vec<u8>>,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedV3Metadata {
    token0: Option<Address>,
    token1: Option<Address>,
    fee: Option<u32>,
    tick_spacing: Option<i32>,
    factory: Option<Address>,
    quoter: Option<Address>,
    storage_layout: Option<PersistedV3Layout>,
    warm_word_radius: Option<i16>,
    #[serde(default)]
    warmed_slots: Vec<U256>,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedV3Layout {
    slot0_slot: U256,
    liquidity_slot: U256,
    ticks_base_slot: U256,
    tick_bitmap_base_slot: U256,
    tick_spacing: i32,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedSolidlyLayout {
    reserve0_slot: U256,
    reserve1_slot: U256,
    token0_slot: U256,
    token1_slot: U256,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedBalancerTokenBalance {
    token: Address,
    slot: U256,
    high_field: bool,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PersistedCurveVariant {
    StableSwap,
    CryptoSwap,
    CryptoSwapNg,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PersistedStatus {
    Pending,
    Cold,
    Ready,
    Degraded,
    Disabled,
    Unsupported,
}

impl TryFrom<&AmmRegistrationArchive> for PersistedArchive {
    type Error = AmmRegistrationPersistenceError;

    fn try_from(archive: &AmmRegistrationArchive) -> Result<Self, Self::Error> {
        check_limit(
            "registrations",
            archive.registrations.len(),
            MAX_REGISTRATIONS,
        )?;
        for registration in &archive.registrations {
            validate_registration_limits(registration)?;
        }
        Ok(Self {
            chain_id: archive.chain_id,
            registrations: archive
                .registrations
                .iter()
                .map(PersistedRegistration::try_from)
                .collect::<Result<_, _>>()?,
        })
    }
}

impl TryFrom<&PoolRegistration> for PersistedRegistration {
    type Error = AmmRegistrationPersistenceError;

    fn try_from(registration: &PoolRegistration) -> Result<Self, Self::Error> {
        let key = match &registration.key {
            PoolKey::UniswapV2(address) => PersistedPoolKey::UniswapV2 { address: *address },
            PoolKey::UniswapV3(address) => PersistedPoolKey::UniswapV3 { address: *address },
            PoolKey::PancakeV3(address) => PersistedPoolKey::PancakeV3 { address: *address },
            PoolKey::Slipstream(address) => PersistedPoolKey::Slipstream { address: *address },
            PoolKey::SolidlyV2(address) => PersistedPoolKey::SolidlyV2 { address: *address },
            PoolKey::BalancerV2(id) => PersistedPoolKey::BalancerV2 { id: *id },
            PoolKey::Curve(address) => PersistedPoolKey::Curve { address: *address },
            PoolKey::Custom(_) => {
                return Err(AmmRegistrationPersistenceError::UnsupportedCustom(
                    registration.key.clone(),
                ));
            }
            #[cfg(feature = "experimental-protocols")]
            PoolKey::BalancerV3(_) | PoolKey::Erc4626(_) | PoolKey::UniswapV4(_) => {
                return Err(AmmRegistrationPersistenceError::InvalidRegistration {
                    pool: registration.key.clone(),
                    reason: "built-in protocol is not supported by this archive version",
                });
            }
        };
        let metadata = match &registration.metadata {
            ProtocolMetadata::Unknown => PersistedMetadata::Unknown,
            ProtocolMetadata::UniswapV2(metadata)
                if matches!(registration.key, PoolKey::UniswapV2(_)) =>
            {
                PersistedMetadata::UniswapV2 {
                    token0: metadata.token0,
                    token1: metadata.token1,
                    fee_bps: metadata.fee_bps,
                }
            }
            ProtocolMetadata::UniswapV3(metadata)
                if matches!(registration.key, PoolKey::UniswapV3(_)) =>
            {
                PersistedMetadata::UniswapV3 {
                    metadata: metadata.into(),
                }
            }
            ProtocolMetadata::PancakeV3(metadata)
                if matches!(registration.key, PoolKey::PancakeV3(_)) =>
            {
                PersistedMetadata::PancakeV3 {
                    metadata: metadata.into(),
                }
            }
            ProtocolMetadata::Slipstream(metadata)
                if matches!(registration.key, PoolKey::Slipstream(_)) =>
            {
                PersistedMetadata::Slipstream {
                    metadata: metadata.into(),
                }
            }
            ProtocolMetadata::SolidlyV2(metadata)
                if matches!(registration.key, PoolKey::SolidlyV2(_)) =>
            {
                PersistedMetadata::SolidlyV2 {
                    token0: metadata.token0,
                    token1: metadata.token1,
                    stable: metadata.stable,
                    storage_layout: metadata.storage_layout.map(Into::into),
                }
            }
            ProtocolMetadata::BalancerV2(metadata)
                if matches!(registration.key, PoolKey::BalancerV2(_)) =>
            {
                PersistedMetadata::BalancerV2 {
                    vault: metadata.vault,
                    pool_address: metadata.pool_address,
                    tokens: metadata.tokens.clone(),
                    balance_slots: metadata.balance_slots.clone(),
                    token_cash: metadata
                        .token_cash
                        .iter()
                        .copied()
                        .map(Into::into)
                        .collect(),
                }
            }
            ProtocolMetadata::Curve(metadata) if matches!(registration.key, PoolKey::Curve(_)) => {
                PersistedMetadata::Curve {
                    coins: metadata.coins.clone(),
                    discovered_slots: metadata.discovered_slots.clone(),
                    variant: metadata.variant.into(),
                    code_seed: metadata.code_seed.as_ref().map(|bytes| bytes.to_vec()),
                }
            }
            ProtocolMetadata::Custom(_) => {
                return Err(AmmRegistrationPersistenceError::UnsupportedCustom(
                    registration.key.clone(),
                ));
            }
            _ => {
                return Err(AmmRegistrationPersistenceError::InvalidRegistration {
                    pool: registration.key.clone(),
                    reason: "pool key and protocol metadata do not match",
                });
            }
        };
        Ok(Self {
            key,
            state_addresses: registration.state_addresses.clone(),
            event_sources: registration
                .event_sources
                .iter()
                .map(PersistedEventSource::try_from)
                .collect::<Result<_, _>>()?,
            metadata,
            status: registration.status.into(),
        })
    }
}

impl TryFrom<PersistedRegistration> for PoolRegistration {
    type Error = AmmRegistrationPersistenceError;

    fn try_from(persisted: PersistedRegistration) -> Result<Self, Self::Error> {
        check_limit(
            "state addresses",
            persisted.state_addresses.len(),
            MAX_STATE_ADDRESSES,
        )?;
        check_limit(
            "event sources",
            persisted.event_sources.len(),
            MAX_EVENT_SOURCES,
        )?;
        let key = match persisted.key {
            PersistedPoolKey::UniswapV2 { address } => PoolKey::UniswapV2(address),
            PersistedPoolKey::UniswapV3 { address } => PoolKey::UniswapV3(address),
            PersistedPoolKey::PancakeV3 { address } => PoolKey::PancakeV3(address),
            PersistedPoolKey::Slipstream { address } => PoolKey::Slipstream(address),
            PersistedPoolKey::SolidlyV2 { address } => PoolKey::SolidlyV2(address),
            PersistedPoolKey::BalancerV2 { id } => PoolKey::BalancerV2(id),
            PersistedPoolKey::Curve { address } => PoolKey::Curve(address),
        };
        let metadata = match persisted.metadata {
            PersistedMetadata::Unknown => ProtocolMetadata::Unknown,
            PersistedMetadata::UniswapV2 {
                token0,
                token1,
                fee_bps,
            } if matches!(key, PoolKey::UniswapV2(_)) => {
                ProtocolMetadata::UniswapV2(UniswapV2Metadata {
                    token0,
                    token1,
                    fee_bps,
                })
            }
            PersistedMetadata::UniswapV3 { metadata } if matches!(key, PoolKey::UniswapV3(_)) => {
                ProtocolMetadata::UniswapV3(metadata.into())
            }
            PersistedMetadata::PancakeV3 { metadata } if matches!(key, PoolKey::PancakeV3(_)) => {
                ProtocolMetadata::PancakeV3(metadata.into())
            }
            PersistedMetadata::Slipstream { metadata } if matches!(key, PoolKey::Slipstream(_)) => {
                ProtocolMetadata::Slipstream(metadata.into())
            }
            PersistedMetadata::SolidlyV2 {
                token0,
                token1,
                stable,
                storage_layout,
            } if matches!(key, PoolKey::SolidlyV2(_)) => {
                ProtocolMetadata::SolidlyV2(SolidlyV2Metadata {
                    token0,
                    token1,
                    stable,
                    storage_layout: storage_layout.map(Into::into),
                })
            }
            PersistedMetadata::BalancerV2 {
                vault,
                pool_address,
                tokens,
                balance_slots,
                token_cash,
            } if matches!(key, PoolKey::BalancerV2(_)) => {
                check_limit("Balancer tokens", tokens.len(), MAX_POOL_TOKENS)?;
                check_limit(
                    "Balancer balance slots",
                    balance_slots.len(),
                    MAX_READ_SET_SLOTS,
                )?;
                check_limit(
                    "Balancer token cash",
                    token_cash.len(),
                    MAX_BALANCER_TOKEN_CASH,
                )?;
                ProtocolMetadata::BalancerV2(BalancerV2Metadata {
                    vault,
                    pool_address,
                    tokens,
                    balance_slots,
                    token_cash: token_cash.into_iter().map(Into::into).collect(),
                })
            }
            PersistedMetadata::Curve {
                coins,
                discovered_slots,
                variant,
                code_seed,
            } if matches!(key, PoolKey::Curve(_)) => {
                check_limit("Curve coins", coins.len(), MAX_POOL_TOKENS)?;
                check_limit(
                    "Curve discovered slots",
                    discovered_slots.len(),
                    MAX_READ_SET_SLOTS,
                )?;
                if let Some(code_seed) = &code_seed {
                    check_limit(
                        "Curve code seed bytes",
                        code_seed.len(),
                        MAX_CODE_SEED_BYTES,
                    )?;
                }
                ProtocolMetadata::Curve(CurveMetadata {
                    coins,
                    discovered_slots,
                    variant: variant.into(),
                    code_seed: code_seed.map(Into::into),
                })
            }
            _ => {
                return Err(AmmRegistrationPersistenceError::InvalidRegistration {
                    pool: key,
                    reason: "pool key and protocol metadata do not match",
                });
            }
        };
        Ok(PoolRegistration {
            key,
            state_addresses: persisted.state_addresses,
            event_sources: persisted
                .event_sources
                .into_iter()
                .map(EventSource::try_from)
                .collect::<Result<_, _>>()?,
            metadata,
            status: persisted.status.into(),
        })
    }
}

impl TryFrom<&EventSource> for PersistedEventSource {
    type Error = AmmRegistrationPersistenceError;

    fn try_from(source: &EventSource) -> Result<Self, Self::Error> {
        Ok(Self {
            emitter: source.emitter,
            topics: source.topics.clone(),
            route: match source.route {
                EventRoute::Direct => PersistedEventRoute::Direct,
                EventRoute::IndexedAddress { topic_index } => PersistedEventRoute::IndexedAddress {
                    topic_index: u64::try_from(topic_index).map_err(|_| {
                        AmmRegistrationPersistenceError::Corrupt(
                            "event topic index exceeds u64".to_owned(),
                        )
                    })?,
                },
                EventRoute::IndexedBytes32 { topic_index } => PersistedEventRoute::IndexedBytes32 {
                    topic_index: u64::try_from(topic_index).map_err(|_| {
                        AmmRegistrationPersistenceError::Corrupt(
                            "event topic index exceeds u64".to_owned(),
                        )
                    })?,
                },
                EventRoute::AdapterDefined => PersistedEventRoute::AdapterDefined,
            },
        })
    }
}

impl TryFrom<PersistedEventSource> for EventSource {
    type Error = AmmRegistrationPersistenceError;

    fn try_from(source: PersistedEventSource) -> Result<Self, Self::Error> {
        check_limit("event topics", source.topics.len(), MAX_EVENT_TOPICS)?;
        let route = match source.route {
            PersistedEventRoute::Direct => EventRoute::Direct,
            PersistedEventRoute::IndexedAddress { topic_index } => EventRoute::IndexedAddress {
                topic_index: usize::try_from(topic_index).map_err(|_| {
                    AmmRegistrationPersistenceError::Corrupt(
                        "event topic index exceeds usize".to_owned(),
                    )
                })?,
            },
            PersistedEventRoute::IndexedBytes32 { topic_index } => EventRoute::IndexedBytes32 {
                topic_index: usize::try_from(topic_index).map_err(|_| {
                    AmmRegistrationPersistenceError::Corrupt(
                        "event topic index exceeds usize".to_owned(),
                    )
                })?,
            },
            PersistedEventRoute::AdapterDefined => EventRoute::AdapterDefined,
        };
        Ok(EventSource {
            emitter: source.emitter,
            topics: source.topics,
            route,
        })
    }
}

impl From<CurveVariant> for PersistedCurveVariant {
    fn from(variant: CurveVariant) -> Self {
        match variant {
            CurveVariant::StableSwap => Self::StableSwap,
            CurveVariant::CryptoSwap => Self::CryptoSwap,
            CurveVariant::CryptoSwapNG => Self::CryptoSwapNg,
        }
    }
}

impl From<PersistedCurveVariant> for CurveVariant {
    fn from(variant: PersistedCurveVariant) -> Self {
        match variant {
            PersistedCurveVariant::StableSwap => Self::StableSwap,
            PersistedCurveVariant::CryptoSwap => Self::CryptoSwap,
            PersistedCurveVariant::CryptoSwapNg => Self::CryptoSwapNG,
        }
    }
}

impl From<&V3Metadata> for PersistedV3Metadata {
    fn from(metadata: &V3Metadata) -> Self {
        Self {
            token0: metadata.token0,
            token1: metadata.token1,
            fee: metadata.fee,
            tick_spacing: metadata.tick_spacing,
            factory: metadata.factory,
            quoter: metadata.quoter,
            storage_layout: metadata.storage_layout.map(Into::into),
            warm_word_radius: metadata.warm_word_radius,
            warmed_slots: metadata.warmed_slots.clone(),
        }
    }
}

impl From<PersistedV3Metadata> for V3Metadata {
    fn from(metadata: PersistedV3Metadata) -> Self {
        Self {
            token0: metadata.token0,
            token1: metadata.token1,
            fee: metadata.fee,
            tick_spacing: metadata.tick_spacing,
            factory: metadata.factory,
            quoter: metadata.quoter,
            storage_layout: metadata.storage_layout.map(Into::into),
            warm_word_radius: metadata.warm_word_radius,
            warmed_slots: metadata.warmed_slots,
        }
    }
}

impl From<V3StorageLayout> for PersistedV3Layout {
    fn from(layout: V3StorageLayout) -> Self {
        Self {
            slot0_slot: layout.slot0_slot,
            liquidity_slot: layout.liquidity_slot,
            ticks_base_slot: layout.ticks_base_slot,
            tick_bitmap_base_slot: layout.tick_bitmap_base_slot,
            tick_spacing: layout.tick_spacing,
        }
    }
}

impl From<PersistedV3Layout> for V3StorageLayout {
    fn from(layout: PersistedV3Layout) -> Self {
        Self::new(
            layout.slot0_slot,
            layout.liquidity_slot,
            layout.ticks_base_slot,
            layout.tick_bitmap_base_slot,
            layout.tick_spacing,
        )
    }
}

impl From<SolidlyStorageLayout> for PersistedSolidlyLayout {
    fn from(layout: SolidlyStorageLayout) -> Self {
        Self {
            reserve0_slot: layout.reserve0_slot,
            reserve1_slot: layout.reserve1_slot,
            token0_slot: layout.token0_slot,
            token1_slot: layout.token1_slot,
        }
    }
}

impl From<PersistedSolidlyLayout> for SolidlyStorageLayout {
    fn from(layout: PersistedSolidlyLayout) -> Self {
        Self::new(
            layout.reserve0_slot,
            layout.reserve1_slot,
            layout.token0_slot,
            layout.token1_slot,
        )
    }
}

impl From<BalancerTokenBalance> for PersistedBalancerTokenBalance {
    fn from(balance: BalancerTokenBalance) -> Self {
        Self {
            token: balance.token,
            slot: balance.slot,
            high_field: balance.high_field,
        }
    }
}

impl From<PersistedBalancerTokenBalance> for BalancerTokenBalance {
    fn from(balance: PersistedBalancerTokenBalance) -> Self {
        Self::new(balance.token, balance.slot, balance.high_field)
    }
}

impl From<PoolStatus> for PersistedStatus {
    fn from(status: PoolStatus) -> Self {
        match status {
            PoolStatus::Pending => Self::Pending,
            PoolStatus::Cold => Self::Cold,
            PoolStatus::Ready => Self::Ready,
            PoolStatus::Degraded => Self::Degraded,
            PoolStatus::Disabled => Self::Disabled,
            PoolStatus::Unsupported => Self::Unsupported,
        }
    }
}

impl From<PersistedStatus> for PoolStatus {
    fn from(status: PersistedStatus) -> Self {
        match status {
            PersistedStatus::Pending => Self::Pending,
            PersistedStatus::Cold => Self::Cold,
            PersistedStatus::Ready => Self::Ready,
            PersistedStatus::Degraded => Self::Degraded,
            PersistedStatus::Disabled => Self::Disabled,
            PersistedStatus::Unsupported => Self::Unsupported,
        }
    }
}

fn resume_status(status: PoolStatus) -> PoolStatus {
    match status {
        PoolStatus::Pending | PoolStatus::Cold | PoolStatus::Ready | PoolStatus::Degraded => {
            PoolStatus::Pending
        }
        PoolStatus::Disabled => PoolStatus::Disabled,
        PoolStatus::Unsupported => PoolStatus::Unsupported,
    }
}

fn validate_registration_limits(
    registration: &PoolRegistration,
) -> Result<(), AmmRegistrationPersistenceError> {
    check_limit(
        "state addresses",
        registration.state_addresses.len(),
        MAX_STATE_ADDRESSES,
    )?;
    check_limit(
        "event sources",
        registration.event_sources.len(),
        MAX_EVENT_SOURCES,
    )?;
    for source in &registration.event_sources {
        check_limit("event topics", source.topics.len(), MAX_EVENT_TOPICS)?;
    }
    match &registration.metadata {
        ProtocolMetadata::BalancerV2(metadata) => {
            check_limit("Balancer tokens", metadata.tokens.len(), MAX_POOL_TOKENS)?;
            check_limit(
                "Balancer balance slots",
                metadata.balance_slots.len(),
                MAX_READ_SET_SLOTS,
            )?;
            check_limit(
                "Balancer token cash",
                metadata.token_cash.len(),
                MAX_BALANCER_TOKEN_CASH,
            )?;
        }
        ProtocolMetadata::Curve(metadata) => {
            check_limit("Curve coins", metadata.coins.len(), MAX_POOL_TOKENS)?;
            check_limit(
                "Curve discovered slots",
                metadata.discovered_slots.len(),
                MAX_READ_SET_SLOTS,
            )?;
            if let Some(code_seed) = &metadata.code_seed {
                check_limit(
                    "Curve code seed bytes",
                    code_seed.len(),
                    MAX_CODE_SEED_BYTES,
                )?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn normalize_registration(
    registration: &mut PoolRegistration,
) -> Result<(), AmmRegistrationPersistenceError> {
    if matches!(registration.key, PoolKey::Custom(_))
        || matches!(registration.metadata, ProtocolMetadata::Custom(_))
    {
        return Err(AmmRegistrationPersistenceError::UnsupportedCustom(
            registration.key.clone(),
        ));
    }
    registration.state_addresses.sort_unstable();
    registration.state_addresses.dedup();
    for source in &mut registration.event_sources {
        source.topics.sort_unstable();
        source.topics.dedup();
    }
    registration.event_sources.sort_by(|left, right| {
        left.emitter
            .cmp(&right.emitter)
            .then_with(|| left.topics.cmp(&right.topics))
            .then_with(|| event_route_key(left.route).cmp(&event_route_key(right.route)))
    });
    registration.event_sources.dedup();
    match &mut registration.metadata {
        ProtocolMetadata::BalancerV2(metadata) => {
            metadata.balance_slots.sort_unstable();
            metadata.balance_slots.dedup();
            metadata
                .token_cash
                .sort_by_key(|balance| (balance.token, balance.slot, balance.high_field));
            metadata.token_cash.dedup();
        }
        ProtocolMetadata::Curve(metadata) => {
            metadata.discovered_slots.sort_unstable();
            metadata.discovered_slots.dedup();
        }
        _ => {}
    }
    Ok(())
}

fn event_route_key(route: EventRoute) -> (u8, usize) {
    match route {
        EventRoute::Direct => (0, 0),
        EventRoute::IndexedAddress { topic_index } => (1, topic_index),
        EventRoute::IndexedBytes32 { topic_index } => (2, topic_index),
        EventRoute::AdapterDefined => (3, 0),
    }
}

fn check_limit(
    field: &'static str,
    actual: usize,
    limit: usize,
) -> Result<(), AmmRegistrationPersistenceError> {
    if actual > limit {
        Err(AmmRegistrationPersistenceError::LimitExceeded {
            field,
            limit,
            actual,
        })
    } else {
        Ok(())
    }
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), AmmRegistrationPersistenceError> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent).map_err(|source| {
        AmmRegistrationPersistenceError::io("create directory", parent, source)
    })?;
    let file_name = path.file_name().ok_or_else(|| {
        AmmRegistrationPersistenceError::Corrupt(
            "registration archive path has no file name".to_owned(),
        )
    })?;
    let sequence = NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed);
    let temp_path = parent.join(format!(
        ".{}.{}.{}.tmp",
        file_name.to_string_lossy(),
        std::process::id(),
        sequence
    ));

    let result = (|| {
        let mut temp = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
            .map_err(|source| {
                AmmRegistrationPersistenceError::io("create temporary file", &temp_path, source)
            })?;
        temp.write_all(bytes).map_err(|source| {
            AmmRegistrationPersistenceError::io("write temporary file", &temp_path, source)
        })?;
        temp.sync_all().map_err(|source| {
            AmmRegistrationPersistenceError::io("sync temporary file", &temp_path, source)
        })?;
        drop(temp);
        std::fs::rename(&temp_path, path).map_err(|source| {
            AmmRegistrationPersistenceError::io("rename temporary file", path, source)
        })?;
        sync_parent_best_effort(parent);
        Ok(())
    })();

    if result.is_err() {
        let _ = std::fs::remove_file(&temp_path);
    }
    result
}

fn sync_parent_best_effort(parent: &Path) {
    #[cfg(unix)]
    if let Ok(directory) = File::open(parent) {
        let _ = directory.sync_all();
    }
    #[cfg(not(unix))]
    let _ = parent;
}
