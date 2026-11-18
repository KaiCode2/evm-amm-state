#!/usr/bin/env bash
set -euo pipefail

state_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

if [[ -z "${E2E_RPC_URL:-}" ]]; then
  echo "stage10 Aero live gates: E2E_RPC_URL (Ethereum archive) is required" >&2
  exit 2
fi

echo "stage10 Aero live gates: endpoint present (value redacted)"

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

# Offline Metaquoter / total-fee quotes against hash-pinned eth_calls and the
# migration evidence values, plus offline closure of the learned read sets.
cargo test --locked --all-features --test aero_parity_rpc -- \
  --ignored --nocapture --test-threads=1 2>&1 | redact_stream

# Event-driven Aero pool state over activity windows of >= 50 blocks on every
# chain, compared against exact storage reads at each event block.
cargo test --locked --all-features --test aero_replay_rpc -- \
  --ignored --nocapture --test-threads=1 2>&1 | redact_stream
