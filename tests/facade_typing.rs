//! MANAGER-AUTHORED acceptance tests for the facade typing pass: revm and
//! `anyhow` are driven out of the public surface.
//!
//! - `AdapterCache::call_raw` yields a crate-owned `CallOutcome` (not
//!   `revm::ExecutionResult`), and the fallible cache methods return a crate
//!   `CacheError` (not `anyhow::Result`).
//! - `AdapterDriver::apply_log` returns a *structured* `DriverError` carrying the
//!   adapter's `AdapterEventError` (previously it stringified the error into an
//!   `anyhow::Error`, discarding the structure).
//! - `quote_via_call` is the public quote helper for custom adapters, mapping a
//!   revert/halt to `SimError::Reverted`.
//!
//! All types are imported from `evm_amm_state::adapters` (the crate re-export),
//! so this file is stable whether those names resolve to upstream types or the
//! crate-owned mirrors introduced later. The implementation agent must make these
//! pass WITHOUT modifying them.

use std::sync::Arc;

use alloy_primitives::{Address, B256, Bytes, Log, LogData, U256};

use evm_amm_state::adapters::{
    AdapterCache, AdapterDriver, AdapterEventError, AdapterEventResult, AdapterRegistry,
    AmmAdapter, CacheError, CallOutcome, DriverError, EventSource, PoolKey, PoolRegistration,
    ProtocolId, SimError, SlotChange, StateDiff, StateUpdate, StateView, quote_via_call,
    quote_via_call_from,
};

/// Minimal `AdapterCache` whose `call_raw` returns a caller-chosen `CallOutcome`.
/// Only `storage` is a required `StateView` method; the rest are trivial.
struct MockCache {
    call: CallOutcome,
}

impl StateView for MockCache {
    fn storage(&self, _address: Address, _slot: U256) -> Option<U256> {
        None
    }
}

impl AdapterCache for MockCache {
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
        Ok(self.call.clone())
    }
}

/// Cache that records the `from` (`msg.sender`) its `call_raw` was invoked with,
/// so the quote helpers' sender threading can be asserted.
struct CapturingCache {
    from: std::cell::Cell<Address>,
}

impl StateView for CapturingCache {
    fn storage(&self, _address: Address, _slot: U256) -> Option<U256> {
        None
    }
}

impl AdapterCache for CapturingCache {
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
        from: Address,
        _to: Address,
        _calldata: Bytes,
        _commit: bool,
    ) -> Result<CallOutcome, CacheError> {
        self.from.set(from);
        Ok(CallOutcome::Success {
            output: Bytes::new(),
            gas_used: 0,
        })
    }
}

/// An adapter that always returns a structured decode error.
struct ErringAdapter;

impl AmmAdapter for ErringAdapter {
    fn protocol(&self) -> ProtocolId {
        ProtocolId::UniswapV2
    }
    fn decode_event(
        &self,
        _pool: &PoolRegistration,
        _log: &Log,
        _view: &dyn StateView,
    ) -> AdapterEventResult {
        AdapterEventResult::error(AdapterEventError::MalformedLog("boom"))
    }
}

#[test]
fn call_outcome_success_exposes_output() {
    let oc = CallOutcome::Success {
        output: Bytes::from(vec![1, 2, 3]),
        gas_used: 100,
    };
    assert_eq!(oc.into_success_output(), Some(Bytes::from(vec![1, 2, 3])));

    let reverted = CallOutcome::Revert {
        output: Bytes::new(),
        gas_used: 5,
    };
    assert_eq!(reverted.into_success_output(), None);
}

#[test]
fn quote_via_call_is_public_and_maps_revert_to_sim_error() {
    let mut cache = MockCache {
        call: CallOutcome::Revert {
            output: Bytes::new(),
            gas_used: 0,
        },
    };
    let err = quote_via_call(&mut cache, Address::ZERO, Bytes::new()).unwrap_err();
    assert!(matches!(err, SimError::Reverted));
}

#[test]
fn quote_via_call_from_threads_the_sender() {
    // `quote_via_call` runs the quote as the ZERO sender.
    let mut cache = CapturingCache {
        from: std::cell::Cell::new(Address::repeat_byte(0xee)),
    };
    quote_via_call(&mut cache, Address::repeat_byte(0x01), Bytes::new()).unwrap();
    assert_eq!(cache.from.get(), Address::ZERO);

    // `quote_via_call_from` runs it as the given sender — this is what every
    // adapter wires from `SimConfig::from`.
    let sender = Address::repeat_byte(0x99);
    let mut cache = CapturingCache {
        from: std::cell::Cell::new(Address::ZERO),
    };
    quote_via_call_from(&mut cache, sender, Address::repeat_byte(0x01), Bytes::new()).unwrap();
    assert_eq!(cache.from.get(), sender);
}

#[test]
fn quote_via_call_returns_success_output() {
    let payload = Bytes::from(vec![0xaa, 0xbb]);
    let mut cache = MockCache {
        call: CallOutcome::Success {
            output: payload.clone(),
            gas_used: 21_000,
        },
    };
    let out = quote_via_call(&mut cache, Address::ZERO, Bytes::new()).expect("success");
    assert_eq!(out, payload);
}

#[test]
fn apply_log_returns_structured_driver_error() {
    let emitter = Address::repeat_byte(0x11);
    let topic = B256::repeat_byte(0x22);
    let pool = PoolKey::UniswapV2(Address::repeat_byte(0x33));

    let registration =
        PoolRegistration::new(pool).with_event_source(EventSource::direct(emitter, vec![topic]));

    let mut registry = AdapterRegistry::new();
    registry.register_adapter(Arc::new(ErringAdapter)).unwrap();
    registry.register_pool(registration).unwrap();
    let driver = AdapterDriver::new(registry);

    let mut cache = MockCache {
        call: CallOutcome::Halt {
            reason: String::new(),
        },
    };
    let log = Log {
        address: emitter,
        data: LogData::new_unchecked(vec![topic], Bytes::new()),
    };

    let err = driver.apply_log(&mut cache, &log).unwrap_err();
    match err {
        DriverError::Decode { error, .. } => {
            assert_eq!(error, AdapterEventError::MalformedLog("boom"));
        }
        other => panic!("expected DriverError::Decode, got {other:?}"),
    }
}

#[test]
fn errors_preserve_their_source_chain() {
    // The boxed cause survives un-flattened and is reachable via source().
    let cache_err = CacheError::Backend("backend blew up".into());
    let source = std::error::Error::source(&cache_err).expect("CacheError keeps its cause");
    assert_eq!(source.to_string(), "backend blew up");

    // DriverError::Decode chains the structured adapter error as its source.
    let driver_err = DriverError::Decode {
        protocol: ProtocolId::UniswapV2,
        error: AdapterEventError::MalformedLog("boom"),
    };
    let source = std::error::Error::source(&driver_err).expect("DriverError keeps its cause");
    assert_eq!(source.to_string(), "malformed log: boom");

    // SimError::Execution carries the boxed cache error un-flattened (not a
    // stringified copy): source() exposes it and it downcasts to the typed
    // CacheError, so a consumer can distinguish an execution failure's cause.
    let sim_err = SimError::Execution(Box::new(CacheError::Backend("call_raw failed".into())));
    let source = std::error::Error::source(&sim_err).expect("SimError::Execution keeps its cause");
    assert!(
        source.downcast_ref::<CacheError>().is_some(),
        "the execution cause downcasts to the typed CacheError"
    );
    assert_eq!(source.to_string(), "cache backend error: call_raw failed");
}
