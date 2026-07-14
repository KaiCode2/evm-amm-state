#!/usr/bin/env bash
set -euo pipefail

state_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
search_dir="$(cd "$state_dir/../evm-amm-search" && pwd)"

export E2E_RPC_URL="${E2E_RPC_URL:-${ETH_RPC_URL:-}}"

if [[ -z "${E2E_RPC_URL}" ]]; then
  echo "stage10 live gates: E2E_RPC_URL or ETH_RPC_URL is required" >&2
  exit 2
fi

echo "stage10 live gates: endpoint present (value redacted)"

redact_stream() {
  perl -pe '
    BEGIN {
      @secrets = grep { defined($_) && length($_) } @ENV{qw(E2E_RPC_URL ETH_RPC_URL ETH_WS_URL)};
    }
    for $secret (@secrets) {
      s/\Q$secret\E/<redacted-endpoint>/g;
    }
    s#(?:https?|wss?)://[^\s,]+#<redacted-endpoint>#g;
  '
}

(
  cd "$state_dir"
  mainnet_swap_tests=(
    balancer_simulate_swap_matches_eth_call
    curve_simulate_swap_matches_eth_call
    curve_cryptoswap_simulate_swap_matches_eth_call
    curve_tricrypto_ng_simulate_swap_matches_eth_call
    v2_simulate_swap_matches_eth_call
    v3_simulate_swap_matches_eth_call
  )
  for test_name in "${mainnet_swap_tests[@]}"; do
    cargo test --all-features --test adapter_swap_sim_rpc "$test_name" -- \
      --exact --ignored --nocapture --test-threads=1 2>&1 | redact_stream
  done
  cargo test --all-features --test v3_full_sync_rpc -- \
    --ignored --nocapture --test-threads=1 2>&1 \
    | redact_stream
  cargo test --all-features --test v3_liquidity_rpc -- \
    --ignored --nocapture --test-threads=1 2>&1 \
    | redact_stream
  cargo test --all-features --test balancer_liquidity_rpc -- \
    --ignored --nocapture --test-threads=1 2>&1 \
    | redact_stream
  pancake_discovery_tests=(
    pancake_get_pool_base_slot_matches_getter
    pancake_create2_matches_getter
    pancake_discovery_resolves_live_pool
  )
  for test_name in "${pancake_discovery_tests[@]}"; do
    cargo test --all-features --test discovery_cl_rpc "$test_name" -- \
      --exact --ignored --nocapture --test-threads=1 2>&1 | redact_stream
  done
  cargo test --all-features --test reactive_ws_e2e \
    ws_subscription_health_probe -- \
    --exact --ignored --nocapture --test-threads=1 2>&1 | redact_stream
  E2E_WS_SECONDS="${E2E_CURVE_WS_SECONDS:-180}" \
    cargo test --all-features --test reactive_curve_ws_e2e \
      ws_curve_liquidity_events_flow_route_and_stay_accurate -- \
      --exact --ignored --nocapture --test-threads=1 2>&1 | redact_stream
)

(
  cd "$search_dir"
  AMM_ROUTE_TUI_BENCH=1 \
  AMM_ROUTE_TUI_BENCH_BOOTSTRAP_TIMEOUT_SECS="${AMM_ROUTE_TUI_BENCH_BOOTSTRAP_TIMEOUT_SECS:-120}" \
  AMM_ROUTE_TUI_BENCH_ROUTE_TIMEOUT_SECS="${AMM_ROUTE_TUI_BENCH_ROUTE_TIMEOUT_SECS:-120}" \
  AMM_ROUTE_TUI_BENCH_IDLE_TIMEOUT_SECS="${AMM_ROUTE_TUI_BENCH_IDLE_TIMEOUT_SECS:-0}" \
    cargo run --release --features live-runtime --bin amm-route-tui 2>&1 \
      | redact_stream
)
