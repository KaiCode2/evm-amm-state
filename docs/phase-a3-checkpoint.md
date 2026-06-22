# Phase A3 Review Checkpoint

Status: completed. Recorded after V2 + V3 adapter `cold_start` landed (PRs #4, #5).

The ROADMAP gates the remaining protocol migrations on validating the adapter
module shape after V2/V3. This is the outcome of that review.

## Verdict

The adapter shape is fundamentally sound: the `AmmAdapter` trait and
`storage.rs` are good templates, and `adapters/*` is already
simulation-independent. Two cheap dedup fixes and one strategic consolidation
are worth making before scaling to the remaining 7 protocols.

## Fix before scaling (→ module-shape hardening slice)

1. **Consolidate the V3 family.** Pancake V3 and Slipstream differ from Uniswap
   V3 only in storage-slot offsets (`V3StorageLayout::pancake/slipstream`), and
   the V3 adapter already resolves the layout per-pool via `layout_for`. The
   only blocker is the registry's 1:1 `ProtocolId→adapter` mapping. Add
   `AmmAdapter::protocols() -> Vec<ProtocolId>` (default `[self.protocol()]`),
   let the registry register one adapter under each, and have the V3 adapter
   claim `[UniswapV3, PancakeV3, Slipstream]`. Rename `UniswapV3Adapter` →
   `V3FamilyAdapter` with a deprecated `UniswapV3Adapter` alias for back-compat.
2. **Dedupe `combine_repair`.** Identical copies in `driver.rs` and
   `reactive.rs`; move to `impl RepairAction`.
3. **Dedupe `route_log` matching.** `reactive.rs`'s fallback re-implements
   `registry.rs`'s per-source match predicate; extract one shared helper.

Also in that slice: tests for the `Lazy`/`HotSlotsOnly` cold-start policies
(currently implemented but untested), and a trait doc documenting the metadata
merge-vs-preserve contract (V2 merges decoded token slots; V3 preserves
config-supplied metadata).

## Feature model — DECISION

Adopt the proposed model; keep a **back-compat default** (current behavior
preserved). Implementation (the cfg-gating) is deferred to its own slice /
Phase S0.

```toml
[features]
default = ["adapters", "simulation", "search", "common-protocols", "toml"]

adapters   = []                                      # no heavy deps; always available
simulation = ["adapters", "dep:amm-math", "dep:amms"]
search     = ["simulation", "dep:rayon"]
discovery  = ["adapters"]
toml       = ["dep:toml"]

# per-protocol (each → adapters); gate adapter + simulation together
uniswap-v2 = ["adapters"]
uniswap-v3 = ["adapters"]
pancake-v3 = ["adapters"]
slipstream = ["adapters"]
solidly-v2 = ["adapters"]
balancer-v2 = ["adapters"]
balancer-v3 = ["adapters"]
curve      = ["adapters"]
erc4626    = ["adapters"]
uniswap-v4 = ["adapters"]

common-protocols = ["uniswap-v2", "uniswap-v3", "balancer-v2", "curve"]
```

Rationale: `adapters/*` is already heavy-dep-free; the blocker for an
adapter-only build is `lib.rs` unconditionally declaring `amm_wrapper`,
`cache_sync`, `configured_amms`, `events`, `discovery`, `routing`, the `*_pool`
modules, and the `amm-math` re-exports. The split = cfg-gating those behind
`simulation`/`search`. Keeping `simulation`+`search` in `default` means nothing
breaks now; the adapter-only build is `--no-default-features --features
adapters,<protocols>`. A leaner default can be revisited at S8.

Resolved ROADMAP open questions:
- Default: ergonomic + back-compat (not minimal) — for now.
- Per-protocol features gate adapter + simulation together (one flag each), not
  separate `*-adapter`/`*-simulation` axes.

## Tracked debt (follow-ups, not blocking)

- `compat.rs` process-global statics: cross-`EvmCache` pollution risk; audit all
  call sites before retiring the shim.
- **`DeferredWork` is produced by `cold_start` but nothing executes it** — wire
  an execution harness (or fold into `RepairAction`) when `configured_amms` is
  rewired.
- `NeedsRepair` / `ReadyWithDeferred` caller contract is undocumented.
- Balancer `ConservativeInvalidation` is a no-op; revisit in the Balancer slice.
- Legacy `events::EventRouter` vs the reactive handler are parallel paths that
  can diverge; decide a source-of-truth / deprecation before running both.
- Reserved-but-unused vocabulary (`RepairAction::V3Incremental/V3Full`,
  `UpdateQuality::Ignored`, some `DeferredWork` variants) — document as reserved
  or gate.

## Sequencing after this checkpoint

1. Module-shape hardening (next).
2. Feature-model split (cfg-gating; = Phase S0 pulled forward) — its own slice.
3. Protocol breadth: Pancake V3 + Slipstream (ride the V3-family consolidation),
   then Solidly V2, Balancer V2/V3, Curve, ERC4626, V4.
