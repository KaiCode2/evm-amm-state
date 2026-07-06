//! MANAGER-AUTHORED acceptance test for the crate's third-party extensibility
//! guarantee: a brand-new AMM adapter, defined ENTIRELY in this external test
//! crate (no crate edits, no fork), can be registered under a `ProtocolId::Custom`
//! protocol and dispatched through the registry to produce a quote.
//!
//! This pins the "add a novel AMM without friction" story that `examples/
//! custom_adapter.rs` and `docs/writing-an-adapter.md` teach. The implementation
//! agent building those artifacts must keep this passing WITHOUT modifying it.
//!
//! The demo adapter quotes with local constant-product math for self-containment
//! (no RPC, no contract). A production adapter would typically delegate to the
//! pool's on-chain quote entrypoint via `quote_via_call` — see the guide.

use std::sync::Arc;

use alloy_primitives::{Address, Bytes, U256};

use evm_amm_state::adapters::{
    AdapterCache, AdapterRegistry, AmmAdapter, CacheError, CallOutcome, CustomPoolKey, PoolKey,
    PoolRegistration, ProtocolId, ProtocolMetadata, SimConfig, SimError, SlotChange, StateDiff,
    StateUpdate, StateView, SwapQuote,
};

const PROTOCOL: &str = "constant-product-demo";

/// Per-pool config for the demo adapter, carried through
/// `ProtocolMetadata::Custom(Arc<dyn Any + Send + Sync>)` and recovered by
/// downcast in `simulate_swap`.
struct ReservesMeta {
    token0: Address,
    token1: Address,
    reserve0: U256,
    reserve1: U256,
}

/// A from-scratch adapter for a novel protocol. Only `protocol` and
/// `simulate_swap` are implemented; every other `AmmAdapter` method keeps its
/// default (cold-start unsupported, events ignored).
struct ConstantProductAdapter;

impl AmmAdapter for ConstantProductAdapter {
    fn protocol(&self) -> ProtocolId {
        ProtocolId::Custom(PROTOCOL)
    }

    fn simulate_swap(
        &self,
        pool: &PoolRegistration,
        _cache: &mut dyn AdapterCache,
        token_in: Address,
        token_out: Address,
        amount_in: U256,
        _config: &SimConfig,
    ) -> Result<SwapQuote, SimError> {
        let meta = match &pool.metadata {
            ProtocolMetadata::Custom(any) => any
                .downcast_ref::<ReservesMeta>()
                .ok_or(SimError::MissingMetadata("reserves"))?,
            _ => return Err(SimError::MissingMetadata("reserves")),
        };

        let (reserve_in, reserve_out) = if token_in == meta.token0 && token_out == meta.token1 {
            (meta.reserve0, meta.reserve1)
        } else if token_in == meta.token1 && token_out == meta.token0 {
            (meta.reserve1, meta.reserve0)
        } else {
            return Err(SimError::Custom("token pair not in pool".into()));
        };

        // Constant product (x*y=k), no fee: out = reserve_out * dx / (reserve_in + dx).
        let out = reserve_out
            .checked_mul(amount_in)
            .and_then(|numer| numer.checked_div(reserve_in + amount_in))
            .ok_or(SimError::Custom("overflow".into()))?;
        Ok(SwapQuote::new(out))
    }
}

/// Trivial cache: the demo adapter quotes from metadata, so no state is read.
struct NoCache;

impl StateView for NoCache {
    fn storage(&self, _address: Address, _slot: U256) -> Option<U256> {
        None
    }
}

impl AdapterCache for NoCache {
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
    fn call_raw(
        &mut self,
        _from: Address,
        _to: Address,
        _calldata: Bytes,
        _commit: bool,
    ) -> Result<CallOutcome, CacheError> {
        Ok(CallOutcome::Halt {
            reason: "unused".into(),
        })
    }
}

#[test]
fn custom_adapter_registers_and_quotes_end_to_end() {
    let token0 = Address::repeat_byte(0x01);
    let token1 = Address::repeat_byte(0x02);
    let key = PoolKey::Custom(CustomPoolKey::Address {
        protocol: PROTOCOL,
        address: Address::repeat_byte(0x03),
    });

    // A novel protocol id + key threads through registration and dispatch with no
    // crate change: `ProtocolId::Custom` / `PoolKey::Custom` are the open hatches.
    let registration = PoolRegistration::new(key.clone()).with_metadata(ProtocolMetadata::Custom(
        Arc::new(ReservesMeta {
            token0,
            token1,
            reserve0: U256::from(1_000_000_u64),
            reserve1: U256::from(2_000_000_u64),
        }),
    ));

    let mut registry = AdapterRegistry::new();
    registry
        .register_adapter(Arc::new(ConstantProductAdapter))
        .unwrap();
    registry.register_pool(registration).unwrap();

    // Dispatch exactly as a consumer would: look the adapter up by the pool's
    // protocol id and call simulate_swap.
    let adapter = registry
        .adapter(key.protocol())
        .expect("custom adapter must dispatch by ProtocolId::Custom");
    let pool = registry.pool(&key).unwrap();
    let mut cache = NoCache;

    let quote = adapter
        .simulate_swap(
            pool,
            &mut cache,
            token0,
            token1,
            U256::from(1_000_u64),
            &SimConfig::default(),
        )
        .expect("quote");

    // 2_000_000 * 1_000 / (1_000_000 + 1_000) = 1998 (integer division).
    assert_eq!(quote.amount_out, U256::from(1_998_u64));
}
