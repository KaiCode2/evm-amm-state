# Curve adapter

The `CurveAdapter` ([`src/adapters/curve.rs`](../src/adapters/curve.rs), feature
`curve`) brings Curve plain pools into the adapters pipeline (cold-start →
react → simulate). It supports four dialects through one adapter:

- **StableSwap** (classic, e.g. 3pool)
- **StableSwap-NG** (newer stable factory pools, e.g. crvUSD/USDT)
- **CryptoSwap v2** (e.g. tricrypto2)
- **Tricrypto-NG** (e.g. tricryptoUSDC/USDT)

Like every adapter in this crate, swaps are quoted by running the pool's **own
`get_dy`** in a local revm against the warmed cache — there is **no
reimplemented Curve math**.

> Implementation history (per-slice specs, repository-only — not shipped in the
> crate package):
> [stableswap](https://github.com/KaiCode2/evm-amm-state/blob/main/docs/curve-stableswap-adapter-spec.md),
> [cryptoswap](https://github.com/KaiCode2/evm-amm-state/blob/main/docs/curve-slice2-cryptoswap-spec.md),
> [liquidity events](https://github.com/KaiCode2/evm-amm-state/blob/main/docs/curve-slice3-liquidity-events-spec.md),
> [tricrypto-ng](https://github.com/KaiCode2/evm-amm-state/blob/main/docs/curve-slice4-tricrypto-ng-spec.md).
> This document is the consolidated reference.

## Configuration — `CurveMetadata`

```rust
use evm_amm_state::adapters::{CurveMetadata, CurveVariant, PoolKey, PoolRegistration, ProtocolMetadata};

// Minimal: coins (index order) + dialect. Cold-start discovers the read-set.
let reg = PoolRegistration::new(PoolKey::Curve(pool_address))
    .with_state_address(pool_address)
    .with_metadata(ProtocolMetadata::Curve(
        CurveMetadata::default()
            // coins[i] is the get_dy index `i`; config-supplied static pool
            // identity, drives the simulate_swap token -> index mapping.
            .with_coins(vec![dai, usdc, usdt])
            // The dialect — selects the get_dy ABI and the event set.
            .with_variant(CurveVariant::StableSwap),
    ));

// Fast reboot: pre-fill the read-set (from a prior discovery / trace / registry)
// to skip discovery, and optionally seed the pool runtime. See "Cold-start".
//   CurveMetadata::default()
//       .with_coins(vec![dai, usdc, usdt])
//       .with_discovered_slots(known_slots) // verify-only cold_start + cold_start_many
//       .with_code_seed(pool_runtime)       // no lazy code fetch at first quote
```

(`CurveMetadata` is `#[non_exhaustive]`; construct it via `default()` + the
`with_*` builders rather than a struct literal.)

`CurveVariant` (defaults to `StableSwap`, so classic + NG pools need no flag):

| Variant | Use for | `get_dy` indices | `TokenExchange` | Liquidity events |
| --- | --- | --- | --- | --- |
| `StableSwap` | classic StableSwap **and** StableSwap-NG | `int128` | `int128` ids | fixed `uint256[N]` arrays; 2-arg (classic) **and** 3-arg (NG) `RemoveLiquidityOne` |
| `CryptoSwap` | Curve v2 (tricrypto2-style) | `uint256` | `uint256` ids (5-arg) | single-fee `AddLiquidity`, fees-array-less `RemoveLiquidity`, 3-arg `RemoveLiquidityOne` (no `RemoveLiquidityImbalance`) |
| `CryptoSwapNG` | Tricrypto-NG | `uint256` | extended 7-arg (+`fee`,`packed_price_scale`) | extended 5-arg `AddLiquidity`, 6-arg `RemoveLiquidityOne`, plus `ClaimAdminFee` |

Two shared axes keep this to one adapter: StableSwap/NG share the `int128`
quote path (differing only by the 3-arg `RemoveLiquidityOne`, which both route);
CryptoSwap/Tricrypto-NG share the `uint256` quote path (differing only in
events).

## Cold-start — discover → verify, or verify-only

A real Curve pool has **no predictable balance-slot layout** (a probe confirmed
`balances[]` is not at a fixed slot — it varies by Vyper build), so the planner
does not hand-code slots. It runs in one of two modes.

**Discover → verify** — the read-set is unknown (`discovered_slots` empty),
mirroring `BalancerV2ColdStartPlanner`:

1. **Discover** — run `get_dy(0, 1, DISCOVER_DX)` against the pool with
   `restrict_to=[pool]`, capturing the exact storage slots it SLOADs (balances +
   amplification + fee, wherever they live). The discover call uses the variant's
   `get_dy` ABI (a CryptoSwap pool reverts the `int128` form).
2. **Verify** — authoritatively warm those captured slots.
3. **finish** — persist `coins` + `discovered_slots` + `variant` (+ any
   `code_seed`), status `Ready`.

Repairs mirror Balancer: a reverting/empty discover → re-run cold-start; an
archive-miss on a discovered slot → `VerifySlots`; a per-slot `SlotFetch`
distinguishes a genuine zero from a fetch failure.

The local discover faults each `get_dy` SLOAD serially over RPC — the dominant
cost of a first boot. `AdapterRegistry::cold_start_primed` (and `cold_start_many`)
accelerate it: they derive the read-set with **one `eth_createAccessList`**,
bulk-load it, then run the discover **warm**. This needs no prior read-set; a
provider without `eth_createAccessList` transparently falls back to the plain
local discovery above. See [`docs/benchmarks.md`](benchmarks.md#curve-cold-start-discovery-vs-a-known-read-set).

**Verify-only** — the read-set is already known (`discovered_slots` pre-populated
from a prior discovery, a block trace, or a registry). The planner **skips
discovery entirely** — no pool-account/bytecode fetch and no cold-cache `get_dy`
faulting — and warms exactly the known slots in a **single verify round**. This
is what makes a known-read-set `cold_start` as cheap as the bundled
`cold_start_many` storage-program path (the same one-shot hydration Uniswap V2/V3
use), and it makes the pool eligible for `cold_start_many` /
`supports_one_shot_hydration`. A stale/incomplete set is safe: verify refreshes
what it has and the first `simulate_swap` lazily faults anything missing. See
[`examples/curve_cold_start_phases.rs`](../examples/curve_cold_start_phases.rs)
for a live discovery-vs-verify-only-vs-`cold_start_many` breakdown.

### Bytecode seeding (optional)

Curve pools are per-pool Vyper builds with **no shared or renderable template**
(unlike Uniswap V2's shared pair runtime or V3's rendered template), so the crate
embeds no Curve seed. A caller that already knows a pool's runtime can attach it
via [`CurveMetadata::with_code_seed`]: cold-start (and `cold_start_many`) verify
it once against the on-chain `EXTCODEHASH` — a mismatch is purged and the pool
falls back to lazily fetching the real code, so a wrong seed is a latency
question, never a correctness one. Seeding removes the one lazy code fetch a Curve
pool otherwise pays on its first `simulate_swap`, matching the fully-offline
V2/V3 profile after bootstrap.

[`CurveMetadata::with_code_seed`]: https://docs.rs/evm-amm-state/latest/evm_amm_state/adapters/struct.CurveMetadata.html

## Reactive — resync (not event-sourcing)

**Curve state cannot be kept current purely from events** (unlike Uniswap V2,
whose `Sync` emits the *absolute* new reserves). Curve events carry deltas, and
the stored `balances[]` are admin-fee-adjusted with Vyper integer rounding the
events do not encode. Measured on a live 3pool swap: the input balance moved by
*exactly* `tokens_sold`, but the output balance moved by `tokens_bought` **plus
the admin-fee skim** (~`54_819` wei) that `TokenExchange` does not carry.
Reconstructing it would mean reimplementing Curve's fee/rounding math — exactly
the line this crate holds.

So the reactive path **resyncs**: on a routed `TokenExchange` / liquidity event,
`decode_event` emits a `VerifySlots` repair over the pool's `discovered_slots`,
which the runtime lowers into a hash-pinned re-read of the post-event state. This
re-reads authoritative state (exact), at the cost of one bounded storage refetch
per event over the *known* slots. Missing/empty config → `RepairAction::None`,
never an error (a decode error would fail the whole `ingest_batch`).

`event_sources(pool)` returns the variant- and arity-correct topic set to
subscribe to; the array-event hashes are derived from `coins.len()` (the
`uint256[N]` arity is part of an event's signature, so the hash is pool-specific).

## Swap simulation

`simulate_swap(pool, cache, token_in, token_out, amount_in, &config)` maps
`token_in`/`token_out` to coin indices via `coins`, then calls the pool's own
`get_dy` (`int128` indices for StableSwap/NG, `uint256` for CryptoSwap/NG)
against the warmed cache and decodes the `uint256` output. Clean errors (never a
wrong-index quote) for: missing pool/coins, a token not in the pool, or a
self-swap (`token_in == token_out`).

## Verified event signatures

Every signature below was verified on-chain (eth_getLogs topic0 histogram) before
being routed — the adapter never guesses a signature.

| Dialect | `TokenExchange` | `AddLiquidity` | `RemoveLiquidity` | `RemoveLiquidityOne` |
| --- | --- | --- | --- | --- |
| StableSwap (classic) | `…,int128,uint256,int128,uint256` | `…,uint256[N],uint256[N],uint256,uint256` | `…,uint256[N],uint256[N],uint256` | `…,uint256,uint256` (2-arg) |
| StableSwap-NG | same int128 | same fixed `uint256[N]` | same | `…,uint256,uint256,uint256` (3-arg) |
| CryptoSwap v2 | `…,uint256,uint256,uint256,uint256` | `…,uint256[N],uint256,uint256` (single fee) | `…,uint256[N],uint256` | `…,uint256,uint256,uint256` (3-arg) |
| Tricrypto-NG | `…,uint256×4,fee,packed_price_scale` (7-arg) | `…,uint256[N],uint256,uint256,uint256` (5-arg) | `…,uint256[N],uint256` | `…,uint256×4,packed_price_scale` (6-arg) + `ClaimAdminFee` |

StableSwap also routes `RemoveLiquidityImbalance` (fixed `uint256[N]`).

## Limitations (out of scope)

- **Metapools / lending pools** — their `get_dy` makes external calls (to the
  base pool / underlying), so the `restrict_to=[pool]` discover capture is
  incomplete. A genuinely different cold-start problem.
- **Truly dynamic-`uint256[]` NG events** — none observed (the NG pools tested
  emit fixed `uint256[N]`); if found, verify the signature then route.

## How it's verified

- **Unit** (`src/adapters/curve.rs`): the topic hashes derived from `coins.len()`
  equal the `sol!`-macro `SIGNATURE_HASH`es (proving the format strings are
  byte-correct), and the variant topic sets are correct + disjoint on swaps.
- **Offline reactive** (`tests/adapter_reactive.rs`): each variant's swap +
  liquidity events route → resync the discovered slot.
- **Cold-start** (`tests/cold_start_adoption.rs`): discover → verify → Ready,
  plus archive-miss / discover-revert repairs.
- **RPC parity** (`tests/adapter_swap_sim_rpc.rs`, `#[ignore]`): fork mainnet at a
  pinned block, cold-start a real pool (3pool, tricrypto2, tricryptoUSDC), and
  assert `simulate_swap` == `eth_call get_dy` at that block.
- **Live WS soak** (`tests/reactive_curve_ws_e2e.rs`, `#[ignore]`): subscribe to
  Curve events over `wss://`, confirm the derived liquidity signatures flow live,
  route registered-pool events, and verify `simulate_swap` stays accurate at head
  for all three variants.
