# Releasing `evm-amm-state`

The current release candidate is `0.3.0-alpha.7`. Publish its prerequisites in
this order:

1. `alloy-transport-balancer 0.3.0-alpha.2`;
2. `evm-fork-cache 0.4.0-alpha.4`;
3. publish `evm-amm-state 0.3.0-alpha.7` after the gates below pass.

The state crate uses an exact registry pin for `evm-fork-cache 0.4.0-alpha.4`.
Treat any failure to resolve that published version as a release blocker rather
than bypassing registry verification.

Alpha.2 includes representative quote read-set learning, exact-canonical
hydration, exact equality between the canonical warm-up quote and its immediate
provider-read-free replay, and retention of generation-local lazy fills across
cumulative overlay replacement. Historical paid QuickNode and Alchemy Base
results, plus the current provider-specific revalidation status, are recorded
in `docs/flashblocks-latency.md`; every release candidate must still repeat the
documented paid-provider gate.

Alpha.3 is a targeted Slipstream correction: native quote calldata uses the
protocol's signed `int24 tickSpacing` field rather than Uniswap V3's `uint24
fee`, and the fast cold-start path accepts complete Slipstream tick-spacing
metadata without inventing a fee.

Alpha.4 combines explicit containment for rejected speculative batches with
canonical baseline lineage initialization for external preview sources. The
subscriber driver remains fail-closed by default, while a `Preferred`
application can discard only `PreconfirmationBatch` data-quality failures and
continue canonical delivery; incompatible `Required` or `Disabled` pairings are
rejected during attachment. The runtime rejects stale, missing-parent, and
wrong-parent previews before they can alter canonical state. Typed rejection
events preserve bounded observability without making raw error text a metric
label.

Alpha.5 adds exact provider-free canonical Uniswap V3 `Swap` replay from a
complete parent snapshot plus ordered block/log context. It reproduces slot0,
active liquidity, fee growth, protocol fees, observation-ring writes, and every
crossed canonical four-word tick record, then validates the event poststate
before atomically applying any write. Missing or contradictory evidence purges
the stale pool and requests repair. Pancake V3 and Slipstream remain explicitly
unsupported for exact swap replay; neither may be promoted from storage-layout
similarity. See `docs/v3-swap-transitions.md` for the capability and evidence
boundary.

Alpha.5 depends on alpha.4's immutable `EvmSnapshot` current block-hash and
address-bound code-hash accessors. Publish and verify that companion version
first. The retained Slipstream evaluator/corpus is research-only and does not
make Base/Optimism Flashblocks execution-ready.

Alpha.6 restores the full-range, layout-only Slipstream quote bootstrap used by
strict provider-free consumers. This is a quote-readiness correction only:
Slipstream event transitions remain unsupported and fail closed exactly as in
alpha.5.

Alpha.7 adds event-only quote/search transitions for the reviewed Base BIFI and
Optimism mooBIFI Slipstream deployments. `Swap`, `Mint`, `Burn`, and `Collect`
advance the state consumed by search without a provider reconstruction; full
fee-growth, gauge, reward, position, and token accounting remain outside that
guarantee unless the separately attested evidence is present. Arbitrary
Slipstream deployments remain unsupported.

The local alpha.7 gate routes the two checked-in historical Swap logs through
the pool-scoped reactive runtime and then executes both quote directions through
the native Slipstream ABI and the real reviewed proxy/implementation bytecode.
It leaves a provider-failure sentinel queued, requires zero invalidations and
resyncs, and fails if any optimized event-to-bid/ask sample reaches one second:

```bash
SLIPSTREAM_E2E_SAMPLES=1000 cargo test --release --locked \
  --test slipstream_swap_transition_acceptance -- --nocapture
```

## Offline release matrix

```bash
cargo fmt --all -- --check
cargo test --locked --all-features
cargo test --locked
cargo test --locked --no-default-features
cargo clippy --locked --all-targets --all-features --no-deps -- -D warnings
cargo clippy --locked --all-targets --no-default-features --no-deps -- -D warnings
cargo check --locked --all-targets --no-default-features --features live-runtime
for f in uniswap-v2 uniswap-v3 pancake-v3 slipstream balancer-v2 solidly-v2 curve; do
  cargo clippy --locked --all-targets --no-default-features --features "adapters,$f" --no-deps -- -D warnings
done
RUSTDOCFLAGS='-D warnings' cargo doc --locked --all-features --no-deps
cargo +1.90.0 check --locked --all-features
bash scripts/check-authoring-hygiene.sh
bash scripts/check-security-exceptions.sh
cargo audit --ignore RUSTSEC-2025-0055
cargo tree -e normal --all-features | grep -E '(amms|amm-math|rayon) v[0-9]' &&
  exit 1 || true
```

Confirm every third-party workflow action and sibling checkout matches the full
commit recorded in `SECURITY.md`. A tag, branch, or unresolved candidate
placeholder is a release blocker.

`RUSTSEC-2025-0055` is narrowly ignored because `ark-relations` records
`tracing-subscriber 0.2.25` only through an inactive optional proof-system
dependency. The scope script fails if that version enters any active target or
feature graph, if another vulnerable version appears, or if the inactive lock
entry disappears without the exception being removed.

Run the offline performance gates:

```bash
cargo bench --locked --bench runtime_lifecycle
cargo bench --locked --bench pool_routing
cargo bench --locked --all-features --bench live_runtime_actor
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
mainnet swap/full-sync/discovery parity, canonical V3 liquidity-event
fail-closed validation, V2 and Curve WebSocket soaks, and the search crate's
headless progressive-route benchmark.
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
cargo package --list --locked
cargo publish --dry-run --locked
```

Warnings about excluded explicit test targets are expected. A dependency
resolution failure for `evm-fork-cache 0.4.0-alpha.4` is not waived; publish and
verify the companion crate first. Do not tag or publish until packaged-source
builds, live gates, downstream compatibility, changelog, and benchmark evidence
pass.

Publishing and tag creation are external state changes and are never performed
as part of an ordinary validation run.
