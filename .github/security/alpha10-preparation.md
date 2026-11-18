# Alpha.10 security binding preparation

Status: exact crate-only terms approved by Kai on 18 November 2026 at
15:39:41 UTC, through 19 November at 16:00 UTC. Approved preparation SHA-256:
`cf32f3bee9b6f71dcccdec1b0db92636b46aea083f8764644592a97e0279a658`.
The alpha.9 acceptance expired and is not reused.

Evidence checklist:

1. The trigger is qualification of the exact alpha.10 archive from source
   `5dca44f86f533065c0f4934ff29dbf2dcb74522e`; policy files are excluded from it.
   Qualification alone adds four exactly hashed excluded tests from the actual
   descendant `cfa549ba9773ce2d32dce2a17d2fb3244161b0df`.
2. No quote, planner, calldata or contract-permission path changes.
3. No runtime state dependency, provider call, hot-path latency or RPC change.
   The excluded V2 soak uses counted, bounded ancestry-header reference reads;
   only its final immutable snapshot quote is provider-disconnected. The existing
   campaign allowance is not renewed. Unfiltered audit/reachability checks remain.
4. No worker lifecycle, restart, reconciliation or production authority changes.
5. No economic or accounting amount changes.
6. Keep every current advisory and warning visible. Document exact artifact,
   time, qualification/promotion and publication boundaries in the decision.
7. Test absent approval, expiry, changed source/lock/archive/workflow, filtered
   audit, missing/new/duplicate findings and warnings, and wrong GitHub scope.
   Wrong/missing test hashes, extra test/non-test edits and lost test-source
   ancestry must refuse; publication must still reject the repaired-test source.
   The live-library and selected ETH/OP local/native consumer gates passed on
   18 November; the separate registry-only Linux worker gate remains pending.

The source/package and retained evidence audit lives at
`bifi-aero-held-startup-fees/outputs/amm-alpha10-release-audit-20261117`.
This policy worktree does not modify the clean publication source or archive.

## Prepared promotion path

The final preparation worktree is rooted at
`cfa549ba9773ce2d32dce2a17d2fb3244161b0df`, preserving the actual repaired-test
ancestry. Only these four prepared `.github/security/` files were copied into
it. The earlier preparation worktree and clean `5dca44f8` publication checkout
remain intact. The exact owner decision is recorded; normal signed promotion,
required CI and the separate exact-archive publication check remain mandatory.

The three long-window campaigns matched 32,264 quotes. The repaired V2 test ran
301.98 seconds, applied eight Sync events, used 25 counted ancestry reference
reads, and reproduced its pinned on-chain quote from the disconnected snapshot.
The optional latest-head differential was inconclusive because that quote was
unchanged. Six closing chain-ID/header reads verified all three campaign end
pins. Failed attempts and cumulative charges remain retained. Exact receipts
and limitations are indexed in
`bifi-aero-held-startup-fees/outputs/amm-alpha10-release-audit-20261117/live-gates-completed-20261118.json`.

The selected ETH/OP consumer passed both local sweeps (1,696 library and 575
worker tests), all eight recovery cases and 2,088.501453 seconds of overlapping
native delivery. Pair-six result SHA-256 is
`e2718fb141d48bcd24b13d4e4b604be259a13f279d1e937c89cbd39575a7aa27`.
The ordinary-state RPC count after cold start is zero. No full-worker Linux
acceptance is claimed before registry publication.

After the exact owner decision, normal
signed PR #46 promotion and required checks remain. A normal merge commit is
accepted only when it preserves actual repaired-test ancestry and the exact
path/hash restrictions. Publication still uses only the clean source and
original archive.
