//! Bounded, exact-block NFT evidence reads before canonical publication.
use super::{GAUGE, NFT, abi, canonical_event};
use alloy_primitives::{B256, U256};
use alloy_rpc_types_eth::{Filter, Log};
use alloy_sol_types::SolEvent;
use std::collections::BTreeSet;

/// One pair per bounded token chunk. No gauge transition means no NFT request.
pub(crate) fn evidence_filters<'a>(chain: u64, logs: impl Iterator<Item = &'a Log>) -> Vec<Filter> {
    if chain != 10 {
        return Vec::new();
    }
    let tokens: BTreeSet<B256> = logs
        .filter(|log| log.address() == GAUGE)
        .filter_map(|log| {
            canonical_event::<abi::Deposit>(&log.inner)
                .map(|event| event.data.tokenId)
                .or_else(|| {
                    canonical_event::<abi::Withdraw>(&log.inner).map(|event| event.data.tokenId)
                })
        })
        .map(|token: U256| B256::from(token.to_be_bytes::<32>()))
        .collect();
    tokens
        .into_iter()
        .collect::<Vec<_>>()
        .chunks(256)
        .flat_map(|tokens| {
            [
                Filter::new()
                    .address(NFT)
                    .event_signature(abi::Transfer::SIGNATURE_HASH)
                    .topic3(tokens.to_vec()),
                Filter::new()
                    .address(NFT)
                    .event_signature(abi::Collect::SIGNATURE_HASH)
                    .topic1(tokens.to_vec()),
            ]
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::Address;

    fn gauge(token: u64, deposit: bool) -> Log {
        let user = Address::repeat_byte(0x42);
        let token_id = U256::from(token);
        let liquidity = 400;
        let data = if deposit {
            abi::Deposit {
                user,
                tokenId: token_id,
                liquidityToStake: liquidity,
            }
            .encode_log_data()
        } else {
            abi::Withdraw {
                user,
                tokenId: token_id,
                liquidityToStake: liquidity,
            }
            .encode_log_data()
        };
        Log {
            inner: alloy_primitives::Log {
                address: GAUGE,
                data,
            },
            ..Default::default()
        }
    }

    #[test]
    fn exact_tokens_for_both_directions_without_shared_manager_feed() {
        let logs = [gauge(7, true), gauge(8, false), gauge(7, false)];
        let filters = evidence_filters(10, logs.iter());
        assert_eq!(filters.len(), 2);
        assert!(
            filters
                .iter()
                .all(|f| f.address.iter().copied().collect::<Vec<_>>() == [NFT])
        );
        assert_eq!(filters[0].topics[3].len(), 2);
        assert_eq!(filters[1].topics[1], filters[0].topics[3]);
        assert!(evidence_filters(8453, logs.iter()).is_empty());
        assert!(evidence_filters(10, [].iter()).is_empty());
        let mut unrelated = gauge(9, true);
        unrelated.inner.address = Address::repeat_byte(0x77);
        assert!(evidence_filters(10, [&unrelated].into_iter()).is_empty());
    }

    #[test]
    fn bounded_chunks_and_malformed_events_do_not_attest_tokens() {
        let logs: Vec<_> = (0..257).map(|token| gauge(token, true)).collect();
        let filters = evidence_filters(10, logs.iter());
        assert_eq!(filters.len(), 4);
        assert_eq!(filters[0].topics[3].len(), 256);
        assert_eq!(filters[2].topics[3].len(), 1);
        let mut malformed = gauge(7, true);
        malformed.inner.data.data = vec![0].into();
        assert!(evidence_filters(10, [&malformed].into_iter()).is_empty());
    }
}
