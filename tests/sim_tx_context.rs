//! Transaction-context plumbing: `SimConfig::gas_price` / `from` reach the EVM
//! as `tx.gasprice` / `tx.origin` through the live cache, through the offline
//! overlay helper (with runtime-code overrides), and fail closed on a cache
//! facade that cannot honour them.

use std::sync::Arc;

use alloy_primitives::{Address, Bytes, U256, address, hex};
use alloy_provider::{RootProvider, network::AnyNetwork};
use alloy_rpc_client::RpcClient;
use alloy_transport::mock::Asserter;
use anyhow::Result;
use evm_amm_state::adapters::{
    AdapterCache, CacheError, CallOutcome, CallTxContext, SimConfig, SlotChange, StateDiff,
    StateUpdate, StateView, overlay_call_raw_with_context, quote_via_call_with_config,
};
use evm_fork_cache::cache::{EvmCache, EvmOverlay};
use revm::state::{AccountInfo, Bytecode};

const ECHO: Address = address!("00000000000000000000000000000000000000e1");
const ORIGIN: Address = address!("1111111111111111111111111111111111111111");

fn echo_runtime() -> Bytes {
    Bytes::from(
        hex::decode(include_str!("fixtures/mock_tx_context_echo_runtime.hex").trim()).unwrap(),
    )
}

async fn cache() -> (EvmCache, Asserter) {
    let asserter = Asserter::new();
    let client = RpcClient::mocked(asserter.clone());
    let provider = RootProvider::<AnyNetwork>::new(client);
    let mut cache = EvmCache::new(Arc::new(provider)).await;
    cache.set_chain_id(10);
    cache.set_block_context(Some(158_800_699), Some(308));
    for account in [Address::ZERO, ORIGIN] {
        cache
            .db_mut()
            .insert_account_info(account, AccountInfo::default());
    }
    let code = Bytecode::new_raw(echo_runtime());
    let code_hash = code.hash_slow();
    cache.db_mut().insert_account_info(
        ECHO,
        AccountInfo {
            balance: U256::ZERO,
            nonce: 1,
            code: Some(code),
            code_hash,
            account_id: None,
        },
    );
    (cache, asserter)
}

fn decode(output: &[u8]) -> (U256, Address, U256) {
    assert_eq!(output.len(), 96);
    (
        U256::from_be_slice(&output[..32]),
        Address::from_slice(&output[44..64]),
        U256::from_be_slice(&output[64..96]),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn live_cache_honours_gas_price_and_origin() -> Result<()> {
    let (mut cache, asserter) = cache().await;
    let config = SimConfig::default()
        .with_tx_origin(ORIGIN)
        .with_effective_gas_price(308, 1_100_000_000);
    assert_eq!(config.tx_origin(), ORIGIN);
    assert_eq!(config.gas_price, Some(1_100_000_308));
    let output = quote_via_call_with_config(&mut cache, &config, ECHO, Bytes::new())
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    let (gas_price, origin, basefee) = decode(&output);
    assert_eq!(gas_price, U256::from(1_100_000_308_u64));
    assert_eq!(origin, ORIGIN);
    assert_eq!(basefee, U256::from(308_u64));

    // Without a configured gas price the call runs exactly as before: zero.
    let output = quote_via_call_with_config(&mut cache, &SimConfig::default(), ECHO, Bytes::new())
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    let (gas_price, origin, _) = decode(&output);
    assert_eq!(gas_price, U256::ZERO);
    assert_eq!(origin, Address::ZERO);
    assert!(asserter.read_q().is_empty());
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn offline_overlay_honours_context_and_overrides_together() -> Result<()> {
    let (mut cache, _asserter) = cache().await;
    let snapshot = cache.snapshot();
    let mut overlay = EvmOverlay::new(snapshot, None);
    let target = address!("00000000000000000000000000000000000000e2");
    // `target` has no code in the snapshot; the override installs the echo
    // runtime for this call only.
    let context = CallTxContext::new().with_gas_price(2_000_000_308);
    let outcome = overlay_call_raw_with_context(
        &mut overlay,
        ORIGIN,
        target,
        Bytes::new(),
        &[(target, echo_runtime())],
        &context,
    )
    .map_err(|error| anyhow::anyhow!("{error}"))?;
    let CallOutcome::Success { output, .. } = outcome else {
        panic!("echo call must succeed");
    };
    let (gas_price, origin, basefee) = decode(&output);
    assert_eq!(gas_price, U256::from(2_000_000_308_u64));
    assert_eq!(origin, ORIGIN);
    assert_eq!(basefee, U256::from(308_u64));
    // The override did not survive the call.
    let plain = overlay
        .call_raw(ORIGIN, target, Bytes::new())
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    assert!(
        matches!(plain, revm::context::result::ExecutionResult::Success { ref output, .. } if output.data().is_empty()),
        "{plain:?}"
    );
    Ok(())
}

/// A facade that implements only the historical surface.
struct LegacyFacade;

impl StateView for LegacyFacade {
    fn storage(&self, _address: Address, _slot: U256) -> Option<U256> {
        None
    }
}

impl AdapterCache for LegacyFacade {
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
        Ok(CallOutcome::Success {
            output: Bytes::from_static(&[0x01]),
            gas_used: 0,
        })
    }
}

#[test]
fn a_legacy_facade_fails_closed_on_a_gas_price_it_cannot_honour() {
    let mut facade = LegacyFacade;
    let default = SimConfig::default();
    assert!(default.tx_context().is_default());
    let output = quote_via_call_with_config(&mut facade, &default, ECHO, Bytes::new())
        .expect("default context delegates to the historical call");
    assert_eq!(output.as_ref(), &[0x01]);
    let priced = SimConfig::default().with_gas_price(1);
    let error = quote_via_call_with_config(&mut facade, &priced, ECHO, Bytes::new())
        .expect_err("a gas price must not be silently dropped");
    assert!(error.to_string().contains("tx.gasprice"), "{error}");
}
