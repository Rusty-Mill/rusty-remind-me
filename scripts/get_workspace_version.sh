#!/usr/bin/env bash
# Print rusty_remind_me's version to stdout, or exit non-zero with nothing
# printed if it can't be determined unambiguously.
#
# Used by the release workflow (`.github/workflows/remind-me-release.yml` at
# the rusty_mill root) to name the tag/release, and by check_plugin_version.sh
# to compare against `.claude-plugin/plugin.json`.
#
# Before the move into the rusty_mill monorepo this read one
# `[workspace.package].version` that all six crates inherited. The monorepo's
# `[workspace.package]` belongs to other crates (and says 0.1.0), so each of
# the six now carries a literal `version = "..."` -- and nothing in Cargo
# keeps them in lockstep any more. This script is what does: it reads every
# member's `[package]` version and fails unless they all agree, so a bump
# that misses one crate is caught here instead of shipping a release whose
# binaries report different versions.
#
# Usage: scripts/get_workspace_version.sh
set -euo pipefail

product_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
members=(remind_me_core remind_me_mcp remind_me_api remind_me_cli remind_me_remote remind_me_hub)

# The `version = "..."` line inside a manifest's own `[package]` table only --
# not a bare first-match grep, which a `[dependencies]` entry written as
# `version = "..."` on its own line could satisfy.
package_version() {
  awk '
    /^\[package\][ \t]*$/ { in_pkg = 1; next }
    /^\[/ { in_pkg = 0 }
    in_pkg && /^version[ \t]*=/ {
      line = $0
      sub(/^version[ \t]*=[ \t]*"/, "", line)
      sub(/".*$/, "", line)
      print line
      exit
    }
  ' "$1"
}

version=""
for member in "${members[@]}"; do
  manifest="$product_root/crates/$member/Cargo.toml"
  if [ ! -f "$manifest" ]; then
    echo "error: $manifest does not exist" >&2
    exit 2
  fi
  v="$(package_version "$manifest")"
  if [ -z "$v" ]; then
    echo "error: no literal version = \"...\" in [package] of $manifest" >&2
    exit 2
  fi
  if [ -z "$version" ]; then
    version="$v"
  elif [ "$v" != "$version" ]; then
    echo "error: crate versions disagree: $member is $v, but crates/${members[0]} is $version." >&2
    echo "Bump all six rusty_remind_me crates together." >&2
    exit 1
  fi
done

echo "$version"
