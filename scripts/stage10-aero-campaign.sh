#!/usr/bin/env bash
# Long-window exact replay + strict-quote campaign for the reviewed Aero pools.
#
# Required: E2E_RPC_URL (Ethereum archive; Alchemy host names derive the Base
# and Optimism endpoints unless E2E_BASE_RPC_URL / E2E_OPTIMISM_RPC_URL are set).
# Optional: AERO_CAMPAIGN_STRIDE (default 100), AERO_CAMPAIGN_ETH_BLOCKS
# (default 20000), AERO_CAMPAIGN_L2_BLOCKS (default 100000, applied to Base and
# Optimism unless AERO_CAMPAIGN_{BASE,OP}_BLOCKS override it),
# AERO_CAMPAIGN_{ETH,BASE,OP}_{START,END}, AERO_CAMPAIGN_QUOTES (default 1),
# AERO_CAMPAIGN_REPORT_DIR (default target/aero-campaign),
# AERO_CAMPAIGN_CHAINS (default "ethereum base optimism").
set -euo pipefail

state_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

if [[ -z "${E2E_RPC_URL:-}" ]]; then
  echo "stage10 Aero campaign: E2E_RPC_URL (Ethereum archive) is required" >&2
  exit 2
fi

echo "stage10 Aero campaign: endpoint present (value redacted)"

export AERO_CAMPAIGN_BASE_BLOCKS="${AERO_CAMPAIGN_BASE_BLOCKS:-${AERO_CAMPAIGN_L2_BLOCKS:-100000}}"
export AERO_CAMPAIGN_OP_BLOCKS="${AERO_CAMPAIGN_OP_BLOCKS:-${AERO_CAMPAIGN_L2_BLOCKS:-100000}}"

redact_stream() {
  perl -pe '
    BEGIN {
      @secrets = grep { defined($_) && length($_) }
        @ENV{qw(E2E_RPC_URL E2E_BASE_RPC_URL E2E_OPTIMISM_RPC_URL)};
    }
    for $secret (@secrets) {
      s/\Q$secret\E/<redacted-endpoint>/g;
    }
    s#(?:https?|wss?)://[^\s,]+#<redacted-endpoint>#g;
  '
}

cd "$state_dir"

for chain in ${AERO_CAMPAIGN_CHAINS:-ethereum base optimism}; do
  case "$chain" in
    ethereum) test_name=ethereum_aero_cl_campaign ;;
    base) test_name=base_aero_cl_campaign ;;
    optimism) test_name=optimism_aero_volatile_campaign ;;
    *) echo "unknown chain $chain" >&2; exit 2 ;;
  esac
  cargo test --locked --release --all-features --test aero_campaign_rpc "$test_name" -- \
    --ignored --nocapture --test-threads=1 2>&1 | redact_stream
done

echo "stage10 Aero campaign: reports in ${AERO_CAMPAIGN_REPORT_DIR:-target/aero-campaign}"
