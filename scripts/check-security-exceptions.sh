#!/usr/bin/env bash
set +x
set -euo pipefail

cd "$(dirname "$0")/.."

policy_error=""

is_patched_tracing_subscriber_version() {
  local version="$1"
  if [[ ! "$version" =~ ^([0-9]+)\.([0-9]+)\.([0-9]+)$ ]]; then
    return 1
  fi

  local major="${BASH_REMATCH[1]}"
  local minor="${BASH_REMATCH[2]}"
  local patch="${BASH_REMATCH[3]}"
  ((major > 0 || minor > 3 || (minor == 3 && patch >= 20)))
}

validate_tracing_subscriber_policy() {
  local locked_versions="$1"
  local active_versions="$2"
  local version
  local ignored_lock_entries=0
  local active_entries=0
  policy_error=""

  while IFS= read -r version; do
    [[ -n "$version" ]] || continue
    if [[ "$version" == "0.2.25" ]]; then
      ((ignored_lock_entries += 1))
    elif ! is_patched_tracing_subscriber_version "$version"; then
      policy_error="Cargo.lock contains another tracing-subscriber version covered by RUSTSEC-2025-0055"
      return 1
    fi
  done <<<"$locked_versions"

  if ((ignored_lock_entries != 1)); then
    policy_error="Cargo.lock must contain exactly one tracing-subscriber 0.2.25 entry until the advisory ignore is removed"
    return 1
  fi

  while IFS= read -r version; do
    [[ -n "$version" ]] || continue
    ((active_entries += 1))
    if ! is_patched_tracing_subscriber_version "$version"; then
      policy_error="the active graph contains tracing-subscriber below patched version 0.3.20"
      return 1
    fi
  done <<<"$active_versions"

  if ((active_entries == 0)); then
    policy_error="the active tracing-subscriber version set is unexpectedly empty"
    return 1
  fi
}

assert_policy_fixtures() {
  local valid_locked
  valid_locked="$(printf '%s\n' '0.2.25' '0.3.23')"

  validate_tracing_subscriber_policy "$valid_locked" "0.3.23"
  ! validate_tracing_subscriber_policy "0.3.23" "0.3.23"
  ! validate_tracing_subscriber_policy \
    "$(printf '%s\n' '0.2.25' '0.3.19' '0.3.23')" "0.3.23"
  ! validate_tracing_subscriber_policy "$valid_locked" "0.2.25"
  ! validate_tracing_subscriber_policy "$valid_locked" ""
}

assert_policy_fixtures

# RUSTSEC-2025-0055 is advisory-wide. The ignored version is recorded only by
# ark-relations through an inactive optional proof-system dependency. Prove the
# complete active graph is patched, and require the ignored entry to remain
# exact so removing it also requires removing the exception.
locked_versions="$(
  awk '
    /^\[\[package\]\]$/ {
      if (name == "tracing-subscriber") {
        print version
      }
      name = ""
      version = ""
      next
    }
    /^name = / {
      value = $0
      sub(/^name = "/, "", value)
      sub(/"$/, "", value)
      name = value
      next
    }
    /^version = / {
      value = $0
      sub(/^version = "/, "", value)
      sub(/"$/, "", value)
      version = value
      next
    }
    END {
      if (name == "tracing-subscriber") {
        print version
      }
    }
  ' Cargo.lock | sort
)"
active_tree="$(
  cargo tree --locked --all-features --target all --prefix none
)"
active_versions="$(
  printf '%s\n' "$active_tree" \
    | awk '$1 == "tracing-subscriber" { sub(/^v/, "", $2); print $2 }' \
    | sort -u
)"

if ! validate_tracing_subscriber_policy "$locked_versions" "$active_versions"; then
  echo "RUSTSEC-2025-0055 scope check failed: $policy_error." >&2
  echo "Locked tracing-subscriber versions:" >&2
  printf '%s\n' "$locked_versions" >&2
  echo "Active tracing-subscriber versions:" >&2
  printf '%s\n' "$active_versions" >&2
  exit 1
fi

inactive_graph="$({
  cargo tree --locked --all-features \
    -i tracing-subscriber@0.2.25 --target all --prefix none 2>/dev/null
} || true)"
if [[ -n "$inactive_graph" ]]; then
  echo "RUSTSEC-2025-0055 is no longer confined to an inactive lock entry." >&2
  echo "$inactive_graph" >&2
  exit 1
fi

echo "Security advisory exception remains confined to an inactive lock entry."
