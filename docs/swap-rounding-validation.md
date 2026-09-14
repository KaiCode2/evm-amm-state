# V3 swap rounding validation

Candidate base: `v0.3.0`, commit `e05945bc52f5ba145e5b423ca1c4a81f187eb185`.
Prepared version: `0.3.1-alpha.1`. Rounding implementation commit:
`623a48049ee13ddcffc6ee708f3e9d0d5aa12732`.
The AMM manifest and lockfile retain the release dependency resolution, including
`evm-fork-cache 0.4.0`. This change has not been published.

## Arithmetic contract

A final price is a rounded result, not an invertible encoding of the specified
amount. Replay retains each segment's starting and ending prices and liquidity.

- A reduced output must be confined to the final segment, consume positive
  remaining output, reproduce the final price through the protocol's output
  calculation, and use the exact-output fee ceiling on every segment. This also
  permits a cap that reaches an initialized tick. No fee-only continuation is
  admitted after a cap.
- With uncapped output, an excess partial-step fee must reproduce the final price
  from the remaining gross input after discounting it by the fee. Earlier
  segments' principal and fees are excluded from that remainder. A fee-ceiling
  ending still supports exact output and explicit price limits.
- Slipstream's fee search uses the upper input budget that can round to the
  endpoint. Output caps constrain that search to fee-ceiling accounting. Its
  ambiguity rejection, supported runtime identities, and distinction between
  quote-state and accounting-state publication remain intact.

No tolerance, new provider read, configuration flag, or dependency is involved.
The forward helpers retain uint256 overflow behavior and the amount0-input
fallback from [SqrtPriceMath](https://github.com/Uniswap/v3-core/blob/main/contracts/libraries/SqrtPriceMath.sol).
Mode coupling follows [SwapMath](https://github.com/Uniswap/v3-core/blob/main/contracts/libraries/SwapMath.sol).

## Independent references and regressions

`v3_swap_transition_differential` executes the embedded deployed pool runtime in
revm. Only tokens and the callback harness are synthetic. Its checked-in Plasma
fixture confirms that immutable patching produces the same runtime code hash as
the actual Plasma pool. Slipstream tests execute the two already-supported proxy
and implementation runtimes with deterministic factory fee getters.

The original four V3/Slipstream differential and acceptance test targets passed
before edits. New cases then failed on the unchanged checks:

| Case | Observed old-code rejection | Reference |
| --- | --- | --- |
| Plasma exact-input geometry | Final-step fee contradiction, 1,037 wei of budget/principal slack | Pool bytecode |
| Capped outputs in both directions | Derived 198, event 197 | Pool bytecode |
| Capped initialized-boundary outputs, both directions | Derived 200, event 199 | Pool bytecode |
| Slipstream exact-input geometry | Fee inference: no match | Both supported runtimes in the passing candidate matrix |
| Slipstream output cap | Derived 19,999,998, event 19,999,997 | Both supported runtimes in the passing candidate matrix |

The expanded corpus also covers prior initialized steps, protocol-fee allocation,
low/high asymmetric prices, both exactness modes, supported fee levels, ordered
sequences, explicit limits, tiny amounts, and existing oracle/crossing cases.
Comparisons include declared slots, reference-accessed slots, and adapter-written
slots, including extra writes. Negative cases require a contradiction and verify
that rejection exposes only the existing purge/repair behavior, never partial
exact writes. A specific extra-fee mutation guards against combining exact-input
fee slack with a capped output.

The former private `valid_partial_fee` tests assumed that discounting input
exactly recovers principal. That helper and those assumptions are replaced by
forward-price validation and runtime-generated cases. A synthetic test of an
arbitrary tick endpoint now explicitly covers fee-ceiling price-limit endings.

## Historical Plasma fixture

`tests/fixtures/plasma_exact_input_rounding.json` contains the exact-input case:

- Chain 9745, pool `0x2a4a9a6c89f2de942c8e2938c1d8495495720a17`, fee 500, spacing 10.
- Event block 32451176, hash
  `0x266d0d20b41dbf50e382cff613b7bd151adf16b1d83b80afc8e0b93b4ff35ece`.
- Parent hash `0xf20d30e5aec1b115d8b3030ab7a13702356bfe92489afacbb89b4a7fee71d6ce`.
- Transaction `0x5548f995bd836926932331ab0403abb835e20119a3f7563b23679280b5239001`,
  transaction index 1, log index 2.
- Exact transaction prestate and poststate from `debug_traceTransaction` with
  `prestateTracer`, both diff modes; receipt and pool logs pinned by block hash.
- The two untouched accounting slots omitted by the trace, read with
  `eth_getStorageAt` at the canonical event hash. The block contains one pool log.

`tests/fixtures/plasma_exact_output_rounding.json` captures a separately located
exact-output event with 976 wei of cap slack:

- Block 32449079, hash `0xf10ffc2a66b1c25d07057cccb882c18e1b13271c22cfdb13845c969e055f3cc0`.
- Parent hash `0x63cc14c0b74e0c764d2d8f019a49b361e7572a35754050c55a91bfdc22651302`.
- Transaction `0x506825d3d837e712426318891a7dfb2a728fa6f87573ef1692cf79bf904a45ef`,
  transaction index 0, log index 3, same pool/fee/spacing.
- Output 605,200,000,000,000,000,000 wei of token0; reconstructed output exceeds
  it by 976 wei. Exact transaction prestate/poststate uses the same trace method.
  Untouched accounting slots 1 and 3 were supplemented at the canonical event
  hash; this block also has exactly one pool log.

Capture used public `https://rpc.plasma.to` on 2026-09-14. This fixture is offline;
CI does not fetch it. The original reported output mismatches of 1,545 and 2 wei
had no transaction identities; no historical provenance is claimed for them.

## Consumer comparison

Isolated consumer checkout: `fix/verify-v3-swap-rounding`, based on
`b58036676db73c83d4348f074567ee9977cbda50`.

The stable baseline changes the original alpha pins to exact AMM `0.3.0` and cache
`0.4.0`. Its existing `simulation_pipeline` suite passes all six tests. The new
`plasma_swap_rounding` test fails there with the documented fee contradiction.
The candidate resolves only AMM through a compatible local path patch; cache and
other lockfile dependencies stay fixed.

Both consumer fixtures additionally contain the real quoter's hash-pinned
`eth_call` output and `debug_traceCall` read set. The regression feeds the captured
swap through the consumer registry and AMM reactive handler, checks exact quality,
no resync/repair/invalidation, and trace poststate, then calls the production
`SnapshotQuoter`. Pool output, final price and initialized ticks must equal the
RPC reference; the offline provider sentinel must remain untouched.

Both historical consumer cases pass with the candidate, as do all six original
simulation tests. Pool replay starts at the transaction prestate; other quote
accounts come from the post-block quoter read set. This is pool-event ingestion
followed by a quote, not a replay of the complete original transaction.

A 120-second live observation compared 357 pool states over 119 observed blocks,
including 56 swaps, 3 mints and 5 burns. It found zero mismatches, zero skipped
pool states under repair, zero generation rebuilds, and zero seconds without
published state. There were observation sampling gaps, so this is not a claim to
have compared every block. Live comparisons cover price, tick and liquidity;
fee/accounting correctness is established by the offline trace and bytecode
comparisons above, not by this live test.

## Verification record

Local platform: macOS arm64. Crate checks use Rust 1.90.0; consumer checks use
Rust 1.98.0. All final checks below passed.

| Check | Result |
| --- | --- |
| `cargo test --locked --all-features` | 595 passed, 27 intentionally ignored |
| `cargo test --locked` | 484 passed, 27 intentionally ignored |
| `cargo test --locked --no-default-features` | 115 passed |
| Clippy, all targets, all features / no default features | Clean with `-D warnings` |
| Format, diff, authoring hygiene | Passed |
| `cargo check --locked --all-targets --no-default-features --features live-runtime` | Passed on Rust 1.90 |
| `cargo doc --locked --all-features --no-deps` | Passed with documentation warnings denied |
| `cargo package --locked --allow-dirty` | Packaged and verified successfully |
| Consumer `simulation_pipeline` | Six passed on stable and candidate |
| Consumer `plasma_swap_rounding` | Both historical cases passed on candidate |
| Consumer Clippy for the new test target | Clean with `-D warnings` |

The ignored crate tests include network-dependent checks and the manual timing
test. Targeted live coverage is reported separately above. The separate live
first-event diagnostic replayed exact swaps with no repair on the 0.05% and 0.30%
pools, then reached its 240-second bound without an event from the 1% pool; that
third pool was not exercised by this diagnostic.

Builds used `CARGO_BUILD_JOBS=2`, `CARGO_INCREMENTAL=0`,
`CARGO_PROFILE_DEV_DEBUG=0`, and `CARGO_PROFILE_TEST_DEBUG=0`. Package verification
was paused once for low disk space; completed test executables from this task
were removed, and the final package verification succeeded.

### Replay timing

The existing offline benchmarks cover mint/burn and other protocols, not V3
swap replay, so `offline_swap_replay_timing` adds a manual measurement of the
existing Ethereum acceptance fixture. Both versions use identical harnesses,
compiler, flags, and 7 samples of 10,000 events. The binaries were saved separately
and alternated after the builds finished:

| Trial order | Minimum / median / maximum, microseconds per event |
| --- | --- |
| Baseline | 101.838 / 164.382 / 287.055 |
| Candidate | 100.659 / 104.691 / 115.666 |
| Candidate | 98.749 / 108.080 / 138.180 |
| Baseline | 101.829 / 109.158 / 191.122 |

An earlier compiler-active comparison measured medians of 132.983 microseconds
on baseline and 292.230 on candidate, with substantial variation. That apparent
slowdown did not reproduce in the alternating trials. These unoptimized,
shared-workstation timings show no repeatable regression in this replay case;
they are not production p95 measurements or a benchmark of every swap shape.

Raw check logs, the live CSV, and the alternating timing records are retained in
`validation-artifacts` alongside the isolated worktrees. The arithmetic fixtures
and their provenance are checked into the test suite.

Owner delivery, recovery scheduling, beneficiary hydration, and TLS changes remain
outside this patch. A live interruption from those paths is recorded separately
and cannot count as successful rounding coverage.
