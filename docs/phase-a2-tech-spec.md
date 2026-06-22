# Phase A2 Tech Spec: Cache Repair Execution

Status: design approved; implementation pending.

## Goal

A2 makes the adapter repair side concrete and complete for Uniswap V2/V3 on top
of the `evm-fork-cache` reactive runtime. A1 made adapters emit semantic events
plus repair *intentions*; for V3 liquidity events those intentions are currently
lowered to observability `Hook` signals only and never executed. A2 closes that
gap by lowering state-affecting repairs into executable `ReactiveEffect`s.

## Runtime model (confirmed against pinned `evm-fork-cache`)

- `ReactiveEffect::StateUpdate` is applied via `EvmCache::apply_updates` in both
  `ingest_batch` and `ingest_batch_with_resync`.
- `ReactiveEffect::Invalidate` is lowered to `StateUpdate::Purge` and applied in
  both entrypoints.
- `ReactiveEffect::Resync` is collected and executed only by
  `ingest_batch_with_resync`, which reads `ResyncTarget::StorageSlots` through
  the hash-pinned `storage_batch_fetcher` and writes the results back as exact
  `StateUpdate::slot`s, reporting them in `ResyncReport`.
- `ResyncTarget::StorageSlots { address, slots }` accepts an arbitrary slot set,
  which is sufficient to express targeted V3 tick repair.

## Approved decisions (A2 review checkpoint)

1. **Observation/freshness tracking stays in `evm-fork-cache`.** The reactive
   path is event-driven and hash-pinned and needs no `SlotObservationTracker`.
   A2 introduces no observation tracking into the adapter layer. Legacy
   `cache_sync` keeps using the tracker untouched. A protocol-hint API to a
   generic freshness service is deferred to a later phase.
2. **V3 liquidity-event repair is a targeted hash-pinned resync**, not manual
   slot injection. Events do not carry prior gross/net or fee-growth, so
   authoritative re-read is both simpler and exact, and is reorg-safe.
3. **Balancer V2 stays the A1 routing proof.** Real vault balance-slot repair
   needs the storage mapping, scheduled for A3. A2 does not add a Balancer
   fallback purge (the only available scope is too broad).

## Scope

- A repair-lowering policy converting `RepairAction` into executable
  `ReactiveEffect`s, replacing the hook-only placeholders for state-affecting
  repairs while preserving the `Hook` signal for observability.
- V3 `Mint`/`Burn` over `[tickLower, tickUpper]` lowers to a `ResyncRequest`
  targeting exactly:
  - the boundary `Tick.Info` slots for `tickLower` and `tickUpper`
    (slots `{0, 3}`, matching `cache_sync::decode_v3_tick_info_raw`),
  - the (deduped, <=2) `tickBitmap` words containing those ticks,
  - the global `liquidity` slot,
  at `ResyncBlock::Hash { require_canonical: true }` of the event block.
- Formalize the skipped-write retry path (`predict_cold_skips` -> `VerifySlots`
  -> resync) as the single repair-retry mechanism and document the
  `StateEffectQuality` transitions, including `ResyncedAuthoritatively`.

## Non-goals

- Balancer real repair (A3).
- Cold-start *execution* of `RepairAction::ColdStart` (A4); A2 only routes it.
- Migrating `cache_sync` / `SlotObservationTracker`.
- Any `amm-math` / simulation coupling.

## Design

- New module `src/adapters/repair.rs` owning
  `repair_to_effects(pool, event, repair, ctx) -> Vec<ReactiveEffect>`.
  `reactive.rs` delegates to it; the function receives the `PoolRegistration`
  so it can resolve the `V3StorageLayout`.
- V3 tick-slot computation helper: given a `V3StorageLayout` and
  `(tick_lower, tick_upper)`, produce the deduped slot set using
  `v3_tick_info_storage_keys_with_base`, `v3_tick_bitmap_storage_key_with_base`,
  `tick_to_word`, and `layout.liquidity_slot`.
- Expose `layout_for(pool)` (currently private in `uniswap_v3.rs`) or relocate
  it to `storage.rs` for reuse.
- `V3Incremental` / `V3Full` / `ColdStart` retain their current `Hook`-only
  lowering in A2: no current adapter emits them (V3 decode emits only
  `V3TickRange`), so executing them now would be untested dead code. Their
  execution is deferred (cold-start execution is A4). A2 implements `V3TickRange`
  concretely and the missing-layout fallback.
- Quality: a V3 liquidity event emitting a resync reports `RequiresRepair`;
  after `ingest_batch_with_resync`, the resynced slots report
  `ResyncedAuthoritatively` in `ResyncReport`.

## Affected files

`src/adapters/repair.rs` (new), `src/adapters/reactive.rs`,
`src/adapters/uniswap_v3.rs` (expose layout), `src/adapters/storage.rs`
(tick-slot set helper), `src/adapters/mod.rs` (exports). No `cache_sync` or
upstream changes.

## Error handling and edge cases

- Missing `V3StorageLayout`: fall back to
  `Invalidate(PurgeScope::AllStorage)` with `RequiresRepair` quality plus a
  degraded-repair hook. `AllStorage` is used because without a layout the
  protocol-specific slot0/liquidity slots cannot be named safely (they differ
  across Uniswap/Pancake/Slipstream). Never silently dropped.
- `tickLower` / `tickUpper` in the same bitmap word: dedup to one bitmap slot.
- Mint/Burn out of current range (liquidity unchanged): tick-info and bitmap
  slots still resynced; liquidity-slot resync is harmless.
- Resyncs execute only under `ingest_batch_with_resync`; `ingest_batch` callers
  receive the hook and pending-resync quality but no execution.

## Acceptance criteria

1. A V3 `Mint`/`Burn` via `ingest_batch_with_resync` emits a `ResyncRequest`
   whose targets are exactly the boundary tick-info slots, the containing bitmap
   word(s), and the liquidity slot, at the event block hash with
   `require_canonical: true`.
2. After execution against a stubbed `storage_batch_fetcher`, the cache holds
   the fetched values and the applied report is `ResyncedAuthoritatively`.
3. A missing-layout V3 liquidity event yields a conservative invalidation, not a
   panic or silent miss.
4. All existing A1 reactive tests still pass unchanged.
5. `cargo test`, `cargo clippy --all-targets --no-deps -- -D warnings`,
   `cargo fmt --check`, and `cargo check --no-default-features --all-targets`
   all pass.

## Test plan

Manager-authored, red against current `main` (hook-only):

- `v3_mint_emits_targeted_tick_resync` — exact resync slot set + hash-pinned
  block.
- `v3_mint_resync_repairs_tick_and_liquidity_slots` — stub fetcher; assert cache
  values and `ResyncedAuthoritatively`.
- `v3_burn_same_word_dedupes_bitmap_slot` — boundary ticks in one word.
- `v3_liquidity_event_missing_layout_falls_back_to_invalidation` — degraded
  path.
- Unit tests in `repair.rs` for the slot-set computation against a golden
  `V3StorageLayout`.
