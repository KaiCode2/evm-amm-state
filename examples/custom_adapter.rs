//! Add a brand-new AMM to `evm-amm-state` from *outside* the crate — no fork, no
//! `src/` edit, no new enum variant.
//!
//! This example is the runnable companion to [`docs/writing-an-adapter.md`]. It
//! implements a toy `ConstantProductAdapter` for an invented protocol and drives
//! it through the same registry a real consumer uses: register the adapter,
//! register a pool, dispatch by protocol id, and quote a swap in both
//! directions. It runs fully offline — no RPC, no env vars, no live node:
//!
//! ```text
//! cargo run --example custom_adapter
//! ```
//!
//! ## The extension points, in the order this file uses them
//!
//! A third party can name and serve a novel AMM through five open hatches, none
//! of which require touching the crate:
//!
//! 1. [`ProtocolId::Custom(&'static str)`] — a protocol identity for an AMM the
//!    crate has never heard of, with no `ProtocolId` enum edit.
//! 2. [`PoolKey::Custom(CustomPoolKey)`] — a pool identity keyed by address,
//!    bytes32, or both, tagged with that same protocol string.
//! 3. [`ProtocolMetadata::Custom(Arc<dyn Any + Send + Sync>)`] — an opaque,
//!    per-pool config blob you define, recovered inside the adapter with
//!    `downcast_ref`.
//! 4. The [`AmmAdapter`] trait — one *required* method (`protocol`); the rest
//!    are defaulted, so a minimal adapter overrides only `protocol` +
//!    `simulate_swap`.
//! 5. [`AdapterRegistry::register_adapter`] + dispatch by
//!    [`AdapterRegistry::adapter`]`(pool.protocol())`.
//!
//! ## Local math here vs. the crate's real design
//!
//! IMPORTANT: this adapter computes `amount_out` with **local constant-product
//! math** purely so the example is self-contained (the reserves live in metadata;
//! nothing is read from a chain). That is the *fallback* strategy.
//!
//! The crate's actual design philosophy is the opposite: a production adapter
//! **reimplements no AMM math**. It builds the pool's own canonical on-chain
//! quote calldata and runs it against the warmed cache via
//! [`quote_via_call`](evm_amm_state::adapters::quote_via_call), then decodes the
//! output — `eth_call`-grade correctness with nothing to drift from the real
//! contract. See [`src/adapters/solidly_v2.rs`] (`SolidlyV2Adapter::simulate_swap`
//! calls the pool's `getAmountOut`) for a real, minimal template of that pattern,
//! and [`docs/writing-an-adapter.md`] for when each strategy applies.
//!
//! [`docs/writing-an-adapter.md`]: ../docs/writing-an-adapter.md
//! [`src/adapters/solidly_v2.rs`]: ../src/adapters/solidly_v2.rs

use std::sync::Arc;

use alloy_primitives::{Address, Bytes, U256};

use evm_amm_state::adapters::{
    AdapterCache, AdapterRegistry, AmmAdapter, CacheError, CallOutcome, CustomPoolKey, PoolKey,
    PoolRegistration, ProtocolId, ProtocolMetadata, SimConfig, SimError, SlotChange, StateDiff,
    StateUpdate, StateView, SwapQuote,
};

/// The `&'static str` that names our invented protocol. It flows through
/// `ProtocolId::Custom`, `CustomPoolKey`, and registry dispatch; keeping it in
/// one place is what ties the adapter, the pool key, and the lookup together.
const PROTOCOL: &str = "constant-product-demo";

// -----------------------------------------------------------------------------
// (3) Custom metadata: our own per-pool config, carried opaquely by the crate.
// -----------------------------------------------------------------------------

/// Per-pool config for the demo adapter.
///
/// The crate stores this behind `ProtocolMetadata::Custom(Arc<dyn Any + Send +
/// Sync>)` without knowing its shape; `simulate_swap` recovers the concrete type
/// with `downcast_ref`. A real adapter would put a storage layout, a quote-target
/// address, token decimals, etc. here — whatever its `simulate_swap` needs.
struct ReservesMeta {
    token0: Address,
    token1: Address,
    reserve0: U256,
    reserve1: U256,
}

// -----------------------------------------------------------------------------
// (4) The adapter: a minimal `AmmAdapter` — only `protocol` + `simulate_swap`.
// -----------------------------------------------------------------------------

/// A from-scratch adapter for a novel constant-product AMM.
///
/// Only the required `protocol` method and `simulate_swap` are implemented. Every
/// other `AmmAdapter` method keeps its trait default: `protocols` reports just
/// this one id, `event_sources`/`decode_event` make it inert to reactive updates,
/// and `cold_start_planner` reports the protocol as unsupported for warming.
/// That is enough to register and quote.
struct ConstantProductAdapter;

impl AmmAdapter for ConstantProductAdapter {
    /// The one required method: which protocol this adapter serves. The registry
    /// keys the adapter by this id and dispatches to it for matching pools.
    fn protocol(&self) -> ProtocolId {
        ProtocolId::Custom(PROTOCOL)
    }

    /// Quote `amount_in` of `token_in` into `token_out`.
    ///
    /// LOCAL MATH — demo only. We recover our `ReservesMeta` from the pool's
    /// `Custom` metadata and apply the constant-product formula directly. A
    /// production adapter would instead build the pool's on-chain quote calldata
    /// and run it via `quote_via_call(cache, target, calldata)` (see the module
    /// doc and `src/adapters/solidly_v2.rs`); the `cache` argument is exactly what
    /// you would thread into that helper.
    fn simulate_swap(
        &self,
        pool: &PoolRegistration,
        _cache: &mut dyn AdapterCache,
        token_in: Address,
        token_out: Address,
        amount_in: U256,
        _config: &SimConfig,
    ) -> Result<SwapQuote, SimError> {
        // Recover our concrete metadata from the opaque `Custom` blob. This
        // `downcast_ref` is the counterpart to wrapping it in `Arc::new(..)` at
        // registration — it is how the crate hands arbitrary config back to the
        // adapter that understands it.
        let meta = match &pool.metadata {
            ProtocolMetadata::Custom(any) => any
                .downcast_ref::<ReservesMeta>()
                .ok_or(SimError::MissingMetadata("reserves"))?,
            _ => return Err(SimError::MissingMetadata("reserves")),
        };

        // Orient the reserves to the requested direction.
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

// -----------------------------------------------------------------------------
// A trivial `AdapterCache`.
//
// `simulate_swap` takes `&mut dyn AdapterCache` because the RECOMMENDED strategy
// runs a quote call against a warmed EVM cache. Our demo quotes from metadata and
// never touches state, so this no-op cache satisfies the type without warming
// anything. A real integration passes an `evm_fork_cache` cache warmed by
// cold-start instead — this stub stands in only because the demo needs no state.
// -----------------------------------------------------------------------------

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

fn main() {
    // Two fake tokens and a fake pool address. Only their identities matter for
    // the demo — no bytecode, no chain.
    let token0 = Address::repeat_byte(0x01);
    let token1 = Address::repeat_byte(0x02);

    // (1) + (2) Name the protocol and key the pool with the `Custom` hatches. No
    // `ProtocolId`/`PoolKey` enum variant is added — the `&'static str` tag is the
    // whole extension mechanism, and `key.protocol()` derives the matching
    // `ProtocolId::Custom(PROTOCOL)` used for dispatch below.
    let key = PoolKey::Custom(CustomPoolKey::Address {
        protocol: PROTOCOL,
        address: Address::repeat_byte(0x03),
    });

    // (3) Attach our arbitrary per-pool config via `ProtocolMetadata::Custom`. The
    // crate stores the `Arc<dyn Any>` without inspecting it; the adapter downcasts
    // it back in `simulate_swap`.
    let registration = PoolRegistration::new(key.clone()).with_metadata(ProtocolMetadata::Custom(
        Arc::new(ReservesMeta {
            token0,
            token1,
            reserve0: U256::from(1_000_000_u64),
            reserve1: U256::from(2_000_000_u64),
        }),
    ));

    // (5) Register the adapter (keyed by `ProtocolId::Custom(PROTOCOL)`) and the
    // pool. This is the entire wiring a consumer performs for a new protocol.
    let mut registry = AdapterRegistry::new();
    registry
        .register_adapter(Arc::new(ConstantProductAdapter))
        .expect("register adapter");
    registry.register_pool(registration).expect("register pool");

    // Dispatch exactly as a consumer would: look the adapter up by the pool's
    // protocol id, fetch the pool, and quote. `registry.adapter(..)` is the same
    // path the reactive/simulation layers use internally.
    let adapter = registry
        .adapter(key.protocol())
        .expect("custom adapter dispatches by ProtocolId::Custom");
    let pool = registry.pool(&key).expect("pool is registered");
    let mut cache = NoCache;
    let config = SimConfig::default();

    let amount_in = U256::from(1_000_u64);

    // Direction A: token0 -> token1.
    // 2_000_000 * 1_000 / (1_000_000 + 1_000) = 1998 (integer division).
    let quote_0_to_1 = adapter
        .simulate_swap(pool, &mut cache, token0, token1, amount_in, &config)
        .expect("quote token0 -> token1");

    // Direction B: token1 -> token0.
    // 1_000_000 * 1_000 / (2_000_000 + 1_000) = 499 (integer division).
    let quote_1_to_0 = adapter
        .simulate_swap(pool, &mut cache, token1, token0, amount_in, &config)
        .expect("quote token1 -> token0");

    println!("custom adapter '{PROTOCOL}' registered and dispatched via ProtocolId::Custom\n");
    println!("reserves: token0 = 1_000_000, token1 = 2_000_000 (constant product, no fee)\n");
    println!("  {amount_in} token0 -> {} token1", quote_0_to_1.amount_out);
    println!("  {amount_in} token1 -> {} token0", quote_1_to_0.amount_out);

    // Self-verify: the example asserts its own output so a regression fails the
    // run instead of printing wrong numbers.
    assert_eq!(quote_0_to_1.amount_out, U256::from(1_998_u64));
    assert_eq!(quote_1_to_0.amount_out, U256::from(499_u64));

    println!("\nboth quotes match expected output — example verified.");
}
