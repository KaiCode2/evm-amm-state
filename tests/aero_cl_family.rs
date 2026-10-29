//! Deterministic coverage of the reviewed Aero (Dromos) CL families: layout
//! classification, capability gating, reviewed identity/warm-cell tables, the
//! Metaquoter quote path, and exact event replay of real Ethereum and Base
//! transactions captured as fixtures (`tests/fixtures/*aero_cl*.json`, built
//! from `debug_traceTransaction` pre/post state).

use std::{collections::BTreeMap, str::FromStr, sync::Arc};

use alloy_primitives::{Address, B256, Bytes, Log, U256, address, b256, hex};
use alloy_provider::{RootProvider, network::AnyNetwork};
use alloy_rpc_client::RpcClient;
use alloy_transport::mock::Asserter;
use evm_amm_state::adapters::storage::{
    AERO_CL_OBSERVATIONS_SLOT, AERO_CL_SECONDS_PER_STAKED_LIQUIDITY_SLOT,
    AERO_CL_STAKED_LIQUIDITY_SLOT, SlipstreamLayoutFamily, V3StorageLayout,
    slipstream_tick_info_storage_keys_with_base, v3_tick_bitmap_storage_key_with_base,
    v3_word_position,
};
use evm_amm_state::adapters::v3_sync::V3SyncSpec;
use evm_amm_state::adapters::{
    AdapterEventContext, AdapterRegistry, AmmAdapter, ColdStartOutcome, ColdStartPolicy,
    ConcentratedLiquidityAdapter, PoolKey, PoolRegistration, ProtocolMetadata, PurgeScope,
    SimConfig, SlipstreamFeeEvidenceError, SlipstreamRuntimeFamily, SlipstreamSnapshotIdentity,
    SlipstreamSwapFeeEvidence, SlipstreamUnstakedFeeEvaluationError, SlipstreamUnstakedFeeProof,
    StateUpdate, StateView, UnsupportedReason, UpdateQuality, V3LiquidityTransitionCapability,
    V3Metadata, V3QuoterKind, V3SwapTransitionCapability,
};
use evm_fork_cache::cache::EvmCache;
use revm::state::{AccountInfo, Bytecode};
use serde_json::Value;

const METAQUOTER: Address = address!("1ddfec5058180c207f55bb77d95acdd0d8227868");
const ETH_POOL: Address = address!("ff35aa95669b5668ac6727fdcfc0d5582cdd78d1");
const BASE_POOL: Address = address!("eebe78d734dc4bac027061cee9bc12c5803fe4f8");
const LEGACY_BASE_POOL: Address = address!("b378137c90444bbcecd44a1f766851fbf53d2a9e");
const BLOCK_HASH: B256 = b256!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
const PARENT_HASH: B256 = b256!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
const TX_HASH: B256 = b256!("cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc");
const WORD_128_MASK: U256 = U256::from_limbs([u64::MAX, u64::MAX, 0, 0]);

#[derive(Clone, Default)]
struct FixtureState(BTreeMap<(Address, U256), U256>);

impl StateView for FixtureState {
    fn storage(&self, address: Address, slot: U256) -> Option<U256> {
        self.0.get(&(address, slot)).copied()
    }
}

fn word(value: &str) -> U256 {
    U256::from_str(value).expect("valid checked-in word")
}

fn aero_registration(pool: Address, spacing: i32) -> PoolRegistration {
    PoolRegistration::new(PoolKey::Slipstream(pool))
        .with_state_address(pool)
        .with_metadata(ProtocolMetadata::Slipstream(
            V3Metadata::default()
                .with_tick_spacing(spacing)
                .with_storage_layout(V3StorageLayout::aero_cl(spacing))
                .with_metaquoter(METAQUOTER),
        ))
}

fn full_context(chain_id: u64) -> AdapterEventContext {
    AdapterEventContext::for_block(1_000, BLOCK_HASH, 1_792_000_000)
        .with_chain_id(chain_id)
        .with_parent_hash(PARENT_HASH)
        .with_transaction_hash(TX_HASH)
        .with_event_order(3, 7)
}

#[test]
fn aero_layouts_grant_capability_only_to_the_reviewed_pools() {
    for (pool, spacing, chain) in [(ETH_POOL, 50, 1_u64), (BASE_POOL, 100, 8_453)] {
        let registration = aero_registration(pool, spacing);
        let context = full_context(chain);
        assert_eq!(
            ConcentratedLiquidityAdapter::swap_transition_capability_with_context(
                &registration,
                &context
            ),
            V3SwapTransitionCapability::Exact,
            "reviewed Aero pool with the Aero layout must be exact"
        );
        assert_eq!(
            ConcentratedLiquidityAdapter::liquidity_transition_capability_with_context(
                &registration,
                &context
            ),
            V3LiquidityTransitionCapability::Exact
        );
        assert_eq!(
            ConcentratedLiquidityAdapter::swap_transition_capability(&registration),
            V3SwapTransitionCapability::Unsupported,
            "capability stays event-scoped (needs complete context)"
        );
        assert_eq!(
            ConcentratedLiquidityAdapter::aero_cl_runtime_family(&registration),
            Some(if chain == 1 {
                SlipstreamRuntimeFamily::AeroEthereumBifi
            } else {
                SlipstreamRuntimeFamily::AeroBaseMooBifi
            })
        );

        // Wrong chain, missing lineage, wrong spacing, legacy layout: closed.
        let wrong_chain = full_context(if chain == 1 { 8_453 } else { 1 });
        assert_eq!(
            ConcentratedLiquidityAdapter::swap_transition_capability_with_context(
                &registration,
                &wrong_chain
            ),
            V3SwapTransitionCapability::Unsupported
        );
        let partial = AdapterEventContext::for_block(1_000, BLOCK_HASH, 1).with_chain_id(chain);
        assert_eq!(
            ConcentratedLiquidityAdapter::swap_transition_capability_with_context(
                &registration,
                &partial
            ),
            V3SwapTransitionCapability::Unsupported
        );
        let wrong_spacing = aero_registration(pool, spacing * 2);
        assert_eq!(
            ConcentratedLiquidityAdapter::swap_transition_capability_with_context(
                &wrong_spacing,
                &context
            ),
            V3SwapTransitionCapability::Unsupported
        );
        assert_eq!(
            ConcentratedLiquidityAdapter::aero_cl_runtime_family(&wrong_spacing),
            None
        );
        let legacy_layout = PoolRegistration::new(PoolKey::Slipstream(pool)).with_metadata(
            ProtocolMetadata::Slipstream(
                V3Metadata::default()
                    .with_tick_spacing(spacing)
                    .with_storage_layout(V3StorageLayout::slipstream(spacing)),
            ),
        );
        assert_eq!(
            ConcentratedLiquidityAdapter::swap_transition_capability_with_context(
                &legacy_layout,
                &context
            ),
            V3SwapTransitionCapability::Unsupported,
            "the legacy slot surface must never replay an Aero pool"
        );
        assert_eq!(
            ConcentratedLiquidityAdapter::liquidity_transition_capability_with_context(
                &legacy_layout,
                &context
            ),
            V3LiquidityTransitionCapability::Unsupported
        );
        // A pinned ring that contradicts the family is not the Aero layout.
        let contradicting_ring = PoolRegistration::new(PoolKey::Slipstream(pool)).with_metadata(
            ProtocolMetadata::Slipstream(
                V3Metadata::default()
                    .with_tick_spacing(spacing)
                    .with_storage_layout(
                        V3StorageLayout::aero_cl(spacing).with_observations_slot(U256::from(20)),
                    ),
            ),
        );
        assert_eq!(
            ConcentratedLiquidityAdapter::swap_transition_capability_with_context(
                &contradicting_ring,
                &context
            ),
            V3SwapTransitionCapability::Unsupported
        );
    }

    // The Aero layout on a legacy reviewed pool is closed, and the legacy pool
    // keeps its legacy capability untouched.
    let aero_on_legacy = aero_registration(LEGACY_BASE_POOL, 200);
    assert_eq!(
        ConcentratedLiquidityAdapter::swap_transition_capability_with_context(
            &aero_on_legacy,
            &full_context(8_453)
        ),
        V3SwapTransitionCapability::Unsupported
    );
    let legacy = PoolRegistration::new(PoolKey::Slipstream(LEGACY_BASE_POOL)).with_metadata(
        ProtocolMetadata::Slipstream(
            V3Metadata::default()
                .with_tick_spacing(200)
                .with_storage_layout(V3StorageLayout::slipstream(200)),
        ),
    );
    assert_eq!(
        ConcentratedLiquidityAdapter::swap_transition_capability_with_context(
            &legacy,
            &full_context(8_453)
        ),
        V3SwapTransitionCapability::Exact
    );
    assert_eq!(
        ConcentratedLiquidityAdapter::aero_cl_runtime_family(&legacy),
        None
    );
    // An unreviewed Aero-layout pool anywhere is closed.
    let unknown = aero_registration(Address::repeat_byte(0x55), 50);
    assert_eq!(
        ConcentratedLiquidityAdapter::swap_transition_capability_with_context(
            &unknown,
            &full_context(1)
        ),
        V3SwapTransitionCapability::Unsupported
    );
}

#[test]
fn aero_families_pin_their_identities() {
    for family in [
        SlipstreamRuntimeFamily::AeroEthereumBifi,
        SlipstreamRuntimeFamily::AeroBaseMooBifi,
    ] {
        assert!(family.is_aero_cl());
        let identities = ConcentratedLiquidityAdapter::aero_cl_runtime_identities(family);
        assert!(
            identities.len() >= 9,
            "every pinned identity must be listed"
        );
        assert!(identities.contains(&(
            address!("1642634c8cb1c9f1b84c9a1a08534f8c3f4e89f8"),
            b256!("041172c7cbffc45627f9dd52727a0124cd6eeb2b5830ab392df54f71a6948693")
        )));
    }
    assert_eq!(SlipstreamRuntimeFamily::AeroEthereumBifi.chain_id(), 1);
    assert_eq!(SlipstreamRuntimeFamily::AeroBaseMooBifi.chain_id(), 8_453);
    assert!(!SlipstreamRuntimeFamily::AerodromeBaseBifi.is_aero_cl());
    assert!(
        ConcentratedLiquidityAdapter::aero_cl_runtime_identities(
            SlipstreamRuntimeFamily::AerodromeBaseBifi
        )
        .is_empty()
    );
}

#[test]
fn aero_evidence_fails_closed_everywhere() {
    // Build evidence for a legacy family (the only way to obtain a proof), then
    // check that re-labelling it as an Aero family is rejected at every gate.
    let identity = SlipstreamSnapshotIdentity::new(
        8_453,
        1_000,
        BLOCK_HASH,
        PARENT_HASH,
        1_792_000_000,
        TX_HASH,
        3,
        7,
    )
    .expect("complete identity");
    let proof = evidence_proof_for_tests(identity);
    let evidence = SlipstreamSwapFeeEvidence::new(
        SlipstreamRuntimeFamily::AeroBaseMooBifi,
        8_453,
        BASE_POOL,
        address!("9cb220581b91b0c83223012fe69688b92ad07a76"),
        b256!("55e3735acd886d76160bfd959db31dd476e73e46e8a0d70ac36e4c53ef5a2a45"),
        address!("1642634c8cb1c9f1b84c9a1a08534f8c3f4e89f8"),
        b256!("041172c7cbffc45627f9dd52727a0124cd6eeb2b5830ab392df54f71a6948693"),
        10_000,
        0,
        proof,
        1_000,
        BLOCK_HASH,
        PARENT_HASH,
        1_792_000_000,
        TX_HASH,
        3,
        7,
    );
    assert_eq!(
        evidence.map(|_| ()),
        Err(SlipstreamFeeEvidenceError::UnsupportedFamily)
    );
    let snapshot_cache = offline_cache(8_453);
    assert_eq!(
        ConcentratedLiquidityAdapter::evaluate_slipstream_unstaked_fee(
            SlipstreamRuntimeFamily::AeroBaseMooBifi,
            Arc::clone(&snapshot_cache),
            identity,
            &full_context(8_453),
        )
        .map(|_| ()),
        Err(SlipstreamUnstakedFeeEvaluationError::UnsupportedFamily)
    );
    assert_eq!(
        ConcentratedLiquidityAdapter::evaluate_slipstream_all_staked_candidate(
            SlipstreamRuntimeFamily::AeroEthereumBifi,
            snapshot_cache,
            identity,
            &full_context(8_453),
        )
        .map(|_| ()),
        Err(SlipstreamUnstakedFeeEvaluationError::UnsupportedFamily)
    );
}

/// A legacy all-staked attestation, obtained the only way the crate allows:
/// through the evaluator over a snapshot carrying the legacy Base runtimes.
fn evidence_proof_for_tests(identity: SlipstreamSnapshotIdentity) -> SlipstreamUnstakedFeeProof {
    let rt = tokio::runtime::Runtime::new().expect("runtime");
    rt.block_on(async {
        let asserter = Asserter::new();
        let client = RpcClient::mocked(asserter);
        let provider = RootProvider::<AnyNetwork>::new(client);
        let mut cache = EvmCache::new(Arc::new(provider)).await;
        cache.set_chain_id(8_453);
        cache.set_block(alloy_eips::BlockId::from((BLOCK_HASH, Some(true))));
        cache.set_timestamp(Some(1_792_000_000));
        cache.set_block_context(Some(1_000), Some(0));
        cache
            .db_mut()
            .cache
            .block_hashes
            .insert(U256::from(999_u64), PARENT_HASH);
        install(
            &mut cache,
            LEGACY_BASE_POOL,
            runtime(include_str!("fixtures/base_slipstream_proxy_runtime.hex")),
        );
        install(
            &mut cache,
            address!("ec8e5342b19977b4ef8892e02d8daecfa1315831"),
            runtime(include_str!(
                "fixtures/base_slipstream_implementation_runtime.hex"
            )),
        );
        let evaluation = ConcentratedLiquidityAdapter::evaluate_slipstream_all_staked_candidate(
            SlipstreamRuntimeFamily::AerodromeBaseBifi,
            cache.snapshot(),
            identity,
            &full_context(8_453),
        )
        .expect("legacy all-staked candidate");
        evaluation.proof()
    })
}

fn offline_cache(chain_id: u64) -> Arc<evm_fork_cache::cache::EvmSnapshot> {
    let rt = tokio::runtime::Runtime::new().expect("runtime");
    rt.block_on(async {
        let asserter = Asserter::new();
        let client = RpcClient::mocked(asserter);
        let provider = RootProvider::<AnyNetwork>::new(client);
        let mut cache = EvmCache::new(Arc::new(provider)).await;
        cache.set_chain_id(chain_id);
        cache.snapshot()
    })
}

fn runtime(hex_runtime: &str) -> Bytes {
    Bytes::from(hex::decode(hex_runtime.trim()).expect("checked-in runtime"))
}

fn install(cache: &mut EvmCache, address: Address, code: Bytes) {
    let code = Bytecode::new_raw(code);
    let code_hash = code.hash_slow();
    cache.db_mut().insert_account_info(
        address,
        AccountInfo {
            balance: U256::ZERO,
            nonce: 1,
            code: Some(code),
            code_hash,
            account_id: None,
        },
    );
}

#[test]
fn reviewed_aero_tables_match_the_traced_read_sets() {
    let adapter = ConcentratedLiquidityAdapter::default();

    // Ethereum: every cell below was observed in the `debug_traceCall`
    // prestate of the Metaquoter quote at block 26292020.
    let eth = aero_registration(ETH_POOL, 50);
    let eth_factory = address!("f4fd2e6c5b60c5b365cd1b96dd6245e6d60414c2");
    let hook = address!("c0ed888afe87ed1acf01d9f19bb850159e3ce434");
    let discount = address!("aa48aaef655fb7cd7acc04717f66022a4771a345");
    let unstaked_module = address!("cc421483cd131a8fd6b68c37e46f244dbec4f797");
    let tape = address!("27d5eb73e7406675f6b0cbd037490ec3dba935a4");
    let voter = address!("382409fad8503fbcdbb27a4211551fa03e884078");
    let registry = address!("dd1705e23762f4d6e44d8c31bd8ea80c54612239");
    let gauge_factory = address!("f437f62053acb121665b8fdbfea8f214fb8e589d");
    let targets = adapter.verified_storage_targets(&eth);
    let has = |address: Address, key: &str| {
        targets.contains(&(address, U256::from_be_slice(&hex::decode(key).unwrap())))
    };
    assert!(
        has(
            eth_factory,
            "93c4211483b3d8d0144a8852226a06f7b74bf3f4a220deeb9a1488e91b289aa7"
        ),
        "factory isPool[pool]"
    );
    assert!(
        has(
            eth_factory,
            "e5787abfa7faedfd28bf59a93e12ab60a9e92470075e75a629e2d4fcba9447e6"
        ),
        "factory poolSwapHook[pool]"
    );
    assert!(
        has(
            eth_factory,
            "aa9f2c1e5115341abf14ad927886306207dcf01beab77ebdfe0e588e834ce55a"
        ),
        "factory tickSpacingToFee[50]"
    );
    for slot in [4_u64, 8, 9, 11, 12, 13] {
        assert!(
            targets.contains(&(eth_factory, U256::from(slot))),
            "factory slot {slot}"
        );
    }
    for slot in [0_u64, 1, 2, 5] {
        assert!(
            targets.contains(&(hook, U256::from(slot))),
            "hook slot {slot}"
        );
    }
    assert!(
        has(
            hook,
            "c48b2ff4992ca57ad80447887d182f4b7b2a6e0861bb0d57be8e3ce86be3125c"
        ),
        "hook dynamicFeeConfig[pool]"
    );
    assert!(
        has(
            discount,
            "aee3e21c37db70f10ee8a0207cf6ab8c05bea9443d2c374482db430f85cf974b"
        ),
        "discount registry discounts[metaquoter]"
    );
    assert!(
        has(
            unstaked_module,
            "8938ec10e2bcf3d1beb8854f630c6741712874b36d30340799997aaebe30b675"
        ),
        "unstaked fee module customFee[pool]"
    );
    assert!(
        has(
            voter,
            "74dc530c4a0e2098eb0ae6a7c594e6cf9ff5b59922b867a8892034a185794e68"
        ),
        "voter gauges[gauge] liveness word"
    );
    assert!(
        has(
            registry,
            "c3e3839b182ed7b084cfe3ce94c87ca82ddaef53609b35b211981813ce90c9a5"
        ),
        "registry targetToFactory[pool]"
    );
    assert!(
        has(
            registry,
            "93c4211483b3d8d0144a8852226a06f7b74bf3f4a220deeb9a1488e91b289aa7"
        ),
        "registry poolToGauge[pool]"
    );
    assert!(
        has(
            registry,
            "0d8b0cd2694e4a007304042a5a95dd5f47ffed48579cb6596af561df025880e5"
        ),
        "registry gaugeToFactory[gauge]"
    );
    assert!(
        has(
            gauge_factory,
            "7ed2f97974f81330e23c337266d40ba1eea858b9270d93098c81f0bd3772779c"
        ),
        "gauge factory per-gauge cell"
    );
    assert!(
        has(
            tape,
            "9177c05f61155d20bddc0c82f3bf8c9d0d86f58f22e922591a9ea8e3f78b1582"
        ),
        "tape hook word"
    );
    for suffix in ["e4", "e5", "e6", "e7", "e8"] {
        assert!(
            has(
                tape,
                &format!("918176cf8c07642da7910eb5e3695ab66aed8cb3f44f74805378b0305958fa{suffix}")
            ),
            "tape record word {suffix}"
        );
    }
    // No MEV module on Ethereum: no module cells.
    assert!(
        !targets
            .iter()
            .any(|(address, _)| *address == address!("29f12ce5be3d88b56d245b7f4e969c518f799a4a"))
    );
    // The block-keyed hook cell and the origin's discount cell are exposed
    // through helpers rather than cold start.
    assert_eq!(
        ConcentratedLiquidityAdapter::aero_cl_block_scoped_storage_targets(&eth, 26_292_020),
        vec![(
            hook,
            U256::from_be_slice(
                &hex::decode("b1a2bef3921ba25664fb82f0344b9303425e1666ac068e81957abc8e1ca9fd70")
                    .unwrap()
            )
        )]
    );
    assert_eq!(
        ConcentratedLiquidityAdapter::aero_cl_discount_storage_target(
            &eth,
            address!("1111111111111111111111111111111111111111")
        ),
        Some((
            discount,
            U256::from_be_slice(
                &hex::decode("06bb1b9bc4293ba066a12274418b7ea4df183c2e4e6b39591987369520ca3956")
                    .unwrap()
            )
        ))
    );
    // Without the Metaquoter registered, its registry/discount cells drop out.
    let eth_quoter_v2 = PoolRegistration::new(PoolKey::Slipstream(ETH_POOL)).with_metadata(
        ProtocolMetadata::Slipstream(
            V3Metadata::default()
                .with_tick_spacing(50)
                .with_storage_layout(V3StorageLayout::aero_cl(50)),
        ),
    );
    let plain = adapter.verified_storage_targets(&eth_quoter_v2);
    assert!(!plain.iter().any(|(address, _)| *address == registry));
    assert!(!plain.iter().any(|(address, _)| *address == discount));
    assert!(plain.len() < targets.len());

    // Code targets and pinned identities.
    let code = adapter.verified_code_targets(&eth);
    for required in [
        ETH_POOL,
        address!("1642634c8cb1c9f1b84c9a1a08534f8c3f4e89f8"),
        eth_factory,
        hook,
        discount,
        unstaked_module,
        tape,
        address!("0ee89bdfd62e3a5325416fb93c0f1601474a5b04"),
        address!("89e91a960154b1ed6c093039a8f0725ab67d14e0"),
        voter,
        registry,
        gauge_factory,
    ] {
        assert!(code.contains(&required), "code target {required}");
    }
    let identities = adapter.reviewed_code_identities(&eth);
    assert!(identities.contains(&(
        ETH_POOL,
        b256!("55e3735acd886d76160bfd959db31dd476e73e46e8a0d70ac36e4c53ef5a2a45")
    )));
    assert!(identities.contains(&(
        hook,
        b256!("d64b9e760431c312c53515167ffb260b357705388da9354f31095d974e19e449")
    )));
    assert!(identities.contains(&(
        eth_factory,
        b256!("3e53af145d8106d91fad73c6dbbbfe3229c5aa96aab1e18a4bf49a705c773ef5")
    )));
    assert!(
        !identities.iter().any(|(address, _)| *address == voter),
        "voter is warmed, not pinned"
    );

    // Base: per-chain hook/factory hashes, the MEV module, no wired unstaked module.
    let base = aero_registration(BASE_POOL, 100);
    let base_factory = address!("9cb220581b91b0c83223012fe69688b92ad07a76");
    let mev = address!("29f12ce5be3d88b56d245b7f4e969c518f799a4a");
    let targets = adapter.verified_storage_targets(&base);
    let has = |address: Address, key: &str| {
        targets.contains(&(address, U256::from_be_slice(&hex::decode(key).unwrap())))
    };
    assert!(
        has(
            base_factory,
            "1ee0fcb56b8b4abf77390fe52361127dee18d5e42e8ca7944d8ef9698ca9c604"
        ),
        "base factory isPool[pool]"
    );
    assert!(
        has(
            base_factory,
            "320ee17a41780db6f7753a281628d99f4d8b363d44b3c510ef0ee7b41eb01c96"
        ),
        "base factory poolSwapHook[pool]"
    );
    assert!(
        has(
            base_factory,
            "280338e5e18d02bee88c9c70e3db1752d7b6f76322956e52d6effab18a4d6354"
        ),
        "base factory tickSpacingToFee[100]"
    );
    assert!(
        has(
            hook,
            "d06599fe5a76486e0dff68dab38995cd1e99da9d6a3ef11438b1c485b33c0e17"
        ),
        "base hook dynamicFeeConfig[pool]"
    );
    assert!(targets.contains(&(mev, U256::from(1))));
    assert!(targets.contains(&(mev, U256::from(2))));
    assert!(
        !targets
            .iter()
            .any(|(address, _)| *address == unstaked_module)
    );
    assert!(
        has(
            voter,
            "40b7fb5e2e8480eca4387d1f19022b858a8e8f75bea4f64b7a969da6b1b26717"
        ),
        "base voter liveness word"
    );
    assert!(
        has(
            address!("7b2a2c12dc5ea2a31455952a05031c3e4bb28f3e"),
            "7ae93a06fcdd1b51b1bdb1ed50390e4f499131970786796561d99853e9cf351b"
        ),
        "base registry targetToFactory[pool]"
    );
    assert!(
        has(
            gauge_factory,
            "33c7dd370274ed81e5684d1f51cb144784f048ef2800461febc1d530967a00e0"
        ),
        "base gauge factory per-gauge cell"
    );
    assert!(
        has(
            tape,
            "92e8eabab03d98804d39842629a8249cbc658d07cf5986cf05670b22b9e0cc0c"
        ),
        "base tape hook word"
    );
    assert_eq!(
        ConcentratedLiquidityAdapter::aero_cl_block_scoped_storage_targets(&base, 53_205_395),
        vec![(
            hook,
            U256::from_be_slice(
                &hex::decode("3ab53798e62b55409c7ee4f39c52e08b77bba31afdfd37d05cc52dba0de6ff7a")
                    .unwrap()
            )
        )]
    );
    let identities = adapter.reviewed_code_identities(&base);
    assert!(identities.contains(&(
        hook,
        b256!("198b45ce022f423a9852526890d236531333e97861078501357e1c72f0eeecbd")
    )));
    assert!(identities.contains(&(
        mev,
        b256!("fb10c802d480cf1d35d1878c7dc9b9e12a54b600f66da776991554ff0c2f9657")
    )));
    assert!(adapter.verified_code_targets(&base).contains(&mev));

    // The legacy Base family is untouched by the Aero tables.
    let legacy = PoolRegistration::new(PoolKey::Slipstream(LEGACY_BASE_POOL));
    assert!(adapter.reviewed_code_identities(&legacy).is_empty());
    assert_eq!(adapter.verified_storage_targets(&legacy).len(), 4);

    // Every verified cell is owned by the pool generation.
    let dependencies = adapter.state_dependencies(&eth);
    for (address, slot) in adapter.verified_storage_targets(&eth) {
        assert!(
            dependencies
                .slots()
                .contains(&evm_amm_state::adapters::StateSlot::new(address, slot))
        );
    }
}

#[test]
fn aero_sync_spec_and_quote_targets_follow_the_registration() {
    let registration = aero_registration(ETH_POOL, 50);
    let ProtocolMetadata::Slipstream(metadata) = &registration.metadata else {
        panic!("slipstream metadata");
    };
    assert_eq!(metadata.quoter_kind, V3QuoterKind::Metaquoter);
    assert_eq!(metadata.quote_target(&SimConfig::default()), METAQUOTER);
    assert_eq!(
        registration.quote_code_targets(&SimConfig::default()),
        vec![METAQUOTER]
    );
    let spec = V3SyncSpec::slipstream(V3StorageLayout::aero_cl(50));
    assert_eq!(
        spec.observations.map(|o| o.array_slot),
        Some(AERO_CL_OBSERVATIONS_SLOT)
    );
    assert!(spec.static_slots.contains(&AERO_CL_STAKED_LIQUIDITY_SLOT));
    assert!(
        spec.static_slots
            .contains(&AERO_CL_SECONDS_PER_STAKED_LIQUIDITY_SLOT)
    );
    assert_eq!(
        V3StorageLayout::aero_cl(50).slipstream_family(),
        Some(SlipstreamLayoutFamily::AeroCl)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn metaquoter_quotes_run_under_the_transaction_context() {
    let asserter = Asserter::new();
    let client = RpcClient::mocked(asserter.clone());
    let provider = RootProvider::<AnyNetwork>::new(client);
    let mut cache = EvmCache::new(Arc::new(provider)).await;
    cache.set_chain_id(1);
    cache
        .db_mut()
        .insert_account_info(Address::ZERO, AccountInfo::default());
    install(
        &mut cache,
        METAQUOTER,
        runtime(include_str!("fixtures/mock_metaquoter_runtime.hex")),
    );
    cache
        .db_mut()
        .insert_account_storage(METAQUOTER, U256::ZERO, U256::from(1_000_u64))
        .unwrap();
    cache
        .db_mut()
        .insert_account_storage(METAQUOTER, U256::from(1), U256::from(5_000_u64))
        .unwrap();
    let token_in = address!("b1f1ee126e9c96231cc3d3fad7c08b4cf873b1f1");
    let token_out = address!("c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2");
    let origin = address!("1111111111111111111111111111111111111111");
    for account in [token_in, token_out, origin] {
        cache
            .db_mut()
            .insert_account_info(account, AccountInfo::default());
    }
    let registration = aero_registration(ETH_POOL, 50);
    let adapter = ConcentratedLiquidityAdapter::default();

    let plain = adapter
        .simulate_swap(
            &registration,
            &mut cache,
            token_in,
            token_out,
            U256::from(7),
            &SimConfig::default(),
        )
        .expect("metaquoter quote");
    assert_eq!(plain.amount_out, U256::from(1_000_u64));

    let priced = SimConfig::default()
        .with_tx_origin(origin)
        .with_effective_gas_price(198_438_672, 1_000_000_000);
    let quote = adapter
        .simulate_swap(
            &registration,
            &mut cache,
            token_in,
            token_out,
            U256::from(7),
            &priced,
        )
        .expect("metaquoter quote under a gas price");
    assert_eq!(
        quote.amount_out,
        U256::from(1_000_u64 + 1_198_438_672),
        "tx.gasprice must reach the quoter"
    );
    let exact_output = adapter
        .simulate_swap_exact_output(
            &registration,
            &mut cache,
            token_in,
            token_out,
            U256::from(9),
            &priced,
        )
        .expect("metaquoter exact-output quote");
    assert_eq!(
        exact_output.amount_in,
        U256::from(5_000_u64 + 1_198_438_672)
    );

    // A QuoterV2-keyed registration never speaks the Metaquoter ABI, and has
    // no exact-output quote.
    let quoter_v2 = PoolRegistration::new(PoolKey::Slipstream(ETH_POOL)).with_metadata(
        ProtocolMetadata::Slipstream(
            V3Metadata::default()
                .with_tick_spacing(50)
                .with_storage_layout(V3StorageLayout::aero_cl(50))
                .with_quoter(METAQUOTER),
        ),
    );
    assert!(
        adapter
            .simulate_swap_exact_output(
                &quoter_v2,
                &mut cache,
                token_in,
                token_out,
                U256::from(9),
                &priced,
            )
            .is_err()
    );
    assert!(asserter.read_q().is_empty(), "quotes must be fully offline");
}

#[tokio::test(flavor = "multi_thread")]
async fn cold_start_fails_closed_on_a_resident_runtime_identity_mismatch() {
    let asserter = Asserter::new();
    let client = RpcClient::mocked(asserter);
    let provider = RootProvider::<AnyNetwork>::new(client);
    let mut cache = EvmCache::new(Arc::new(provider)).await;
    cache.set_chain_id(1);
    // The reviewed implementation address carries the wrong runtime.
    install(
        &mut cache,
        address!("1642634c8cb1c9f1b84c9a1a08534f8c3f4e89f8"),
        Bytes::from_static(&[0x00]),
    );
    let mut registry = AdapterRegistry::new().with_code_seeding(false);
    registry
        .register_adapter(Arc::new(ConcentratedLiquidityAdapter::default()))
        .unwrap();
    let mut registration = aero_registration(ETH_POOL, 50);
    let outcome = registry
        .cold_start(&mut registration, &mut cache, ColdStartPolicy::HotSlotsOnly)
        .expect("cold start runs");
    match outcome {
        ColdStartOutcome::Unsupported(UnsupportedReason::Custom(reason)) => {
            assert!(
                reason.contains("reviewed runtime identity mismatch"),
                "{reason}"
            );
        }
        other => panic!("expected a fail-closed identity mismatch, got {other:?}"),
    }
}

#[derive(Clone, Copy)]
enum Surface {
    /// A pure swap: slot0, liquidity, the staked word (low 128 bits) and the
    /// observation ring are reproduced exactly.
    Swap,
    /// A position rebalance (burns, collects, mints). Gauge `stake`/`unstake`
    /// calls emit no pool event, so the staked word and tick word 1
    /// (`stakedLiquidityNet`) drift until the next repair; everything on the
    /// quote surface is reproduced exactly.
    Liquidity,
}

fn fixture_registration(fixture: &Value) -> (PoolRegistration, u64, Address) {
    let chain_id = fixture["reference"]["chain_id"].as_u64().unwrap();
    let pool = Address::from_str(fixture["pool"]["address"].as_str().unwrap()).unwrap();
    let spacing = if chain_id == 1 { 50 } else { 100 };
    (aero_registration(pool, spacing), chain_id, pool)
}

fn fixture_context(fixture: &Value, log_index: u64) -> AdapterEventContext {
    let reference = &fixture["reference"];
    AdapterEventContext::for_block(
        reference["block_number"].as_u64().unwrap(),
        B256::from_str(reference["block_hash"].as_str().unwrap()).unwrap(),
        reference["block_timestamp"].as_u64().unwrap(),
    )
    .with_chain_id(reference["chain_id"].as_u64().unwrap())
    .with_parent_hash(B256::from_str(reference["parent_hash"].as_str().unwrap()).unwrap())
    .with_transaction_hash(B256::from_str(reference["transaction_hash"].as_str().unwrap()).unwrap())
    .with_event_order(reference["transaction_index"].as_u64().unwrap(), log_index)
}

fn replay_fixture(contents: &str, surface: Surface) {
    let fixture: Value = serde_json::from_str(contents).expect("checked-in fixture");
    assert_eq!(fixture["schema"], "evm-amm-state-aero-historical-v1");
    let (registration, _chain_id, pool) = fixture_registration(&fixture);
    assert_eq!(
        fixture["pool"]["proxy_runtime_code_hash"],
        "0x55e3735acd886d76160bfd959db31dd476e73e46e8a0d70ac36e4c53ef5a2a45"
    );
    let adapter = ConcentratedLiquidityAdapter::default();
    let mut state = FixtureState::default();
    for (slot, value) in fixture["parent_storage"].as_object().unwrap() {
        state
            .0
            .insert((pool, word(slot)), word(value.as_str().unwrap()));
    }
    let parent = state.clone();
    for entry in fixture["pool_logs"].as_array().unwrap() {
        let log_index = entry["log_index"].as_u64().unwrap();
        let topics = entry["topics"]
            .as_array()
            .unwrap()
            .iter()
            .map(|topic| B256::from_str(topic.as_str().unwrap()).unwrap())
            .collect();
        let data = Bytes::from(
            hex::decode(entry["data"].as_str().unwrap().trim_start_matches("0x")).unwrap(),
        );
        let log = Log::new(pool, topics, data).expect("valid fixture log");
        let context = fixture_context(&fixture, log_index);
        let result = adapter.decode_event_with_context(&registration, &log, &state, &context);
        let event = result
            .event
            .unwrap_or_else(|| panic!("log {log_index} produced no event: {:?}", result.error));
        assert!(
            result.error.is_none(),
            "log {log_index} must replay exactly: {:?}",
            result.error
        );
        assert_eq!(
            event.quality,
            UpdateQuality::Exact,
            "log {log_index} must be an exact transition"
        );
        for update in event.updates {
            match update {
                StateUpdate::Slot {
                    address,
                    slot,
                    value,
                } => {
                    state.0.insert((address, slot), value);
                }
                StateUpdate::Purge {
                    address,
                    scope: PurgeScope::Slots(slots),
                } => {
                    for slot in slots {
                        state.0.remove(&(address, slot));
                    }
                }
                other => panic!("unexpected non-exact update {other:?}"),
            }
        }
    }

    let expected: BTreeMap<U256, U256> = fixture["expected_writes"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(slot, value)| (word(slot), word(value.as_str().unwrap())))
        .collect();
    let spacing = if pool == ETH_POOL { 50 } else { 100 };
    let layout = V3StorageLayout::aero_cl(spacing);
    let ring = AERO_CL_OBSERVATIONS_SLOT;
    let ring_end = ring + U256::from(65_535);
    let chain_value = |slot: U256| -> Option<U256> {
        expected
            .get(&slot)
            .copied()
            .or_else(|| parent.0.get(&(pool, slot)).copied())
    };
    let replayed = |slot: U256| -> Option<U256> { state.0.get(&(pool, slot)).copied() };

    // Static quote-surface slots and the observation ring.
    let mut compared = 0;
    for (slot, expected_value) in &expected {
        let on_surface = *slot == layout.slot0_slot
            || *slot == layout.liquidity_slot
            || (*slot >= ring && *slot < ring_end)
            || (matches!(surface, Surface::Swap) && *slot == AERO_CL_STAKED_LIQUIDITY_SLOT);
        if !on_surface {
            continue;
        }
        let value = replayed(*slot).unwrap_or_else(|| {
            panic!("slot {slot:#x} was written on chain but is unknown after replay")
        });
        if *slot == AERO_CL_STAKED_LIQUIDITY_SLOT {
            assert_eq!(
                value & WORD_128_MASK,
                *expected_value & WORD_128_MASK,
                "stakedLiquidity (low 128 bits of slot 12) must match"
            );
            assert_eq!(
                value >> 176_usize,
                parent.0[&(pool, *slot)] >> 176_usize,
                "tickSpacing bits must be preserved"
            );
        } else {
            assert_eq!(
                value, *expected_value,
                "slot {slot:#x} must match the on-chain post-state"
            );
        }
        compared += 1;
    }
    assert!(compared >= 2, "the fixture must exercise the quote surface");

    // Every tick named by a Mint/Burn log: word 0 (liquidityGross | liquidityNet)
    // and its bitmap word are exact; a cleared tick is fully zeroed.
    let mut ticks = Vec::new();
    for entry in fixture["pool_logs"].as_array().unwrap() {
        let topics = entry["topics"].as_array().unwrap();
        let topic0 = topics[0].as_str().unwrap();
        let is_mint = topic0.starts_with("0x7a53080b");
        let is_burn = topic0.starts_with("0x0c396cd9");
        if !(is_mint || is_burn) {
            continue;
        }
        for topic in &topics[2..4] {
            let raw = U256::from_str(topic.as_str().unwrap()).unwrap();
            let low = (raw & U256::from(0x00ff_ffff_u32)).to::<u32>();
            let tick = if low & 0x0080_0000 != 0 {
                (low | 0xff00_0000) as i32
            } else {
                low as i32
            };
            if !ticks.contains(&tick) {
                ticks.push(tick);
            }
        }
    }
    for tick in ticks {
        let keys = slipstream_tick_info_storage_keys_with_base(tick, layout.ticks_base_slot);
        let expected_word0 = chain_value(keys[0]).unwrap_or(U256::ZERO);
        let value = replayed(keys[0]).unwrap_or(U256::ZERO);
        assert_eq!(value, expected_word0, "tick {tick} word 0 must match");
        if expected_word0.is_zero() {
            for key in keys {
                assert!(
                    replayed(key).unwrap_or(U256::ZERO).is_zero(),
                    "cleared tick {tick} must be fully zeroed"
                );
            }
        }
        let bitmap = v3_tick_bitmap_storage_key_with_base(
            v3_word_position(tick, spacing),
            layout.tick_bitmap_base_slot,
        );
        assert_eq!(
            replayed(bitmap),
            chain_value(bitmap),
            "bitmap word of tick {tick} must match"
        );
    }
}

#[test]
fn ethereum_aero_swap_replays_exactly() {
    replay_fixture(
        include_str!("fixtures/ethereum_aero_cl_swap.json"),
        Surface::Swap,
    );
}

#[test]
fn base_aero_swap_replays_exactly() {
    replay_fixture(
        include_str!("fixtures/base_aero_cl_swap.json"),
        Surface::Swap,
    );
}

#[test]
fn base_aero_rebalance_replays_exactly() {
    replay_fixture(
        include_str!("fixtures/base_aero_cl_rebalance.json"),
        Surface::Liquidity,
    );
}

#[test]
fn base_aero_swap_then_rebalance_replays_exactly() {
    replay_fixture(
        include_str!("fixtures/base_aero_cl_swap_rebalance.json"),
        Surface::Liquidity,
    );
}
