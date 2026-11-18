//! Bounded, exact-block NFT evidence reads before canonical publication.
use super::{RUNTIMES, StakingFamily, StakingRuntime, abi, aero_abi, canonical_event};
use alloy_primitives::{B256, U256};
use alloy_rpc_types_eth::{Filter, Log};
use alloy_sol_types::SolEvent;
use std::collections::BTreeSet;

fn stake_token(runtime: StakingRuntime, log: &Log) -> Option<U256> {
    match runtime.family {
        StakingFamily::Legacy => canonical_event::<abi::Deposit>(&log.inner)
            .map(|event| event.data.tokenId)
            .or_else(|| {
                canonical_event::<abi::Withdraw>(&log.inner).map(|event| event.data.tokenId)
            }),
        StakingFamily::AeroCl => canonical_event::<aero_abi::Deposit>(&log.inner)
            .map(|event| event.data.tokenId)
            .or_else(|| {
                canonical_event::<aero_abi::Withdraw>(&log.inner).map(|event| event.data.tokenId)
            }),
    }
}

/// One pair per bounded token chunk per reviewed runtime on `chain`. No gauge
/// transition means no NFT request.
pub(crate) fn evidence_filters<'a>(chain: u64, logs: impl Iterator<Item = &'a Log>) -> Vec<Filter> {
    let runtimes: Vec<StakingRuntime> = RUNTIMES
        .iter()
        .copied()
        .filter(|runtime| runtime.chain == chain)
        .collect();
    if runtimes.is_empty() {
        return Vec::new();
    }
    let mut tokens: Vec<BTreeSet<B256>> = vec![BTreeSet::new(); runtimes.len()];
    for log in logs {
        for (index, runtime) in runtimes.iter().enumerate() {
            if log.address() != runtime.gauge {
                continue;
            }
            if let Some(token) = stake_token(*runtime, log) {
                tokens[index].insert(B256::from(token.to_be_bytes::<32>()));
            }
        }
    }
    runtimes
        .iter()
        .zip(tokens)
        .flat_map(|(runtime, tokens)| {
            let nft = runtime.nft;
            tokens
                .into_iter()
                .collect::<Vec<_>>()
                .chunks(256)
                .flat_map(|tokens| {
                    [
                        Filter::new()
                            .address(nft)
                            .event_signature(abi::Transfer::SIGNATURE_HASH)
                            .topic3(tokens.to_vec()),
                        Filter::new()
                            .address(nft)
                            .event_signature(abi::Collect::SIGNATURE_HASH)
                            .topic1(tokens.to_vec()),
                    ]
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::Address;

    const GAUGE: Address = super::super::LEGACY_GAUGE;
    const NFT: Address = super::super::LEGACY_NFT;

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

    fn aero_gauge(token: u64, deposit: bool) -> Log {
        let owner = Address::repeat_byte(0x42);
        let token_id = U256::from(token);
        let data = if deposit {
            aero_abi::Deposit {
                sender: owner,
                owner,
                tokenId: token_id,
                liquidityToStake: 400,
            }
            .encode_log_data()
        } else {
            aero_abi::Withdraw {
                sender: owner,
                account: owner,
                tokenId: token_id,
                liquidityToStake: 400,
            }
            .encode_log_data()
        };
        Log {
            inner: alloy_primitives::Log {
                address: super::super::AERO_BASE_GAUGE,
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
    fn aero_gauge_events_name_tokens_on_their_own_chain_only() {
        let logs = [aero_gauge(7, true), aero_gauge(8, false)];
        let filters = evidence_filters(8453, logs.iter());
        assert_eq!(filters.len(), 2);
        assert!(filters.iter().all(|f| {
            f.address.iter().copied().collect::<Vec<_>>() == [super::super::AERO_NFT]
        }));
        assert_eq!(filters[0].topics[3].len(), 2);
        assert!(evidence_filters(1, logs.iter()).is_empty());
        assert!(evidence_filters(10, logs.iter()).is_empty());
        // A legacy-shaped payload on the Aero gauge attests nothing.
        let mut legacy_shaped = gauge(7, true);
        legacy_shaped.inner.address = super::super::AERO_BASE_GAUGE;
        assert!(evidence_filters(8453, [&legacy_shaped].into_iter()).is_empty());
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
