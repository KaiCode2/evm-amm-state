//! Exact canonical reads with bounded parallelism and deterministic merging.
use super::*;
use alloy_rpc_types_eth::Filter;
use futures::{StreamExt, stream};

impl<P> AlloyAmmSubscriberDriver<P>
where
    P: Provider<Ethereum> + Clone + Send + Sync + 'static,
{
    pub(super) async fn fetch_reconciliation_logs(
        &mut self,
        block: &BlockRef,
        filters: Vec<Filter>,
    ) -> Result<BTreeMap<u64, RpcLog>, AmmSubscriberDriverError> {
        let provider = self.subscriber.provider().clone();
        let stats = &self.stats;
        // Parallel preparation does not collapse these into one billable call.
        // No tasks outlive this future; dropping it cancels remaining waiters.
        let mut reads = stream::iter(filters)
            .map(|filter| {
                let provider = provider.clone();
                async move {
                    AmmSubscriberDriverCounters::add(&stats.reconciliation_requests, 1);
                    provider.get_logs(&filter.at_block_hash(block.hash)).await
                }
            })
            .buffered(4);
        let mut logs = BTreeMap::new();
        while let Some(result) = reads.next().await {
            let fetched = result.map_err(|_| {
                AmmSubscriberDriverError::Provider(
                    "canonical log reconciliation RPC failed".to_owned(),
                )
            })?;
            AmmSubscriberDriverCounters::add(
                &self.stats.reconciliation_logs_fetched,
                fetched.len() as u64,
            );
            for log in fetched {
                let key = validated_log_key(&log, block)?;
                match logs.entry(key) {
                    std::collections::btree_map::Entry::Vacant(entry) => {
                        entry.insert(log);
                    }
                    std::collections::btree_map::Entry::Occupied(entry)
                        if !same_validated_log(entry.get(), &log) =>
                    {
                        return Err(AmmSubscriberDriverError::InvalidCanonicalLog(
                            "conflicting logs share one global log index",
                        ));
                    }
                    std::collections::btree_map::Entry::Occupied(_) => {}
                }
            }
        }
        Ok(logs)
    }
}

// Both logs have already been validated against the same header and keyed by
// global log index. Optional timestamp presence is not consensus identity.
pub(super) fn same_validated_log(a: &RpcLog, b: &RpcLog) -> bool {
    a.inner == b.inner
        && a.transaction_hash == b.transaction_hash
        && a.transaction_index == b.transaction_index
}
