# Phase A3 Tech Spec (slice 1): Uniswap V2 Adapter `cold_start`

Status: design approved; implementation pending.

## Goal

Begin ROADMAP Phase A3 (protocol migration) by making the Uniswap V2 adapter a
*complete* adapter: implement `AmmAdapter::cold_start` as a self-contained,
shim-free, synchronous path that brings a V2 pool to `Ready` through the
`AdapterCache` facade, reproducing the legacy cache end-state without the
`LocalAMM`/simulation coupling or the `cache_sync/compat.rs` process-global
sidecar.

## Approved decisions

1. **Storage reads (slots 6/7/8), not view calls.** `cold_start` obtains
   `token0` (slot 6), `token1` (slot 7), and packed reserves (slot 8,
   `V2_RESERVES_SLOT`) by fetching those slots through the facade. This is
   consistent with the adapter layer already being storage-layout-aware (the
   reactive `Sync` masked write hardcodes slot 8; V3 uses `V3StorageLayout`),
   is fully offline-testable, and is shim-free. It assumes the canonical
   Uniswap V2 storage layout; non-canonical clones are out of scope (handled by
   a future layout abstraction or separate adapters).
2. **`cold_start`-only scope.** This slice implements and tests the adapter
   method only. `configured_amms`, the legacy `cache_sync::v2_sync` path, and
   the compat shim are left untouched; rewiring callers and retiring the shim
   come after V3 `cold_start` exists, at the V2/V3 review checkpoint.
3. **Feature model deferred** to the V2/V3 checkpoint (no feature-flag work in
   this slice).

## Runtime grounding (verified against pinned `evm-fork-cache`)

- `AmmAdapter::cold_start(&self, pool: &mut PoolRegistration, cache: &mut dyn
  AdapterCache, policy: ColdStartPolicy) -> Result<ColdStartOutcome>` is
  synchronous; the default returns `Unsupported`.
- `AdapterCache::verify_slots(&[(Address, U256)]) -> Result<Vec<SlotChange>>`
  fetches the requested slots at the pinned block through the cache's
  `storage_batch_fetcher`, treats a cold slot as `old = ZERO`, injects only
  changed slots into the cache, and returns the changed set. It **errors** if no
  batch fetcher is configured. This is the freshness-guaranteeing primitive
  (mirrors the legacy purge-then-refetch) and is offline-testable via
  `set_storage_batch_fetcher`.
- Account/bytecode prefetch (`ensure_account`) is async and a caller concern;
  the storage-focused `cold_start` does not call it.

## Behavior

`UniswapV2Adapter::cold_start`:

1. Resolve `address = pool.key.address()` (must be a `UniswapV2` key).
2. Decide the slot set by `ColdStartPolicy`:
   - `Strict` / `Eager`: verify slots 6, 7, 8.
   - `HotSlotsOnly`: verify slot 8 only.
   - `Lazy`: verify slot 8 now; defer slots 6/7 as
     `DeferredWork::VerifySlots([(addr, 6), (addr, 7)])`.
3. `cache.verify_slots(...)` for the chosen slots (authoritative, hash/block
   pinned). Errors propagate as `Err`.
4. If the reserves slot (8) is still absent after verification, return
   `NeedsRepair(report, RepairAction::VerifySlots([(addr, V2_RESERVES_SLOT)]))`
   — never a silent partial.
5. Read the now-hot slots via `cached_storage`: decode `token0`/`token1`
   (low 20 bytes of slots 6/7) and reserves+`blockTimestampLast` from slot 8.
6. **Merge** into `pool.metadata`: set `token0`/`token1` (and leave any
   config-supplied `fee_bps` intact — V2 has no on-chain fee). Set
   `pool.status = Ready` (or as deferred for `Lazy`).
7. Return `Ready(report)` (or `ReadyWithDeferred(report, deferred)` for `Lazy`),
   with `report.verified_slots` / `report.changed_slots` populated.

### Cold-start ↔ reactive synergy (why slot 8 matters)

The A2 reactive `Sync` path emits a masked write to slot 8 that is **skipped
when the slot is cold** (then triggers a `VerifySlots` resync). By warming slot
8 with the real reserves + `blockTimestampLast`, `cold_start` turns subsequent
`Sync` events into exact, zero-resync writes whose timestamp-preserve works.

## Scope / non-goals

In: `UniswapV2Adapter::cold_start`; `V2_TOKEN0_SLOT`/`V2_TOKEN1_SLOT` consts
(slots 6/7) in `storage.rs`; a small slot-decode helper; tests; this doc.

Out: rewiring `configured_amms`; deleting `cache_sync::v2_sync`; retiring the
compat shim; feature flags; V3; `LocalAMM`/simulation; non-canonical V2 clones;
per-cycle freshness tolerance (a legacy refresh-cycle concern, not cold-start).

## Affected files

`src/adapters/uniswap_v2.rs` (implement `cold_start`), `src/adapters/storage.rs`
(token slot consts + optional decode helper), `tests/adapter_reactive.rs`
(acceptance tests reuse the existing real-`EvmCache` + stub-fetcher harness),
`docs/phase-a3-tech-spec.md`. No `cache_sync` or upstream changes.

## Acceptance criteria

1. `Eager` cold-start against a stub fetcher seeded at slots 6/7/8 returns
   `Ready`, warms all three slots, sets `status = Ready` and
   `ProtocolMetadata::UniswapV2 { token0, token1, fee_bps }` (config `fee_bps`
   preserved).
2. After cold-start, a reactive `Sync` for the pool applies as
   `ExactFromInput` with no resync, and preserves the `blockTimestampLast`
   high bits (proving the cold-start↔reactive synergy).
3. `Lazy` returns `ReadyWithDeferred` with slots 6/7 deferred and slot 8 hot.
4. A reserves slot that cannot be fetched yields `NeedsRepair`, never a silent
   partial.
5. Full CI matrix green (fmt, clippy default + no-default-features, test default
   + no-default-features, `cargo doc -D warnings`).

## Test plan (manager-authored, red-green)

In `tests/adapter_reactive.rs` (reuses `setup_cache`, `stub_fetcher`,
`reserves_slot`, the reactive runtime helpers):

- `v2_cold_start_brings_pool_ready`
- `v2_cold_start_then_sync_applies_exact_no_resync` (synergy)
- `v2_cold_start_lazy_defers_token_slots`
- `v2_cold_start_missing_reserves_needs_repair`

All fail today (`cold_start` returns `Unsupported`) and pass after
implementation.

## Risks & assumptions

- Assumes canonical V2 storage (slots 6/7/8), consistent with the existing
  slot-8 masked write. Non-canonical clones out of scope.
- The compat shim stays in place (still used by the legacy path); this slice
  only stops *new* adapter code from depending on it. Full retirement is
  post-checkpoint.
- A genuinely zero-reserve pool would read as an absent slot 8 and surface as
  `NeedsRepair`; acceptable for a degenerate pool.
