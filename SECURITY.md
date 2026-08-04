# Security Policy

`evm-amm-state` is a pre-1.0 EVM simulation component. Only the latest
published line receives security fixes. Report vulnerabilities privately
through this repository's GitHub Security tab; do not disclose sensitive
details in a public issue.

Include the crate version, Rust version, enabled features, impact, and a minimal
reproduction where possible.

## Dependency and CI policy

CI audits the checked-in lockfile with `cargo-audit 0.22.2` after
`scripts/check-security-exceptions.sh` proves every documented exception remains
inside its reviewed reachability scope. It never regenerates the lockfile before
the audit.

Every third-party workflow action is pinned to an immutable full commit:

| Action | Reviewed ref | Pinned commit |
| --- | --- | --- |
| [`actions/checkout`](https://github.com/actions/checkout/releases/tag/v4.4.0) | `v4.4.0` | `11d5960a326750d5838078e36cf38b85af677262` |
| [`dtolnay/rust-toolchain`](https://github.com/dtolnay/rust-toolchain/commit/4cda84d5c5c54efe2404f9d843567869ab1699d4) | `stable` | `4cda84d5c5c54efe2404f9d843567869ab1699d4` |
| [`Swatinem/rust-cache`](https://github.com/Swatinem/rust-cache/releases/tag/v2.9.1) | `v2.9.1` | `c19371144df3bb44fab255c43d04cbc2ab54d1c4` |

Sibling repositories are checked out at exact candidate commits in every CI and
live workflow. The transport candidate is
`7868bea593dec5748ad7475d1909fc3a2de0d4ad`; the cache candidate is
`2be88d15fa15b5c44188b318463aa4705bb75aef`; and the live search gate uses
`566bde7949e828cd82c0d864576ee10f7044cc88`. Any revision change requires
rerunning the applicable locked release or paid-provider matrix.

The accepted advisory and unmaintained-dependency scopes are documented and
machine-checked by `scripts/check-security-exceptions.sh`. New vulnerability
advisories remain release-blocking.

The current graph has one ignored vulnerability advisory:

- `RUSTSEC-2025-0055` affects `tracing-subscriber` 0.2.25. That version is an
  unreachable lockfile entry. The scope check requires it to remain the only
  affected locked version while every reachable version is patched at 0.3.20
  or newer. Removing the inactive entry without removing the ignore also fails.

RustSec also reports four unmaintained crates. These are warnings rather than
vulnerability advisories, and each accepted reachability boundary is checked:

- `bincode` 1.3.3 is inherited only through `evm-fork-cache`, whose versioned
  on-disk cache formats require an explicit migration before it can be removed.
- `derivative` 2.2.0 is an unreachable lockfile entry.
- `paste` 1.0.15 is an active transitive procedural macro through the pinned
  Alloy and Arkworks graph; its immediate reverse dependencies are fixed by the
  scope check.
- `proc-macro-error2` 2.0.1 is an active transitive procedural-macro helper
  through the pinned Alloy Solidity macro graph; its immediate reverse
  dependencies are fixed by the scope check.

Any change to these paths requires renewed review. Remove an exception as soon
as its lock entry or upstream constraint disappears.
