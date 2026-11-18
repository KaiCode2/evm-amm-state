//! Bounded header-only metadata for the live test; never reads reserve state.
use std::collections::HashMap;

use alloy_primitives::B256;
use evm_fork_cache::reactive::{ReactiveBatchReport, ReactiveReport};

use super::*;

pub(super) const MAX_HEADER_READS: usize = 64;

#[derive(Default)]
pub(super) struct Headers {
    verified: HashMap<B256, alloy_rpc_types_eth::Header>,
    pub(super) requests: usize,
}

impl Headers {
    async fn header(
        &mut self,
        provider: &RootProvider<AnyNetwork>,
        hash: B256,
    ) -> Result<alloy_rpc_types_eth::Header> {
        if let Some(header) = self.verified.get(&hash) {
            return Ok(header.clone());
        }
        anyhow::ensure!(
            self.requests < MAX_HEADER_READS,
            "header metadata read cap exceeded"
        );
        self.requests += 1;
        let full = tokio::time::timeout(Duration::from_secs(3), provider.get_block_by_hash(hash))
            .await
            .context("header metadata timed out")??
            .context("event header unavailable")?;
        let header = full
            .header
            .clone()
            .try_map(|header| header.try_into_header())
            .map_err(|_| anyhow!("header is not a complete Ethereum header"))?;
        anyhow::ensure!(
            header.hash == hash && header.inner.hash_slow() == hash,
            "header hash does not authenticate its ancestry"
        );
        self.verified.insert(hash, header.clone());
        Ok(header)
    }

    pub(super) async fn records(
        &mut self,
        provider: &RootProvider<AnyNetwork>,
        previous: Option<BlockRef>,
        log: &RpcLog,
    ) -> Result<(Vec<ReactiveInputRecord<Ethereum>>, ReactiveContext)> {
        anyhow::ensure!(
            !log.removed,
            "removed log: live soak cannot claim an unchanged canonical window"
        );
        let hash = log.block_hash.context("Sync missing block hash")?;
        let header = self.header(provider, hash).await?;
        let ctx = ctx_from_log(log, block_ref(&header))?;
        let mut intervening = Vec::new();
        let mut cursor = header;
        if let Some(previous) = previous {
            anyhow::ensure!(
                cursor.number >= previous.number,
                "out-of-order canonical log"
            );
            if cursor.number == previous.number {
                anyhow::ensure!(
                    cursor.hash == previous.hash,
                    "same-height canonical replacement"
                );
            } else {
                while cursor.number > previous.number + 1 {
                    let parent = self.header(provider, cursor.parent_hash).await?;
                    anyhow::ensure!(
                        parent.number.checked_add(1) == Some(cursor.number),
                        "header ancestry skipped a height"
                    );
                    cursor = parent.clone();
                    intervening.push(parent);
                }
                anyhow::ensure!(
                    cursor.parent_hash == previous.hash,
                    "header ancestry does not descend from retained state"
                );
            }
        }
        // These are real header inputs, not an invented log or completeness
        // certificate. They supply ancestry only; final quote parity still tests
        // whether the stream supplied the reserve changes required at N.
        let records = intervening
            .into_iter()
            .rev()
            .map(|header| {
                let block = block_ref(&header);
                let context = ReactiveContext {
                    chain_id: Some(1),
                    source: InputSource::Synthetic,
                    chain_status: ChainStatus::Included {
                        block,
                        confirmations: 0,
                    },
                    block: Some(block),
                    transaction_index: None,
                    log_index: None,
                };
                ReactiveInputRecord::new(ReactiveInput::BlockHeader(header), context)
            })
            .collect();
        Ok((records, ctx))
    }
}

fn block_ref(header: &alloy_rpc_types_eth::Header) -> BlockRef {
    BlockRef {
        number: header.number,
        hash: header.hash,
        parent_hash: Some(header.parent_hash),
        timestamp: Some(header.timestamp),
    }
}

pub(super) fn require_no_recovery(report: &ReactiveBatchReport<Ethereum>) -> Result<()> {
    anyhow::ensure!(
        report.resyncs.is_empty(),
        "live V2 soak requested state resync"
    );
    for report in &report.reports {
        let reason = match report.as_ref() {
            ReactiveReport::Reorg(_) => Some("canonical rollback"),
            ReactiveReport::MissedBlockRange(_) => Some("uncertified block gap"),
            ReactiveReport::Health(_) => Some("runtime health transition"),
            ReactiveReport::CoverageGap(_) => Some("state coverage gap"),
            ReactiveReport::Error(_) => Some("reactive input error"),
            _ => None,
        };
        if let Some(reason) = reason {
            anyhow::bail!("live V2 soak rejected {reason}; preserve the failed attempt");
        }
    }
    Ok(())
}
