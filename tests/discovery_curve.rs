//! MANAGER-AUTHORED acceptance tests for Curve discovery via the MetaRegistry
//! (ViewCall through `AdapterCache::call_raw`). The implementation agent must
//! make these pass WITHOUT modifying them.
//!
//! A `MockViewCache` executes the MetaRegistry read calls by matching the
//! function selector and returning canned ABI responses — no real chain, no real
//! constants (those are pinned by the gated `tests/discovery_curve_rpc.rs`).

use std::collections::HashMap;

use alloy_primitives::{Address, Bytes, U256, address};
use alloy_sol_types::{SolCall, sol};
use anyhow::Result;
use evm_amm_state::adapters::{
    AdapterCache, CacheError, CallOutcome, CurveFactory, CurveFactoryConfig, CurveVariant,
    PoolDiscovery, PoolFactory, PoolKey, PoolQuery, ProtocolId, ProtocolMetadata, SlotChange,
    StateDiff, StateUpdate, StateView,
};

// The MetaRegistry read surface discovery relies on (named returns).
sol! {
    function find_pools_for_coins(address from, address to) external view returns (address[] pools);
    function get_coins(address pool) external view returns (address[8] coins);
    function is_meta(address pool) external view returns (bool meta);
    function get_pool_asset_type(address pool) external view returns (uint256 asset_type);
}

const META_REGISTRY: Address = address!("00000000000000000000000000000000000000e0");
const DAI: Address = address!("000000000000000000000000000000000000da01");
const USDC: Address = address!("000000000000000000000000000000000000c0de");
const USDT: Address = address!("000000000000000000000000000000000000d701");
const PLAIN_POOL: Address = address!("00000000000000000000000000000000000000b0");
const META_POOL: Address = address!("00000000000000000000000000000000000000ab");

struct PoolInfo {
    coins: Vec<Address>,
    is_meta: bool,
    asset_type: u64,
}

#[derive(Default)]
struct MockViewCache {
    pools_for_pair: Vec<Address>,
    pools: HashMap<Address, PoolInfo>,
}

impl StateView for MockViewCache {
    fn storage(&self, _address: Address, _slot: U256) -> Option<U256> {
        None
    }
}

impl MockViewCache {
    fn coins8(&self, pool: Address) -> [Address; 8] {
        let mut out = [Address::ZERO; 8];
        if let Some(info) = self.pools.get(&pool) {
            for (i, c) in info.coins.iter().take(8).enumerate() {
                out[i] = *c;
            }
        }
        out
    }
}

impl AdapterCache for MockViewCache {
    fn cached_storage(&self, _address: Address, _slot: U256) -> Option<U256> {
        None
    }
    fn apply_updates(&mut self, _updates: &[StateUpdate]) -> StateDiff {
        StateDiff::default()
    }
    fn verify_slots(&mut self, _slots: &[(Address, U256)]) -> Result<Vec<SlotChange>, CacheError> {
        Ok(Vec::new())
    }
    fn purge_storage(&mut self, _address: Address) -> StateDiff {
        StateDiff::default()
    }
    fn purge_slots(&mut self, _address: Address, _slots: &[U256]) -> StateDiff {
        StateDiff::default()
    }
    fn read_storage_slot(&mut self, _address: Address, _slot: U256) -> Result<U256, CacheError> {
        Ok(U256::ZERO)
    }
    fn read_storage_slots(&mut self, slots: &[(Address, U256)]) -> Result<Vec<U256>, CacheError> {
        Ok(vec![U256::ZERO; slots.len()])
    }
    fn call_raw(
        &mut self,
        _from: Address,
        _to: Address,
        calldata: Bytes,
        _commit: bool,
    ) -> Result<CallOutcome, CacheError> {
        let sel = calldata.get(..4).unwrap_or_default();
        // `abi_encode_returns` takes `&Self::Return`, which for a single return
        // value is that value's type directly (not a wrapper struct).
        let output: Vec<u8> = if sel == find_pools_for_coinsCall::SELECTOR {
            find_pools_for_coinsCall::abi_encode_returns(&self.pools_for_pair)
        } else if sel == get_coinsCall::SELECTOR {
            let call = get_coinsCall::abi_decode(&calldata)
                .map_err(|e| CacheError::Backend(Box::new(e)))?;
            get_coinsCall::abi_encode_returns(&self.coins8(call.pool))
        } else if sel == is_metaCall::SELECTOR {
            let call =
                is_metaCall::abi_decode(&calldata).map_err(|e| CacheError::Backend(Box::new(e)))?;
            let meta = self
                .pools
                .get(&call.pool)
                .map(|p| p.is_meta)
                .unwrap_or(false);
            is_metaCall::abi_encode_returns(&meta)
        } else if sel == get_pool_asset_typeCall::SELECTOR {
            let call = get_pool_asset_typeCall::abi_decode(&calldata)
                .map_err(|e| CacheError::Backend(Box::new(e)))?;
            let asset_type = U256::from(
                self.pools
                    .get(&call.pool)
                    .map(|p| p.asset_type)
                    .unwrap_or(0),
            );
            get_pool_asset_typeCall::abi_encode_returns(&asset_type)
        } else {
            return Ok(CallOutcome::Revert {
                output: Bytes::new(),
                gas_used: 0,
            });
        };
        Ok(CallOutcome::Success {
            output: output.into(),
            gas_used: 0,
        })
    }
}

fn discovery() -> PoolDiscovery {
    PoolDiscovery::new([
        Box::new(CurveFactory::new(CurveFactoryConfig::new(META_REGISTRY))) as Box<dyn PoolFactory>,
    ])
}

fn curve_metadata(
    pool: &evm_amm_state::adapters::DiscoveredPool,
) -> &evm_amm_state::adapters::CurveMetadata {
    match &pool.registration.metadata {
        ProtocolMetadata::Curve(m) => m,
        other => panic!("expected Curve metadata, got {other:?}"),
    }
}

/// A DAI/USDC query resolves a plain 3-coin pool via the MetaRegistry, keyed as
/// Curve, carrying the FULL coin set (multi-token ⊇ {DAI, USDC}).
#[test]
fn finds_plain_pool_with_full_coin_set() -> Result<()> {
    let mut cache = MockViewCache {
        pools_for_pair: vec![PLAIN_POOL],
        pools: HashMap::new(),
    };
    cache.pools.insert(
        PLAIN_POOL,
        PoolInfo {
            coins: vec![DAI, USDC, USDT],
            is_meta: false,
            asset_type: 0,
        },
    );

    let found = discovery().find(&mut cache, PoolQuery::pair(DAI, USDC).on(ProtocolId::Curve))?;
    assert_eq!(found.len(), 1, "one plain pool");
    assert_eq!(found[0].key, PoolKey::Curve(PLAIN_POOL));
    let md = curve_metadata(&found[0]);
    assert_eq!(
        md.coins,
        vec![DAI, USDC, USDT],
        "full coin set, not just the pair"
    );
    assert_eq!(
        md.variant,
        CurveVariant::StableSwap,
        "asset_type 0 => stable/int128"
    );
    Ok(())
}

/// Metapools (is_meta == true) are filtered out — plain pools only.
#[test]
fn metapools_are_filtered() -> Result<()> {
    let mut cache = MockViewCache {
        pools_for_pair: vec![META_POOL, PLAIN_POOL],
        pools: HashMap::new(),
    };
    cache.pools.insert(
        META_POOL,
        PoolInfo {
            coins: vec![DAI, USDC],
            is_meta: true,
            asset_type: 0,
        },
    );
    cache.pools.insert(
        PLAIN_POOL,
        PoolInfo {
            coins: vec![DAI, USDC, USDT],
            is_meta: false,
            asset_type: 0,
        },
    );

    let found = discovery().find(&mut cache, PoolQuery::pair(DAI, USDC).on(ProtocolId::Curve))?;
    assert_eq!(found.len(), 1, "metapool excluded");
    assert_eq!(found[0].key, PoolKey::Curve(PLAIN_POOL));
    Ok(())
}

/// `get_pool_asset_type == 4` (crypto) selects the uint256 `get_dy` variant.
#[test]
fn crypto_asset_type_selects_cryptoswap_variant() -> Result<()> {
    let mut cache = MockViewCache {
        pools_for_pair: vec![PLAIN_POOL],
        pools: HashMap::new(),
    };
    cache.pools.insert(
        PLAIN_POOL,
        PoolInfo {
            coins: vec![USDC, USDT],
            is_meta: false,
            asset_type: 4,
        },
    );

    let found = discovery().find(
        &mut cache,
        PoolQuery::pair(USDC, USDT).on(ProtocolId::Curve),
    )?;
    assert_eq!(found.len(), 1);
    assert_eq!(curve_metadata(&found[0]).variant, CurveVariant::CryptoSwap);
    Ok(())
}

/// An empty MetaRegistry answer is an empty result, not an error.
#[test]
fn no_pools_is_empty() -> Result<()> {
    let mut cache = MockViewCache::default();
    let found = discovery().find(&mut cache, PoolQuery::pair(DAI, USDC).on(ProtocolId::Curve))?;
    assert!(found.is_empty());
    Ok(())
}

/// A pool the registry returns that does NOT actually contain both query coins is
/// dropped (the MetaRegistry can over-return).
#[test]
fn pool_missing_a_query_coin_is_dropped() -> Result<()> {
    let mut cache = MockViewCache {
        pools_for_pair: vec![PLAIN_POOL],
        pools: HashMap::new(),
    };
    // Pool contains USDC + USDT but NOT DAI.
    cache.pools.insert(
        PLAIN_POOL,
        PoolInfo {
            coins: vec![USDC, USDT],
            is_meta: false,
            asset_type: 0,
        },
    );
    let found = discovery().find(&mut cache, PoolQuery::pair(DAI, USDC).on(ProtocolId::Curve))?;
    assert!(found.is_empty(), "pool lacking DAI is not a DAI/USDC pool");
    Ok(())
}
