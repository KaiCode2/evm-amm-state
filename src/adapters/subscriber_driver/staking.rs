//! Complete staking proofs at the common canonical-delivery boundary.
use super::*;

impl<P> AlloyAmmSubscriberDriver<P>
where
    P: Provider<Ethereum> + Clone + Send + Sync + 'static,
{
    pub(super) async fn complete_staking_evidence(
        &mut self,
        chain: u64,
        block: &BlockRef,
        logs: &mut BTreeMap<u64, RpcLog>,
    ) -> Result<(), AmmSubscriberDriverError> {
        // Validate the trigger before it can cause provider work. Reorg repair
        // and subscription sealing use the same exact hash and validation.
        for log in logs.values() {
            validated_log_key(log, block)?;
        }
        if chain == 10 && logs.len() > 16_384 {
            return Err(AmmSubscriberDriverError::InvalidCanonicalLog(
                "staking trigger exceeds batch bound",
            ));
        }
        let filters =
            super::super::slipstream_staking::reads::evidence_filters(chain, logs.values());
        // Two filters per bounded token chunk: issue the pair concurrently, so
        // a relevant staking block adds one provider round trip, not two.
        for pair in filters.chunks_exact(2) {
            let transfer = pair[0].clone().at_block_hash(block.hash);
            let collect = pair[1].clone().at_block_hash(block.hash);
            AmmSubscriberDriverCounters::add(&self.stats.reconciliation_requests, 2);
            let (transfers, collects) = tokio::join!(
                self.subscriber.provider().get_logs(&transfer),
                self.subscriber.provider().get_logs(&collect),
            );
            for (filter, result) in [(transfer, transfers), (collect, collects)] {
                let fetched = result
                    .map_err(|error| AmmSubscriberDriverError::Provider(error.to_string()))?;
                AmmSubscriberDriverCounters::add(
                    &self.stats.reconciliation_logs_fetched,
                    fetched.len() as u64,
                );
                for log in fetched {
                    let key = validated_log_key(&log, block)?;
                    if !filter.address.matches(&log.address())
                        || !filter
                            .topics
                            .iter()
                            .zip(log.topics().iter().map(Some).chain(std::iter::repeat(None)))
                            .all(|(expected, actual)| {
                                expected.is_empty()
                                    || actual.is_some_and(|topic| expected.matches(topic))
                            })
                    {
                        return Err(AmmSubscriberDriverError::InvalidCanonicalLog(
                            "staking evidence violated requested filter",
                        ));
                    }
                    match logs.entry(key) {
                        std::collections::btree_map::Entry::Vacant(entry) => {
                            entry.insert(log);
                        }
                        std::collections::btree_map::Entry::Occupied(entry)
                            if entry.get() != &log =>
                        {
                            return Err(AmmSubscriberDriverError::InvalidCanonicalLog(
                                "conflicting staking evidence at one log index",
                            ));
                        }
                        std::collections::btree_map::Entry::Occupied(_) => {}
                    }
                    if logs.len() > 16_384 {
                        return Err(AmmSubscriberDriverError::InvalidCanonicalLog(
                            "staking evidence exceeds batch bound",
                        ));
                    }
                }
            }
        }
        Ok(())
    }
}
