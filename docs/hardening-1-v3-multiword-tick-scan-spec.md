# Hardening #1 — Uniswap V3 multi-word adaptive tick scan (cold-start)

Manager-owned spec. Implementation agent builds to it; manager authors the
acceptance tests and reviews.

## Outcome

Extend `UniswapV3ColdStartPlanner` (`src/adapters/uniswap_v3.rs`) so cold-start
warms a **bounded window of neighbouring tick-bitmap words** (and their
initialized ticks), not just the single current-tick word. This makes
moderate tick-crossing swaps fully offline-pre-warmed instead of relying on
`EvmCache`'s lazy backend fetch.

## Current state

3 rounds today: R1 `slot0`+`liquidity` (decodes current tick → current word
`W0`); R2 (Strict/Eager) the single current word's bitmap; R3 the `{0,3}`
`Tick.Info` slots of ticks initialized in `W0`. `HotSlotsOnly` = R1 only;
`Lazy` = R1 + defers the single bitmap word. Neighbouring words are never warmed.

## Scope / design

- Add a bounded radius constant, e.g. `const V3_TICK_WORD_RADIUS: i16 = 2;`
  (window = `[W0 - R, W0 + R]`, i.e. `2R+1` words). Document the rationale (one
  word covers 256 tick-spacings; R=2 covers ±2 words — generous for moderate
  swaps while staying bounded; a true outward-adaptive scan is a future
  refinement). Keep it a single named const, easy to tune.
- **R1 (Slot0Liquidity):** unchanged warm of `slot0`+`liquidity`; from the warmed
  `slot0` decode `W0`, then compute the window of words `[W0-R .. W0+R]`, clamped
  to the valid V3 word range for the pool's `tick_spacing` (derive from
  `MIN_TICK`/`MAX_TICK = ±887272`; skip words/ticks outside). Resolve each word's
  bitmap key.
- **R2 (BitmapWord, Strict/Eager):** verify **all** window bitmap keys in one
  round (replaces the single-key verify). Store the window as
  `Vec<(i16 word, U256 key)>`.
- **R3 (TickInfo, Strict/Eager):** for **each** warmed window word, read its
  bitmap value, extract initialized ticks (bit `i` set ⇒ tick
  `(word*256 + i) * tick_spacing`, skipping any tick outside `[MIN_TICK, MAX_TICK]`),
  and collect the `{0,3}` `Tick.Info` slots across the whole window. Verify them
  in one round. Empty ⇒ Done.
- **Policy:** `Strict`/`Eager` do the windowed scan (same radius). `HotSlotsOnly`
  unchanged (R1 only — no bitmap/tick warming). `Lazy` unchanged in spirit but
  defers the **window** of bitmap words (`DeferredWork::VerifySlots` over all
  window bitmap keys) instead of one.
- `slot0`-cold path (`NeedsRepair(VerifySlots(slot0))` / Degraded) unchanged.
  Config metadata still preserved (V3 is "preserve").

## Non-goals

- True outward-adaptive scan (scan until N consecutive empty words) — future.
- Changing the reactive path, the sim, or any other adapter.
- No new production dependencies. No public-API change beyond the planner
  internals (the const may be `pub(crate)`).

## Edge cases

- Word arithmetic must not overflow `i16`/`i32`: clamp the window to the valid
  word range; compute tick indices in `i32` and skip ticks outside `±887272`.
- `tick_spacing` other than 60 (e.g. 10) — window math is in word units, so it
  generalises; the test uses 60.
- A window word whose bitmap is `0` contributes no ticks (fine).
- Bounded guarantee: never verify more than `2R+1` bitmap words + their ticks.

## Acceptance criteria (manager tests in `tests/cold_start_adoption.rs`)

These must pass UNMODIFIED:
1. **Neighbour warm-up** (`v3_cold_start_warms_neighbouring_tick_words`): an
   Eager cold-start of a V3 pool with initialized ticks in `W0`, `W0-1`, `W0+1`
   warms the **bitmap-word slots of `W0±1`** and the **`Tick.Info` slots of the
   neighbour-word ticks** (currently `None` ⇒ red), while still warming `W0`'s
   bitmap + ticks (regression). Reaches `Ready`.
2. **HotSlotsOnly unchanged** (`v3_cold_start_hot_slots_only_skips_tick_words`):
   under `HotSlotsOnly`, neighbour (and current) bitmap words are NOT warmed —
   only `slot0`+`liquidity`. Pins the policy boundary.
3. The existing `v3_cold_start_ready_warms_slot0_and_liquidity`,
   `v3_cold_start_missing_layout_is_unsupported`,
   `v3_cold_start_failed_slot0_needs_repair`, and the `adapter_reactive`/`pipeline_e2e`
   suites still pass.

## Verification

```
CARGO_NET_GIT_FETCH_WITH_CLI=true cargo test --test cold_start_adoption --test adapter_reactive --test pipeline_e2e --test adapter_a1
cargo fmt --all --check
cargo clippy --all-targets --no-deps -- -D warnings
cargo clippy --no-default-features --all-targets --no-deps -- -D warnings
cargo test && cargo test --no-default-features
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps
```

## Assumptions

- `MIN_TICK`/`MAX_TICK = ±887272` (Uniswap V3 constants); add them locally if not
  present. Radius `R=2` unless the implementer finds a clearly better bounded
  default (justify any change to the manager).
