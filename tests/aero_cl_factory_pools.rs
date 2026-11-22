//! Factory-admitted Aero CL pools: `ClFactorySpec::aero_cl` discovery, CREATE2
//! admission to the reviewed Aero CL runtime surface, and governance routing,
//! next to the address-pinned families that keep their behaviour.

use std::collections::HashMap;

use alloy_primitives::{Address, B256, Bytes, Log, LogData, U256, address, b256};
use alloy_sol_types::SolEvent;
use evm_amm_state::adapters::factory::derive;
use evm_amm_state::adapters::storage::{SlipstreamLayoutFamily, V3StorageLayout};
use evm_amm_state::adapters::{
    AdapterCache, AdapterEventContext, AdapterRegistry, AmmAdapter, CacheError, CallOutcome,
    ClFactorySpec, ConcentratedLiquidityAdapter, ConcentratedLiquidityFactory, CreationLogContext,
    DiscoveredPool, PoolDiscovery, PoolFactory, PoolKey, PoolQuery, PoolRegistration, ProtocolId,
    ProtocolMetadata, SlotChange, StateDiff, StateUpdate, StateView,
    V3LiquidityTransitionCapability, V3Metadata, V3QuoterKind, V3SwapTransitionCapability,
};

const BASE_FACTORY: Address = address!("9cb220581b91b0c83223012fe69688b92ad07a76");
const METAQUOTER: Address = address!("1ddfec5058180c207f55bb77d95acdd0d8227868");
const HOOK: Address = address!("c0ed888afe87ed1acf01d9f19bb850159e3ce434");
const WETH: Address = address!("4200000000000000000000000000000000000006");
const USDC: Address = address!("833589fcd6edb6e08f4c7c32d4f71b54bda02913");
const WETH_USDC_100: Address = address!("2a430bebd2082d8e05d56d64dede8944279bbc24");
const GAUGE: Address = address!("00000000000000000000000000000000000000aa");
const PROXY_RUNTIME: B256 =
    b256!("55e3735acd886d76160bfd959db31dd476e73e46e8a0d70ac36e4c53ef5a2a45");
const GAUGE_PROXY_RUNTIME: B256 =
    b256!("b03fb822ffeb7154c5d94f0206d76b417f4f97b2b0a744e07e289cfb5fd0c639");
const BLOCK_HASH: B256 = b256!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
const PARENT_HASH: B256 = b256!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
const TX_HASH: B256 = b256!("cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc");

alloy_sol_types::sol! {
    event PoolCreated(address indexed token0, address indexed token1, int24 indexed tickSpacing, address pool);
    event CustomFeeSet(address indexed pool, uint24 fee);
}

#[derive(Default)]
struct StorageCache {
    storage: HashMap<(Address, U256), U256>,
    batch_reads: usize,
}

impl StateView for StorageCache {
    fn storage(&self, address: Address, slot: U256) -> Option<U256> {
        self.storage.get(&(address, slot)).copied()
    }
}

impl AdapterCache for StorageCache {
    fn cached_storage(&self, address: Address, slot: U256) -> Option<U256> {
        self.storage(address, slot)
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
    fn read_storage_slot(&mut self, address: Address, slot: U256) -> Result<U256, CacheError> {
        Ok(self.storage(address, slot).unwrap_or_default())
    }
    fn read_storage_slots(&mut self, slots: &[(Address, U256)]) -> Result<Vec<U256>, CacheError> {
        self.batch_reads += 1;
        Ok(slots
            .iter()
            .map(|(address, slot)| self.storage(*address, *slot).unwrap_or_default())
            .collect())
    }
    fn call_raw(
        &mut self,
        _from: Address,
        _to: Address,
        _calldata: Bytes,
        _commit: bool,
    ) -> Result<CallOutcome, CacheError> {
        Ok(CallOutcome::Halt {
            reason: "storage-only test cache".into(),
        })
    }
}

fn word(address: Address) -> U256 {
    U256::from_be_slice(address.as_slice())
}

fn slipstream_metadata(pool: &PoolRegistration) -> &V3Metadata {
    match &pool.metadata {
        ProtocolMetadata::Slipstream(metadata) => metadata,
        other => panic!("expected Slipstream metadata, got {other:?}"),
    }
}

fn discover(gauge: Option<Address>) -> (DiscoveredPool, usize) {
    let spec = ClFactorySpec::aero_cl(BASE_FACTORY);
    let mut cache = StorageCache::default();
    cache.storage.insert(
        (
            BASE_FACTORY,
            derive::v3_get_pool_slot_by_spacing(spec.get_pool_base_slot, WETH, USDC, 100),
        ),
        word(WETH_USDC_100),
    );
    if let Some(gauge) = gauge {
        cache
            .storage
            .insert((WETH_USDC_100, U256::from(3)), word(gauge));
    }
    let discovery = PoolDiscovery::new([
        Box::new(ConcentratedLiquidityFactory::new(spec)) as Box<dyn PoolFactory>
    ]);
    let mut found = discovery
        .find(&mut cache, PoolQuery::pair(WETH, USDC))
        .expect("Aero CL discovery succeeds");
    assert_eq!(found.len(), 1, "only the seeded spacing resolves");
    (found.remove(0), cache.batch_reads)
}

fn full_context(chain_id: u64) -> AdapterEventContext {
    AdapterEventContext::for_block(1_000, BLOCK_HASH, 1_792_000_000)
        .with_chain_id(chain_id)
        .with_parent_hash(PARENT_HASH)
        .with_transaction_hash(TX_HASH)
        .with_event_order(3, 7)
}

#[test]
fn aero_cl_preset_pins_the_verified_factory_shape() {
    let spec = ClFactorySpec::aero_cl(BASE_FACTORY);
    assert_eq!(spec.protocol, ProtocolId::Slipstream);
    assert_eq!(spec.get_pool_base_slot, U256::from(19));
    assert_eq!(spec.tick_spacings(), &[1, 10, 50, 100, 150, 200, 250]);
    assert_eq!(spec.slipstream_layout, SlipstreamLayoutFamily::AeroCl);
    assert_eq!(spec.quoter, Some(METAQUOTER));
    assert_eq!(spec.quoter_kind, V3QuoterKind::Metaquoter);
    assert!(spec.verify_derivations);
    assert_eq!(spec.creation_topic0, Some(PoolCreated::SIGNATURE_HASH));
    let create2 = spec.create2.expect("preset pins CREATE2");
    assert_eq!(
        derive::v3_pool_address_by_spacing(BASE_FACTORY, create2.init_code_hash, WETH, USDC, 100),
        WETH_USDC_100
    );
    assert_eq!(
        ClFactorySpec::slipstream(BASE_FACTORY).slipstream_layout,
        SlipstreamLayoutFamily::Legacy
    );
}

#[test]
fn discovery_registers_aero_cl_pools_with_gauge_in_one_batch() {
    let (found, batch_reads) = discover(Some(GAUGE));
    assert_eq!(batch_reads, 1, "gauge read joins the mapping batch");
    assert_eq!(found.key, PoolKey::Slipstream(WETH_USDC_100));
    let metadata = slipstream_metadata(&found.registration);
    assert_eq!(metadata.token0, Some(WETH));
    assert_eq!(metadata.token1, Some(USDC));
    assert_eq!(metadata.tick_spacing, Some(100));
    assert_eq!(metadata.fee, None);
    assert_eq!(metadata.factory, Some(BASE_FACTORY));
    assert_eq!(metadata.quoter, Some(METAQUOTER));
    assert_eq!(metadata.quoter_kind, V3QuoterKind::Metaquoter);
    assert_eq!(metadata.gauge, Some(GAUGE));
    assert_eq!(metadata.storage_layout, Some(V3StorageLayout::aero_cl(100)));
    assert!(
        found
            .registration
            .event_sources
            .iter()
            .any(|source| source.emitter == HOOK),
        "admitted pools subscribe the hook governance events"
    );

    let (gaugeless, _) = discover(None);
    assert_eq!(slipstream_metadata(&gaugeless.registration).gauge, None);
}

#[test]
fn creation_events_register_admissible_pools() {
    let factory = ConcentratedLiquidityFactory::new(ClFactorySpec::aero_cl(BASE_FACTORY));
    let event = PoolCreated {
        token0: WETH,
        token1: USDC,
        tickSpacing: alloy_primitives::aliases::I24::try_from(100).unwrap(),
        pool: WETH_USDC_100,
    };
    let log = Log {
        address: BASE_FACTORY,
        data: event.encode_log_data(),
    };
    let found = factory
        .decode_creation(&log, CreationLogContext::new(Some(5), Some(2)))
        .expect("decodes")
        .expect("Aero CL creation event registers a pool");
    assert_eq!(found.key, PoolKey::Slipstream(WETH_USDC_100));
    assert_eq!(
        slipstream_metadata(&found.registration).storage_layout,
        Some(V3StorageLayout::aero_cl(100))
    );
    assert!(ConcentratedLiquidityAdapter::is_aero_cl_factory_pool(
        &found.registration
    ));

    let forged = Log {
        address: BASE_FACTORY,
        data: PoolCreated {
            pool: Address::repeat_byte(0x77),
            ..event
        }
        .encode_log_data(),
    };
    let fresh = ConcentratedLiquidityFactory::new(ClFactorySpec::aero_cl(BASE_FACTORY));
    assert!(
        fresh
            .decode_creation(&forged, CreationLogContext::new(Some(5), Some(3)))
            .is_err(),
        "the first creation event cross-checks the CREATE2 derivation"
    );
    let unverified = factory
        .decode_creation(&forged, CreationLogContext::new(Some(5), Some(3)))
        .expect("later events skip the one-shot derivation check")
        .expect("decodes");
    assert!(
        !ConcentratedLiquidityAdapter::is_aero_cl_factory_pool(&unverified.registration),
        "admission re-derives every pool, so a non-derivable one stays generic"
    );
}

#[test]
fn admitted_pools_get_the_reviewed_aero_cl_surface() {
    let (found, _) = discover(Some(GAUGE));
    let pool = found.registration;
    let adapter = ConcentratedLiquidityAdapter::default();
    let context = full_context(8_453);

    assert!(ConcentratedLiquidityAdapter::is_aero_cl_factory_pool(&pool));
    assert_eq!(
        ConcentratedLiquidityAdapter::aero_cl_runtime_family(&pool),
        None
    );
    assert_eq!(
        ConcentratedLiquidityAdapter::swap_transition_capability_with_context(&pool, &context),
        V3SwapTransitionCapability::Exact
    );
    assert_eq!(
        ConcentratedLiquidityAdapter::liquidity_transition_capability_with_context(&pool, &context),
        V3LiquidityTransitionCapability::Exact
    );
    assert_eq!(
        ConcentratedLiquidityAdapter::swap_transition_capability_with_context(
            &pool,
            &full_context(1)
        ),
        V3SwapTransitionCapability::Unsupported,
        "a Base factory pool never replays under another chain"
    );

    let identities = adapter.reviewed_code_identities(&pool);
    assert!(identities.contains(&(WETH_USDC_100, PROXY_RUNTIME)));
    assert!(identities.contains(&(GAUGE, GAUGE_PROXY_RUNTIME)));
    assert!(
        identities
            .iter()
            .any(|(address, _)| *address == BASE_FACTORY)
    );
    let targets = adapter.verified_storage_targets(&pool);
    assert!(targets.iter().any(|(address, _)| *address == BASE_FACTORY));
    assert!(targets.iter().any(|(address, _)| *address == HOOK));
    let overrides = ConcentratedLiquidityAdapter::aero_cl_quote_code_overrides(&pool);
    assert!(overrides.iter().any(|(address, _)| *address == GAUGE));
    assert_eq!(
        ConcentratedLiquidityAdapter::aero_cl_block_scoped_storage_targets(&pool, 7).len(),
        1
    );

    let (gaugeless, _) = discover(None);
    let identities = adapter.reviewed_code_identities(&gaugeless.registration);
    assert!(identities.contains(&(WETH_USDC_100, PROXY_RUNTIME)));
    assert!(
        !identities
            .iter()
            .any(|(_, hash)| *hash == GAUGE_PROXY_RUNTIME)
    );
    assert_eq!(
        ConcentratedLiquidityAdapter::aero_cl_quote_code_overrides(&gaugeless.registration).len(),
        1,
        "only the pool tape is stubbed without a gauge"
    );
}

#[test]
fn unproven_registrations_stay_on_the_generic_path() {
    let adapter = ConcentratedLiquidityAdapter::default();
    let context = full_context(8_453);
    let proven = V3Metadata::default()
        .with_token0(WETH)
        .with_token1(USDC)
        .with_tick_spacing(100)
        .with_factory(BASE_FACTORY)
        .with_storage_layout(V3StorageLayout::aero_cl(100));
    let unproven = [
        (Address::repeat_byte(0x55), proven.clone()),
        (
            WETH_USDC_100,
            proven.clone().with_factory(Address::repeat_byte(0x66)),
        ),
        (
            WETH_USDC_100,
            proven
                .clone()
                .with_tick_spacing(50)
                .with_storage_layout(V3StorageLayout::aero_cl(50)),
        ),
        (
            WETH_USDC_100,
            proven
                .clone()
                .with_storage_layout(V3StorageLayout::slipstream(100)),
        ),
    ];
    for (address, metadata) in unproven {
        let pool = PoolRegistration::new(PoolKey::Slipstream(address))
            .with_metadata(ProtocolMetadata::Slipstream(metadata));
        assert!(!ConcentratedLiquidityAdapter::is_aero_cl_factory_pool(
            &pool
        ));
        assert_eq!(
            ConcentratedLiquidityAdapter::swap_transition_capability_with_context(&pool, &context),
            V3SwapTransitionCapability::Unsupported
        );
        assert!(adapter.reviewed_code_identities(&pool).is_empty());
    }
}

#[test]
fn governance_events_route_to_admitted_pools() {
    let (found, _) = discover(Some(GAUGE));
    let mut registry = AdapterRegistry::new();
    registry
        .register_adapter(std::sync::Arc::new(ConcentratedLiquidityAdapter::default()))
        .unwrap();
    registry.register_pool(found.registration).unwrap();

    let fee_set = |pool: Address| Log {
        address: HOOK,
        data: CustomFeeSet {
            pool,
            fee: alloy_primitives::aliases::U24::from(250_u32),
        }
        .encode_log_data(),
    };
    assert_eq!(
        registry
            .route_log(&fee_set(WETH_USDC_100))
            .map(|pool| &pool.key),
        Some(&PoolKey::Slipstream(WETH_USDC_100))
    );
    assert!(
        registry
            .route_log(&fee_set(Address::repeat_byte(0x44)))
            .is_none(),
        "hook events for unregistered pools never route"
    );
    let unrelated = Log {
        address: Address::repeat_byte(0x99),
        data: LogData::new_unchecked(vec![CustomFeeSet::SIGNATURE_HASH], Bytes::new()),
    };
    assert!(registry.route_log(&unrelated).is_none());
}
