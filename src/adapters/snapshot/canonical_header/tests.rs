use super::*;
use crate::adapters::{
    AdapterRegistry, AmmChangeSet, AmmRuntime, AmmRuntimeBaseline, AmmRuntimeConfig,
    AmmStateCommit, AmmStateQuality,
};
use alloy_consensus::Header as ConsensusHeader;
use alloy_primitives::B256;
use alloy_provider::{RootProvider, network::AnyNetwork};
use alloy_rpc_client::RpcClient;
use alloy_transport::mock::Asserter;
use evm_fork_cache::EvmCache;
use std::sync::Arc;

#[tokio::test]
async fn commit_rejects_wrong_chain_number_or_hash_even_with_valid_header() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let provider = RootProvider::<AnyNetwork>::new(RpcClient::mocked(Asserter::new()));
            let mut cache = EvmCache::new(Arc::new(provider)).await;
            let header = Header::new(ConsensusHeader {
                base_fee_per_gas: Some(1),
                gas_limit: 30_000_000,
                ..ConsensusHeader::default()
            });
            cache.advance_block(&header).unwrap();
            let baseline = AmmRuntimeBaseline::from_verified_header(1, header.clone()).unwrap();
            let runtime = AmmRuntime::spawn(
                cache,
                AdapterRegistry::new(),
                baseline,
                AmmRuntimeConfig::default(),
            )
            .unwrap();
            let snapshot = runtime.latest_snapshot();
            let changes = Arc::new(
                AmmChangeSet::new(
                    snapshot.version(),
                    snapshot.point(),
                    AmmStateQuality::Coherent,
                    [],
                    [],
                    false,
                )
                .unwrap(),
            );
            let matching = Arc::new(AmmCanonicalHeader::from_verified_parts(1, header.clone()));
            assert!(
                AmmStateCommit::new(snapshot.clone(), changes.clone(), None, Some(matching))
                    .canonical_header()
                    .is_some()
            );
            let mut next_number = header.inner.clone();
            next_number.number += 1;
            let mut other_hash = header.inner.clone();
            other_hash.parent_hash = B256::repeat_byte(0xaa);
            for evidence in [
                AmmCanonicalHeader::from_verified_parts(2, header),
                AmmCanonicalHeader::from_verified_parts(1, Header::new(next_number)),
                AmmCanonicalHeader::from_verified_parts(1, Header::new(other_hash)),
            ] {
                let commit = AmmStateCommit::new(
                    snapshot.clone(),
                    changes.clone(),
                    None,
                    Some(Arc::new(evidence)),
                );
                assert!(commit.canonical_header().is_none());
                assert_eq!(commit.snapshot().point(), snapshot.point());
            }
            runtime.shutdown().await.unwrap();
        })
        .await;
}
