# Canonical state source boundary

This local candidate starts from published alpha.9 source commit
`7352514b79d5741d887ff4a11bae6fa59408aabf`, with cache alpha.1 unchanged.
`adapters::canonical_state` owns a provider-incapable fixed-topology source
lineage and emits immutable `AmmStateSnapshot` values for ordinary consumers.
The existing log runtime and its event-completeness contract are unchanged.

Evidence checklist: full consensus header, exact chain/parent/number/context;
fixed account/code/static-slot completeness plus source-attested bounded dynamic
keys; zero provider capability; atomic validation before commit; bounded undo
and monotonically increasing generations; no quote/economics/permission change;
source durability, cursor and readiness remain with the embedding application.
Tests must cover coherent advancement, code/coverage/header/identity rejection,
rollback and alternate branch, finality/deep undo, version growth and immutable
older publications. The alpha.10 preparation versions this API without changing any dependency.
Publication, deployment and activation remain held.

This API is a source trust boundary, like complete canonical-log ingestion. It
does not cryptographically prove arbitrary state values against the state root.
The authenticated source owner validates all ordered old/new effects, accounts
and per-height completeness before supplying a delivery. Missing storage is
never zero. Scope/topology/code changes require a new explicitly reviewed
baseline; an application must revoke readiness before transition and wait for
durability before publishing to execution consumers.

`CanonicalStateOwner` shares the existing snapshot vocabulary, immutable registry
and pool generation revisions. It adds no provider, background repair, event
fabrication or log-completeness claim. Rewind increments versions and all pool
revisions, preventing an old quote memo from authorizing a different branch.

Resident code-hash inventory must exactly match the baseline, and each account's
actual code bytes must hash to its reviewed identity. A regression supplies an
irrelevant code body in the backing cache, shadowed by a valid account overlay;
the otherwise coherent state is rejected before history can retain it.

`bootstrap_from_delivery` transfers a complete cold execution scope alongside
the baseline topology. It requires the same exact point and identical overlapping
accounts, storage and ancestor hashes. Added cells still require application-owned
actual cold read evidence; the API does not turn matching pins into read proofs.
The original bootstrap delegates to this path using its existing cache.

Local validation on macOS/Rust 1.90: six focused canonical-state tests, 249
library tests with `live-runtime`, and strict all-target Clippy passed. There is
no hosted acceptance, new published version, or release authority. The bot
must qualify against this exact local source before a reviewed upstream pin
can replace alpha.9. `evm-fork-cache = 0.4.2-alpha.1` is unchanged.

The alpha.10 packaging branch retains the API at
`f2d1251402aa1177aa5a9b6b2114a716214cbda5` byte-for-byte. Only package/root-lock
version and release documentation change. Registry inspection on 9 November
confirmed published alpha.9 checksum
`6b479657c772c2e7d61144945c5a34c328156b5b018a52bc272ce32016758303`;
alpha.10 was absent. Packaging and complete validation are still in progress.


Preparation checklist for alpha.10: the source API tree remains byte-identical
at the binding above; all applicable offline matrix/package checks must run
against the versioned candidate, and the downstream bot must use a separately
recorded immutable source override until publication is authorized. Full
unfiltered audit is captured outside the repository's advisory-ignore config;
ordinary policy evaluation remains separate and release-blocking. No runtime
I/O, source scope, economic behavior or permission changes are introduced by
the packaging commit.
