# Phase A1 Tech Spec: Core Adapter Orchestration

Status: implemented.

## What A1 Added

Phase A1 turns the A0 adapter vocabulary into a usable hot-path adapter layer
without migrating the legacy simulation-coupled event and cache-sync modules.

The implemented adapter layer now includes:

- `AdapterRegistry` ownership of both tracked `PoolRegistration`s and protocol
  adapters keyed by `ProtocolId`.
- Duplicate adapter rejection through `RegistryError::DuplicateAdapter`.
- Generic registry routing for direct, indexed-address, and indexed-bytes32
  event sources before adapter-defined routing is attempted.
- Adapter-defined routing as a fallback path that avoids the trait default
  recursing back into the full registry route method.
- `SubscriptionSpec { sources }`, preserving complete `EventSource` records
  including emitter, topic filters, and route kind.
- `AdapterDriver`, which processes logs in caller-provided order:
  route log, select adapter, decode against the current cache `StateView`, apply
  emitted `StateUpdate`s, call `AmmAdapter::after_apply`, and return an
  `AdapterEventReport`.
- Re-exports for adapter cache update and skipped-update vocabulary, including
  `SlotDelta`, `SkippedDelta`, and `SkippedMask`.

Adapter mutations are expressed directly as `evm_fork_cache::StateUpdate`;
A1 does not introduce a second mutation-plan vocabulary.

## Reactive Runtime Integration

`AmmReactiveHandler` implements `evm_fork_cache::reactive::ReactiveHandler`,
bridging the registry into the reactive runtime:

- `interests()` builds `ReactiveInterest::Logs` from each pool's event sources,
  including adapter-derived sources not stored on the registration, with a
  `RouteKeySpec` matching the source's `EventRoute`.
- `handle()` routes the log to a pool, decodes via the adapter against the
  current `StateView`, then emits `ReactiveEffect`s: `StateUpdate`s for the
  decoded mutations, hash-pinned `Resync`es for predicted cold-slot skips,
  `Invalidate` for purge repairs, and `Hook` signals for semantic/repair events.
- Cold-slot prediction (`predict_cold_skips`) inspects the read-only state view
  to decide which masked/delta writes the runtime will skip, and requests
  `VerifySlots` resyncs for them so V2/V3 events self-heal a cold cache.

The synchronous `AdapterDriver` remains for callers that apply logs against an
`AdapterCache` directly without the runtime.

## Protocol Proof Adapters

### Uniswap V2

`UniswapV2Adapter` subscribes directly to the pool's
`Sync(uint112,uint112)` event.

The adapter decodes the event without using `amms`, `amm-math`, or simulation
pool types. It emits a masked write to `V2_RESERVES_SLOT` over the low 224 bits:

- `reserve0` occupies bits `[0, 112)`.
- `reserve1` occupies bits `[112, 224)`.
- `blockTimestampLast` occupies bits `[224, 256)` and is preserved when the slot
  is hot.

The event quality is `UpdateQuality::ExactIfApplied`. If the masked write is
skipped because the reserves slot is cold, `after_apply` requests verification
of the reserves slot.

### Uniswap V3

`UniswapV3Adapter` subscribes directly to `Swap`, `Mint`, and `Burn` topics.

`Swap` is implemented as an exact hot-slot update:

- masked `slot0` low-184-bit write for `sqrtPriceX96` and `tick`;
- absolute global liquidity slot write;
- layout read from `ProtocolMetadata::UniswapV3`, `PancakeV3`, or
  `Slipstream`.

`Mint` and `Burn` intentionally do not perform full tick maintenance in A1.
They return semantic liquidity events with `UpdateQuality::RequiresRepair` and a
targeted `RepairAction::V3TickRange`.

### Balancer V2

`BalancerV2Adapter` models the vault-emitted subscription shape for
`Swap(bytes32,address,address,uint256,uint256)`.

The event source is the vault from `ProtocolMetadata::BalancerV2.vault`, routed
by indexed `poolId` at topic index `1`. This proves routing does not depend on
`log.address == pool`.

## Non-Goals

A1 does not:

- migrate `src/events` or `src/cache_sync` to adapters;
- build cold-start implementations;
- implement full V3 tick, bitmap, fee-growth, or oracle maintenance;
- infer tracked pools from `EvmCache`;
- introduce RPC or provider ownership into adapters;
- add simulation model conversions;
- replace legacy event/cache-sync compatibility paths.

## A2/A3 Handoff

A2 should connect adapter reports and repair actions to a cache execution policy:

- execute `VerifySlots`, `PurgeStorage`, and `PurgeSlots` repairs;
- decide how skipped masked and delta updates are retried after repair;
- preserve existing observation/freshness behavior where it is still required.

A3 should migrate existing protocol behavior behind adapters incrementally:

- start with full Uniswap V2 cold-start and event handling;
- migrate V3 tick maintenance from the legacy event/cache-sync paths;
- add Pancake V3 and Slipstream wrappers around the V3 layout handling;
- migrate Balancer V2 once vault storage mapping and conservative repair policy
  are explicit.
