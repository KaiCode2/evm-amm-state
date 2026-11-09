//! Strict existing-subscription assembly. No incomplete input authorizes a read.

use super::*;
use alloy_rpc_types_eth::Filter;
use evm_fork_cache::reactive::{LogInterest, SubscriberGapPolicy, SubscriberInputBatch};

pub(super) fn validate_attachment<P>(
    subscriber: &AlloySubscriber<P, Ethereum>,
    config: AmmSubscriberDriverConfig,
) -> Result<(), AmmSubscriberDriverError>
where
    P: Provider<Ethereum> + Clone + Send + Sync + 'static,
{
    if config.canonical_log_source != CanonicalLogSource::SubscriptionOnly {
        return Ok(());
    }
    let source = subscriber.config();
    if subscriber.mode() != SubscriberMode::PubSub
        || subscriber.gap_policy() != SubscriberGapPolicy::FailClosed
        || source.reconnect.enabled
        || source.verify_log_block_context
        || source.hydrate_pending_transactions
        || source.preconfirmations != PreconfirmationMode::Disabled
    {
        return Err(AmmSubscriberDriverError::EventOnly(
            "requires PubSub, FailClosed gaps, and disabled reconnect/verification/previews/pending hydration",
        ));
    }
    Ok(())
}

impl<P> AlloyAmmSubscriberDriver<P>
where
    P: Provider<Ethereum> + Clone + Send + Sync + 'static,
{
    pub(super) async fn prepare_event_only_sources(
        &mut self,
        plans: &[AmmSubscriberOwnerPlan],
    ) -> Result<(), AmmSubscriberDriverError> {
        if self.canonical_log_source != CanonicalLogSource::SubscriptionOnly {
            return Ok(());
        }
        let mut addresses = std::collections::BTreeSet::new();
        for interest in self
            .initial_interests
            .iter()
            .chain(plans.iter().flat_map(|plan| &plan.interests))
        {
            match interest {
                ReactiveInterest::Logs(logs) => {
                    if logs.provider_filter.address.is_empty()
                        || logs.provider_filter.block_option != Filter::default().block_option
                    {
                        return Err(AmmSubscriberDriverError::EventOnly(
                            "cold combined source needs bounded addresses and unbounded live block range",
                        ));
                    }
                    addresses.extend(logs.provider_filter.address.iter().copied());
                }
                ReactiveInterest::Blocks(block)
                    if block.mode == evm_fork_cache::reactive::BlockInterestMode::Header => {}
                _ => {
                    return Err(AmmSubscriberDriverError::EventOnly(
                        "only canonical logs and headers are supported",
                    ));
                }
            }
        }
        if addresses.is_empty()
            || addresses.len() > self.subscriber.config().max_log_addresses_per_subscription
        {
            return Err(AmmSubscriberDriverError::EventOnly(
                "combined source exceeds its configured address bound or has no log source",
            ));
        }
        let mut interests = self.initial_interests.clone();
        interests.push(ReactiveInterest::Logs(LogInterest {
            provider_filter: Filter::new().address(addresses.into_iter().collect::<Vec<_>>()),
            local_matcher: None,
            route_key: None,
        }));
        // This runs only during cold adoption before any owner is staged. The
        // ordinary owner reconciliation below establishes the exact baseline.
        // The broad base filter subsumes each owner filter into one physical
        // stream; owner/adapter routing still checks the exact local predicate.
        self.subscriber.register_interests(&interests).await?;
        Ok(())
    }

    pub(super) fn require_automatic_source_mutation_allowed(
        &self,
    ) -> Result<(), AmmSubscriberDriverError> {
        if self.canonical_log_source == CanonicalLogSource::SubscriptionOnly {
            return Err(AmmSubscriberDriverError::EventOnly(
                "adding/replacing sources requires explicit cold restart",
            ));
        }
        Ok(())
    }

    pub(super) fn require_event_only_controls(
        &self,
        batch: &SubscriberInputBatch<Ethereum>,
    ) -> Result<(), AmmSubscriberDriverError> {
        if self.canonical_log_source == CanonicalLogSource::SubscriptionOnly
            && (batch.preconfirmation_invalidated()
                || batch
                    .chain_controls()
                    .iter()
                    .any(|control| matches!(control, ChainControl::Reorg { .. })))
        {
            return Err(AmmSubscriberDriverError::EventOnly(
                "reorg/preview transition requires explicit repair",
            ));
        }
        Ok(())
    }

    pub(super) fn accept_event_only_header(
        &self,
        header: &RpcHeader,
    ) -> Result<bool, AmmSubscriberDriverError> {
        if self.canonical_log_source != CanonicalLogSource::SubscriptionOnly {
            return Ok(true);
        }
        if header.inner.hash_slow() != header.hash {
            return Err(AmmSubscriberDriverError::EventOnly("header hash mismatch"));
        }
        let current = self.runtime.latest_snapshot().point();
        if header.inner.number == current.block_number() && header.hash == current.block_hash() {
            return Ok(false);
        }
        if let Some((pending, _)) = self.pending_headers.get(&header.inner.number) {
            if pending.hash == header.hash {
                return Ok(false);
            }
            return Err(AmmSubscriberDriverError::EventOnly(
                "replacement header requires explicit repair",
            ));
        }
        let parent = header
            .inner
            .number
            .checked_sub(1)
            .ok_or(AmmSubscriberDriverError::EventOnly("invalid header number"))?;
        let expected = if parent == current.block_number() {
            Some(current.block_hash())
        } else {
            self.pending_headers
                .get(&parent)
                .map(|(header, _)| header.hash)
        };
        if expected != Some(header.inner.parent_hash) {
            return Err(AmmSubscriberDriverError::EventOnly(
                "header gap/reorg requires explicit repair",
            ));
        }
        Ok(true)
    }

    pub(super) fn require_event_only_buffer_bound(&self) -> Result<(), AmmSubscriberDriverError> {
        if self.canonical_log_source == CanonicalLogSource::SubscriptionOnly
            && (self.pending_headers.len() > RETAINED_CANONICAL_LINEAGE
                || self.buffered_logs.len() > RETAINED_CANONICAL_LINEAGE
                || self
                    .buffered_logs
                    .values()
                    .map(BTreeMap::len)
                    .sum::<usize>()
                    > self.subscriber.config().max_pending_records)
        {
            return Err(AmmSubscriberDriverError::EventOnly(
                "incomplete source exceeded bounded assembly; explicit repair required",
            ));
        }
        Ok(())
    }

    pub(super) fn require_event_only_log(
        &self,
        log: &RpcLog,
    ) -> Result<(), AmmSubscriberDriverError> {
        if self.canonical_log_source != CanonicalLogSource::SubscriptionOnly {
            return Ok(());
        }
        let number = log.block_number.ok_or(AmmSubscriberDriverError::EventOnly(
            "log has no block number",
        ))?;
        let current = self.runtime.latest_snapshot().point();
        if log.removed
            || number <= current.block_number()
            || self
                .highest_observed_block
                .is_some_and(|observed| number < observed)
            || self
                .pending_headers
                .get(&number)
                .is_some_and(|(header, _)| Some(header.hash) != log.block_hash)
        {
            return Err(AmmSubscriberDriverError::EventOnly(
                "removed, late or conflicting canonical log requires explicit repair",
            ));
        }
        Ok(())
    }

    pub(super) async fn deliver_event_only(
        &mut self,
        header: RpcHeader,
        seal: Option<SealReason>,
        ingress: Instant,
        decoded: Duration,
    ) -> Result<(), AmmSubscriberDriverError> {
        let current = self.runtime.latest_snapshot().point();
        if current.block_number().checked_add(1) != Some(header.inner.number)
            || header.inner.parent_hash != current.block_hash()
        {
            return Err(AmmSubscriberDriverError::EventOnly(
                "canonical gap/reorg requires explicit repair",
            ));
        }
        let reason = seal.ok_or(AmmSubscriberDriverError::EventOnly(
            "incomplete stream cannot be reconciled implicitly",
        ))?;
        let (number, hash) = (header.inner.number, header.hash);
        self.seal_from_subscription(header, reason, ingress, decoded)
            .await?;
        self.record_canonical_block(number, hash);
        self.retire_buffered_logs_through(number);
        Ok(())
    }
}
