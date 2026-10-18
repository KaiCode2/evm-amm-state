use super::*;
use alloy_primitives::address;
const GAUGE: Address = address!("41160e66fcaa10cbb148ace60bc2a22d609ec519");
const NFT: Address = address!("416b433906b1b72fa758e166e239c43d68dc6f29");

fn event(
    header: &RpcHeader,
    emitter: Address,
    signature: &str,
    topics: Vec<B256>,
    index: u64,
) -> RpcLog {
    let mut log = reconciled_log(emitter, keccak256(signature), header, index);
    log.inner = PrimitiveLog::new(
        emitter,
        std::iter::once(keccak256(signature))
            .chain(topics)
            .collect(),
        Bytes::new(),
    )
    .unwrap();
    log
}

fn evidence(header: &RpcHeader) -> (RpcLog, RpcLog, RpcLog) {
    let token = B256::from(U256::from(7).to_be_bytes::<32>());
    let owner = Address::repeat_byte(0x42).into_word();
    let gauge = event(
        header,
        GAUGE,
        "Deposit(address,uint256,uint128)",
        vec![
            owner,
            token,
            B256::from(U256::from(400).to_be_bytes::<32>()),
        ],
        5,
    );
    let transfer = event(
        header,
        NFT,
        "Transfer(address,address,uint256)",
        vec![owner, GAUGE.into_word(), token],
        4,
    );
    let mut collect = event(
        header,
        NFT,
        "Collect(uint256,address,uint256,uint256)",
        vec![token],
        3,
    );
    collect.inner.data.data = encoded_words([
        U256::from_be_slice(owner.as_slice()),
        U256::ZERO,
        U256::ZERO,
    ]);
    (gauge, transfer, collect)
}

#[tokio::test]
async fn evidence_is_bounded_exact_block_deduplicated_and_not_fetched_on_quiet_blocks() -> Result<()>
{
    tokio::task::LocalSet::new()
        .run_until(async {
            let asserter = Asserter::new();
            let base = header(500, B256::repeat_byte(0x49));
            let delivered = header(501, base.hash);
            let block = BlockRef {
                number: 501,
                hash: delivered.hash,
                parent_hash: Some(base.hash),
                timestamp: Some(delivered.inner.timestamp),
            };
            let (mut driver, runtime, stats) = subscription_driver(asserter.clone(), base).await?;
            let mut logs = BTreeMap::new();
            driver
                .complete_staking_evidence(10, &block, &mut logs)
                .await?;
            assert_eq!(stats.snapshot().reconciliation_requests(), 0);
            let (gauge, transfer, collect) = evidence(&delivered);
            logs.insert(5, gauge);
            // An already streamed transfer is identical evidence, not a second transition.
            logs.insert(4, transfer.clone());
            let mut fetched_transfer = transfer;
            fetched_transfer.block_timestamp = block.timestamp;
            asserter.push_success(&vec![fetched_transfer]);
            asserter.push_success(&vec![collect]);
            driver
                .complete_staking_evidence(10, &block, &mut logs)
                .await?;
            assert_eq!(logs.keys().copied().collect::<Vec<_>>(), [3, 4, 5]);
            assert_eq!(stats.snapshot().reconciliation_requests(), 2);
            assert_eq!(stats.snapshot().reconciliation_logs_fetched(), 2);
            assert!(asserter.read_q().is_empty());
            runtime.shutdown().await?;
            Ok(())
        })
        .await
}

#[tokio::test]
async fn wrong_hash_conflicts_and_out_of_filter_evidence_are_rejected() -> Result<()> {
    tokio::task::LocalSet::new()
        .run_until(async {
            for failure in ["hash", "timestamp", "filter", "conflict", "provider"] {
                let asserter = Asserter::new();
                let base = header(500, B256::repeat_byte(0x49));
                let delivered = header(501, base.hash);
                let block = BlockRef {
                    number: 501,
                    hash: delivered.hash,
                    parent_hash: Some(base.hash),
                    timestamp: Some(delivered.inner.timestamp),
                };
                let (mut driver, runtime, _) = subscription_driver(asserter.clone(), base).await?;
                let (gauge, mut transfer, collect) = evidence(&delivered);
                let mut logs = BTreeMap::from([(5, gauge)]);
                match failure {
                    "timestamp" => transfer.block_timestamp = Some(u64::MAX),
                    "hash" => transfer.block_hash = Some(B256::repeat_byte(0xaa)),
                    "filter" => transfer.inner.address = Address::repeat_byte(0x55),
                    "conflict" => {
                        let mut conflicting = transfer.clone();
                        conflicting.transaction_hash = Some(B256::repeat_byte(0xbb));
                        logs.insert(4, conflicting);
                    }
                    _ => {}
                }
                if failure == "provider" {
                    asserter.push_failure_msg("unavailable");
                } else {
                    asserter.push_success(&vec![transfer]);
                }
                asserter.push_success(&vec![collect]);
                assert!(
                    driver
                        .complete_staking_evidence(10, &block, &mut logs)
                        .await
                        .is_err(),
                    "{failure}"
                );
                runtime.shutdown().await?;
            }
            Ok(())
        })
        .await
}

#[tokio::test]
async fn unrelated_nft_transfer_bloom_does_not_force_reconciliation() -> Result<()> {
    tokio::task::LocalSet::new()
        .run_until(async {
            let filter = Filter::new()
                .address(NFT)
                .event_signature(keccak256("Transfer(address,address,uint256)"))
                .topic1(GAUGE.into_word());
            let (driver, runtime, _) =
                subscription_driver_with_filter(Asserter::new(), header(500, B256::ZERO), filter)
                    .await?;
            let mut bloom = alloy_primitives::Bloom::ZERO;
            bloom.accrue(alloy_primitives::BloomInput::Raw(NFT.as_slice()));
            let topic = keccak256("Transfer(address,address,uint256)");
            bloom.accrue(alloy_primitives::BloomInput::Raw(topic.as_slice()));
            assert!(driver.bloom_excludes_every_interest(&bloom));
            bloom.accrue(alloy_primitives::BloomInput::Raw(
                GAUGE.into_word().as_slice(),
            ));
            assert!(
                !driver.bloom_excludes_every_interest(&bloom),
                "membership is not proof of absence"
            );
            runtime.shutdown().await?;
            Ok(())
        })
        .await
}
