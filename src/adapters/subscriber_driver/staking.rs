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
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        for pair in filters.chunks_exact(2) {
            let transfer = pair[0].clone().at_block_hash(block.hash);
            let collect = pair[1].clone().at_block_hash(block.hash);
            AmmSubscriberDriverCounters::add(&self.stats.reconciliation_requests, 2);
            let provider = self.subscriber.provider().clone();
            let (transfers, collects) = evidence_deadline(deadline, async {
                tokio::join!(provider.get_logs(&transfer), provider.get_logs(&collect),)
            })
            .await?;
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
                    // WS often omits blockTimestamp while getLogs includes it.
                    // Both were checked against the header; optional metadata
                    // presence must not turn the same canonical log into a conflict.
                    match logs.entry(key) {
                        std::collections::btree_map::Entry::Vacant(entry) => {
                            entry.insert(log);
                        }
                        std::collections::btree_map::Entry::Occupied(entry)
                            if !super::reconciliation::same_validated_log(entry.get(), &log) =>
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

async fn evidence_deadline<F: std::future::Future>(
    deadline: tokio::time::Instant,
    future: F,
) -> Result<F::Output, AmmSubscriberDriverError> {
    tokio::time::timeout_at(deadline, future)
        .await
        .map_err(|_| {
            AmmSubscriberDriverError::Provider("staking evidence deadline exceeded".to_owned())
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn a_stalled_read_cannot_hold_publication_indefinitely() {
        let result = evidence_deadline(
            tokio::time::Instant::now() + Duration::from_millis(1),
            std::future::pending::<()>(),
        )
        .await;
        assert!(matches!(result, Err(AmmSubscriberDriverError::Provider(_))));
    }
}
