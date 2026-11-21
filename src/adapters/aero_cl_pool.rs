//! Factory-admitted Aero (Dromos) CL pools.
//!
//! Every Aero CL pool is an EIP-1167 clone of one shared implementation that
//! the chain's `CLFactory` deploys with CREATE2, salted by
//! `keccak(abi.encode(token0, token1, int24 tickSpacing))`. A registration
//! whose address re-derives from its factory, tokens and tick spacing is
//! therefore provably a factory pool running the reviewed runtime, so it is
//! admitted to the reviewed Aero CL surface without a per-pool pin. The
//! deployment-wide bindings (factory, hook, fee modules, NFT manager,
//! Metaquoter registry) are reviewed once per chain here.

use alloy_primitives::{Address, B256, address, b256};

use super::factory::derive;
use super::storage::{SlipstreamLayoutFamily, layout_for};
use super::types::{PoolRegistration, ProtocolId, ProtocolMetadata};

/// Shared Aero CL implementation behind every Aero CL pool proxy.
pub(crate) const AERO_CL_IMPLEMENTATION: Address =
    address!("1642634c8cb1c9f1b84c9a1a08534f8c3f4e89f8");
pub(crate) const AERO_CL_IMPLEMENTATION_RUNTIME_CODE_HASH: B256 =
    b256!("041172c7cbffc45627f9dd52727a0124cd6eeb2b5830ab392df54f71a6948693");
/// EIP-1167 proxy runtime delegating to [`AERO_CL_IMPLEMENTATION`].
pub(crate) const AERO_CL_PROXY_RUNTIME_CODE_HASH: B256 =
    b256!("55e3735acd886d76160bfd959db31dd476e73e46e8a0d70ac36e4c53ef5a2a45");
/// Keccak of the EIP-1167 creation code cloning [`AERO_CL_IMPLEMENTATION`].
pub(crate) const AERO_CL_POOL_INIT_CODE_HASH: B256 =
    b256!("3e77b70b9c4ce644601ed4aab19834375c6a9a2bca7adeb89a7bdd5fd8eb0a7d");
/// The Aero `Metaquoter` (same address on every chain).
pub(crate) const AERO_CL_METAQUOTER: Address = address!("1ddfec5058180c207f55bb77d95acdd0d8227868");
/// Tick spacings enabled on every reviewed Aero CL factory.
pub(crate) const AERO_CL_TICK_SPACINGS: [i32; 7] = [1, 10, 50, 100, 150, 200, 250];
/// `CLFactory._getPool` base slot (after the four `CLFactoryIndexation` slots
/// and the fifteen `CLFactory` state variables).
pub(crate) const AERO_CL_GET_POOL_BASE_SLOT: u64 = 19;
/// `CLPool.gauge`.
pub(crate) const AERO_CL_POOL_GAUGE_SLOT: u64 = 3;

/// The chain-specific bindings of one reviewed Aero CL factory deployment,
/// with the runtime keccak observed at the pinned review block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AeroClDeployment {
    pub(crate) chain_id: u64,
    pub(crate) factory: Address,
    pub(crate) factory_runtime_code_hash: B256,
    pub(crate) hook_runtime_code_hash: B256,
    pub(crate) mev_tax_module: Option<(Address, B256)>,
    pub(crate) unstaked_fee_module_runtime_code_hash: B256,
    pub(crate) unstaked_fee_module_wired: bool,
    pub(crate) nft_runtime_code_hash: B256,
    pub(crate) factory_registry: Address,
}

pub(crate) const AERO_CL_ETHEREUM: AeroClDeployment = AeroClDeployment {
    chain_id: 1,
    factory: address!("f4fd2e6c5b60c5b365cd1b96dd6245e6d60414c2"),
    factory_runtime_code_hash: b256!(
        "3e53af145d8106d91fad73c6dbbbfe3229c5aa96aab1e18a4bf49a705c773ef5"
    ),
    hook_runtime_code_hash: b256!(
        "d64b9e760431c312c53515167ffb260b357705388da9354f31095d974e19e449"
    ),
    mev_tax_module: None,
    unstaked_fee_module_runtime_code_hash: b256!(
        "90c9628c5dd2b42252a3ddadfc1bf8bf4fe81ed4ee4d2df1beb48403d5434a2d"
    ),
    unstaked_fee_module_wired: true,
    nft_runtime_code_hash: b256!(
        "0e52c540913f0de73b9063379637246c59eff6e75ebad5ad7ed23f0bd591ab04"
    ),
    factory_registry: address!("dd1705e23762f4d6e44d8c31bd8ea80c54612239"),
};

pub(crate) const AERO_CL_BASE: AeroClDeployment = AeroClDeployment {
    chain_id: 8_453,
    factory: address!("9cb220581b91b0c83223012fe69688b92ad07a76"),
    factory_runtime_code_hash: b256!(
        "6c09090837ace0aefe6bfa24a3bc772d13a9775b7e3109c9fece534a0ce053a6"
    ),
    hook_runtime_code_hash: b256!(
        "198b45ce022f423a9852526890d236531333e97861078501357e1c72f0eeecbd"
    ),
    mev_tax_module: Some((
        address!("29f12ce5be3d88b56d245b7f4e969c518f799a4a"),
        b256!("fb10c802d480cf1d35d1878c7dc9b9e12a54b600f66da776991554ff0c2f9657"),
    )),
    unstaked_fee_module_runtime_code_hash: b256!(
        "e3ec637a6e3236331711ec5d4c476d8f0c0e9d54e8c3936fe068195884b5b829"
    ),
    unstaked_fee_module_wired: false,
    nft_runtime_code_hash: b256!(
        "b2d0d8278354542651e2db288b71292635d9fd7bd28784f736373da801974599"
    ),
    factory_registry: address!("7b2a2c12dc5ea2a31455952a05031c3e4bb28f3e"),
};

const AERO_CL_DEPLOYMENTS: [AeroClDeployment; 2] = [AERO_CL_ETHEREUM, AERO_CL_BASE];

/// The reviewed Aero CL deployment whose factory is `factory`, if any.
pub(crate) fn aero_cl_deployment_for_factory(factory: Address) -> Option<AeroClDeployment> {
    AERO_CL_DEPLOYMENTS
        .into_iter()
        .find(|deployment| deployment.factory == factory)
}

/// The CREATE2 address `factory` deploys the `(token0, token1, tick_spacing)`
/// Aero CL pool at.
pub(crate) fn aero_cl_pool_address(
    factory: Address,
    token0: Address,
    token1: Address,
    tick_spacing: i32,
) -> Address {
    derive::v3_pool_address_by_spacing(
        factory,
        AERO_CL_POOL_INIT_CODE_HASH,
        token0,
        token1,
        tick_spacing,
    )
}

/// A registration proven to be a pool of a reviewed Aero CL deployment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AdmittedAeroClPool {
    pub(crate) deployment: AeroClDeployment,
    pub(crate) pool: Address,
    pub(crate) tick_spacing: i32,
    pub(crate) gauge: Option<Address>,
}

/// Admit `pool` when its metadata names a reviewed Aero CL factory, sorted
/// tokens, the Aero CL storage layout and a tick spacing from which the
/// factory's CREATE2 derivation reproduces the registered address. Anything
/// else is `None` and stays on the fail-closed generic Slipstream path.
pub(crate) fn admit_aero_cl_pool(pool: &PoolRegistration) -> Option<AdmittedAeroClPool> {
    if pool.protocol() != ProtocolId::Slipstream {
        return None;
    }
    let ProtocolMetadata::Slipstream(metadata) = &pool.metadata else {
        return None;
    };
    let address = pool.key.address()?;
    let deployment = aero_cl_deployment_for_factory(metadata.factory?)?;
    let (token0, token1) = (metadata.token0?, metadata.token1?);
    let layout = layout_for(pool)?;
    let tick_spacing = layout.tick_spacing;
    let admissible = token0 < token1
        && tick_spacing > 0
        && metadata
            .tick_spacing
            .is_none_or(|spacing| spacing == tick_spacing)
        && layout.slipstream_family() == Some(SlipstreamLayoutFamily::AeroCl)
        && aero_cl_pool_address(deployment.factory, token0, token1, tick_spacing) == address;
    admissible.then_some(AdmittedAeroClPool {
        deployment,
        pool: address,
        tick_spacing,
        gauge: metadata.gauge.filter(|gauge| *gauge != Address::ZERO),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::{PoolKey, V3Metadata, V3StorageLayout};

    const WETH: Address = address!("4200000000000000000000000000000000000006");
    const USDC: Address = address!("833589fcd6edb6e08f4c7c32d4f71b54bda02913");
    const BASE_WETH_USDC_100: Address = address!("2a430bebd2082d8e05d56d64dede8944279bbc24");

    fn registration(address: Address, metadata: V3Metadata) -> PoolRegistration {
        PoolRegistration::new(PoolKey::Slipstream(address))
            .with_metadata(ProtocolMetadata::Slipstream(metadata))
    }

    fn base_metadata() -> V3Metadata {
        V3Metadata::default()
            .with_token0(WETH)
            .with_token1(USDC)
            .with_tick_spacing(100)
            .with_factory(AERO_CL_BASE.factory)
            .with_storage_layout(V3StorageLayout::aero_cl(100))
    }

    #[test]
    fn create2_reproduces_the_base_weth_usdc_pool() {
        assert_eq!(
            aero_cl_pool_address(AERO_CL_BASE.factory, WETH, USDC, 100),
            BASE_WETH_USDC_100
        );
    }

    #[test]
    fn admits_a_factory_derived_pool() {
        let gauge = address!("00000000000000000000000000000000000000aa");
        let admitted = admit_aero_cl_pool(&registration(
            BASE_WETH_USDC_100,
            base_metadata().with_gauge(gauge),
        ))
        .expect("factory-derived pool is admitted");
        assert_eq!(admitted.deployment, AERO_CL_BASE);
        assert_eq!(admitted.pool, BASE_WETH_USDC_100);
        assert_eq!(admitted.tick_spacing, 100);
        assert_eq!(admitted.gauge, Some(gauge));
    }

    #[test]
    fn rejects_registrations_the_factory_did_not_deploy() {
        let other = address!("00000000000000000000000000000000000000bb");
        let rejected = [
            registration(other, base_metadata()),
            registration(
                BASE_WETH_USDC_100,
                base_metadata().with_factory(AERO_CL_ETHEREUM.factory),
            ),
            registration(
                BASE_WETH_USDC_100,
                base_metadata()
                    .with_tick_spacing(50)
                    .with_storage_layout(V3StorageLayout::aero_cl(50)),
            ),
            registration(
                BASE_WETH_USDC_100,
                base_metadata().with_storage_layout(V3StorageLayout::slipstream(100)),
            ),
            registration(
                BASE_WETH_USDC_100,
                base_metadata().with_token0(USDC).with_token1(WETH),
            ),
            PoolRegistration::new(PoolKey::UniswapV3(BASE_WETH_USDC_100))
                .with_metadata(ProtocolMetadata::UniswapV3(base_metadata())),
        ];
        assert!(
            rejected
                .iter()
                .all(|pool| admit_aero_cl_pool(pool).is_none())
        );
    }
}
