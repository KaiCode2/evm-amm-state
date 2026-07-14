# Releasing `evm-amm-state`

The current release candidate is `0.2.0`. Publish in this order:

1. `alloy-transport-balancer 0.2.0`;
2. `evm-fork-cache 0.3.0`;
3. `evm-amm-state 0.2.0`;
4. `evm-amm-search 0.1.0`.

The state crate intentionally keeps a versioned sibling path during development.
Packaging removes the path and resolves `evm-fork-cache 0.3.0` from crates.io.
Until that version is published, state packaging is expected to stop at
dependency resolution.

## Offline release matrix

```bash
cargo fmt --all -- --check
cargo test --all-features
cargo test
cargo test --no-default-features
cargo clippy --all-targets --all-features -- -D warnings
cargo clippy --all-targets --no-default-features -- -D warnings
cargo check --all-targets --no-default-features --features live-runtime
for f in uniswap-v2 uniswap-v3 pancake-v3 slipstream balancer-v2 solidly-v2 curve; do
  cargo clippy --all-targets --no-default-features --features "adapters,$f" -- -D warnings
done
RUSTDOCFLAGS='-D warnings' cargo doc --all-features --no-deps
cargo +1.88 check --all-features
cargo audit
cargo tree -e normal --all-features | grep -E '(amms|amm-math|rayon) v[0-9]' &&
  exit 1 || true
```

Run the offline performance gates:

```bash
cargo bench --bench runtime_lifecycle
cargo bench --bench pool_routing
cargo bench --all-features --bench live_runtime_actor
```

## Live gates

Live release validation must fail closed when explicitly requested without an
endpoint. Load a private environment without printing it and map the configured
HTTP endpoint to `E2E_RPC_URL`:

```bash
set -a; . ../evm-amm-search/.env; set +a
export E2E_RPC_URL="${E2E_RPC_URL:-${ETH_RPC_URL:-}}"
./scripts/stage10-live-gates.sh
```

The runner refuses to start without a configured endpoint and executes pinned
mainnet swap, liquidity-event, full-sync, and discovery parity; V2 and Curve
WebSocket soaks; and the search crate's headless progressive-route benchmark.
The release runner stops after the first usable route. Full background idle is
a separately recorded provider-sensitive diagnostic; opt into it with
`AMM_ROUTE_TUI_BENCH_IDLE_TIMEOUT_SECS=<seconds>` without turning prolonged
background discovery into a first-usability failure.
Base-specific Solidly/Slipstream parity is a separate mandatory release gate:

```bash
export E2E_BASE_RPC_URL=<private-base-archive-url>
./scripts/stage10-base-live-gates.sh
```

That runner also fails closed when the Base endpoint is absent. An Ethereum
endpoint is never treated as Base.
The scheduled/manual `.github/workflows/live.yml` job invokes the same runner,
so an explicitly requested CI run fails instead of silently skipping when its
paid endpoint secret is absent.

Record provider class, block, cache mode, sample count, median, p95, maximum,
failures, and retries. Never record the endpoint.

The final 2026-07-12 mainnet TUI capture used a fresh cache and the bounded
stable-baseline startup contract: `77` ready pools, handles at `12.276s`, first
route at `12.280s`, and zero transport/RPC/runtime failures. Direct USDC to WETH
quotes succeeded for Pancake V3 (`4/0`), Sushi V3 (`4/0`), Uniswap V3 (`4/0`),
and V2 (`1/0`). This replaces the earlier post-ready basket measurement, whose
in-flight V3 work could become stale after subscriber attachment.

## Packaging

```bash
cargo package --list
cargo publish --dry-run
```

Warnings about excluded explicit test targets are expected. A dependency
resolution failure for `evm-fork-cache 0.3.0` is not waived; publish and verify
the companion crate first. Do not tag or publish until packaged-source builds,
live gates, downstream compatibility, changelog, and benchmark evidence pass.

Publishing and tag creation are external state changes and are never performed
as part of an ordinary validation run.
