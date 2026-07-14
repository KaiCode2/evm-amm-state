#!/usr/bin/env bash
set -euo pipefail

state_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

if [[ -z "${E2E_BASE_RPC_URL:-}" ]]; then
  echo "stage10 Base live gates: E2E_BASE_RPC_URL is required" >&2
  exit 2
fi

echo "stage10 Base live gates: endpoint present (value redacted)"

redact_stream() {
  perl -pe '
    BEGIN { $secret = $ENV{E2E_BASE_RPC_URL} // ""; }
    s/\Q$secret\E/<redacted-endpoint>/g if length($secret);
    s#(?:https?|wss?)://[^\s,]+#<redacted-endpoint>#g;
  '
}

cd "$state_dir"

cargo test --all-features --test adapter_swap_sim_rpc \
  solidly_simulate_swap_matches_eth_call -- \
  --exact --ignored --nocapture --test-threads=1 2>&1 | redact_stream

cargo test --all-features --test discovery_cl_rpc \
  slipstream_get_pool_base_slot_matches_getter -- \
  --exact --ignored --nocapture --test-threads=1 2>&1 | redact_stream

cargo test --all-features --test discovery_solidly_rpc -- \
  --ignored --nocapture --test-threads=1 2>&1 | redact_stream
