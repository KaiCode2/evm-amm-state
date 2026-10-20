# Releasing `evm-amm-state`

The `0.3.1-alpha.6` release source is based on published
`0.3.1-alpha.5`. It adds generic optional consumed receipt/assembly provenance and
an explicit default-off driver builder. Existing canonical state, quote, provider,
reconciliation and execution-clock behavior is unchanged. See
`docs/canonical-input-timing.md` for APIs, bounds and disabled costs.

This release requires exact registry `evm-fork-cache = "=0.4.1"` in normal and
dev dependencies. Cache 0.4.1 is published and its archive was verified against
commit `8726fe3f2d6b653a912e9f4af84a7f113d1db5da`, checksum
`d5be321d4ab546df56ef8f765ae0401f8352c44be4a6c763ac1efea277b0ce7c`.
The temporary Git patch is removed. Qualify the unchanged release matrix and
packaged consumer against this real registry dependency before publishing AMM.
Check the registry version immediately before authorized publication; an
ambiguous upload must be resolved by inspecting its archive before retrying.

Consumers must explicitly select `=0.3.1-alpha.6` after it exists in the registry.
Both packages add generic data-only telemetry APIs, not metrics exporters or bot
policy. No new toolchain/runtime/provider capability is required. Broad protocol
or live-provider qualification is not inferred from these telemetry tests; retain
all existing gates and the explicit Flashblocks limitations below.

Neither package publishing, tag creation, production deployment nor activation
is authorized by candidate preparation.

## Stable baseline publication

The `0.3.0` prerequisites were published in this order:

1. `alloy-transport-balancer 0.3.0`;
2. `evm-fork-cache 0.4.0`;
3. regenerate this crate's `Cargo.lock` against the published
   `evm-fork-cache 0.4.0` (`cargo update -p evm-fork-cache`), which
   cannot resolve until step 2 lands;
4. publish `evm-amm-state 0.3.0` after the gates below pass.

That historical baseline resolved cache 0.4.0. The current release must resolve
`evm-fork-cache 0.4.1` from crates.io before release.
Candidate integration may use an external patch configuration; neither that
configuration nor a path dependency may ship. A registry resolution failure
remains a release blocker.

Alpha.2 includes representative quote read-set learning, exact-canonical
hydration, exact equality between the canonical warm-up quote and its immediate
provider-read-free replay, and retention of generation-local lazy fills across
cumulative overlay replacement. Historical paid QuickNode and Alchemy Base
results, plus the current provider-specific revalidation status, are recorded
in `docs/flashblocks-latency.md`. For `0.3.0`, Flashblocks remains an opt-in,
in-development integration outside the qualified canonical release scope. The
September 5 paid-provider checks did not pass. Preserve those results and label
the feature consistently in the README, changelog, and release announcement;
they do not block publication of the validated canonical stack. Before claiming
Flashblocks readiness or a latency advantage in a future release, repeat and
pass the documented paid-provider gates without relaxing their requirements.

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
similarity. See `docs/v3-event-transitions.md` for the capability and evidence
boundary.

Alpha.5 depends on alpha.4's immutable `EvmSnapshot` current block-hash and
address-bound code-hash accessors. Publish and verify that companion version
first. The retained Slipstream evaluator/corpus is research-only and does not
make Base/Optimism Flashblocks execution-ready.

Historical `0.3.0-alpha.6` restores the full-range, layout-only Slipstream quote bootstrap used by
strict provider-free consumers. This is a quote-readiness correction only:
Slipstream event transitions remain unsupported and fail closed exactly as in
alpha.5.

Historical `0.3.0-alpha.7` adds event-only quote/search transitions for the reviewed Base BIFI and
Optimism mooBIFI Slipstream deployments. `Swap`, `Mint`, `Burn`, and `Collect`
advance the state consumed by search without a provider reconstruction; full
fee-growth, gauge, reward, position, and token accounting remain outside that
guarantee unless the separately attested evidence is present. Arbitrary
Slipstream deployments remain unsupported.

The local `0.3.0-alpha.7` gate routes the two checked-in historical Swap logs through
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

Confirm every third-party workflow action and live integration checkout matches
the full commit recorded in `SECURITY.md`. A tag, branch, or unresolved candidate
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
The scheduled/manual `.github/workflows/live.yml` job invokes the mainnet
runner only. Run the separate Base runner explicitly; the workflow does not
satisfy that gate. Both runners fail instead of silently skipping when their
required endpoint is absent.

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
resolution failure for `evm-fork-cache 0.4.1` is not waived; publish and
verify the companion crate first. Do not tag or publish until packaged-source
builds, canonical live gates, downstream compatibility, changelog, and benchmark
evidence pass. Flashblocks is governed by the explicit in-development scope
above; its failed live checks must remain visible in the release material.

Publishing and tag creation are external state changes and are never performed
as part of an ordinary validation run.

## Optional ingress timing feature

The `ingress-timing` Cargo feature is disabled by default and forwards only
`evm-fork-cache/ingress-timing`. Without it the new public input timing DTO,
accessors, configuration flag, inline metadata, ledger and clockwork are absent.
Legacy `AmmCommitTiming` methods and timing behavior remain available unchanged.
Enabling the Cargo feature still requires `with_input_timing(true)` on the driver
for collection. An application-owned cache receipt lookup is additionally needed
for transport-receipt coverage; without a lookup, driver/assembly clocks can be
collected while consumed receipt origins remain explicitly unknown.

CI exercises default/no-default/enabled configurations, verifies the default
resolved graph does not enable either crate's ingress feature, and checks API
absence and original commit-record layout with timing disabled. Enabled tests
retain capacity, duplicate, content-conflict and exact-input provenance coverage.
No exporter, subscriber, global setup or runtime dependency is introduced.
