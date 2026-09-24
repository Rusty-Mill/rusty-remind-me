#!/usr/bin/env bash
# Fail if `.claude-plugin/plugin.json`'s "version" has drifted from the
# crates' version (see get_workspace_version.sh, which also fails if the six
# crates disagree among themselves).
#
# The two are independent, hand-maintained fields — nothing in Cargo itself
# ties a plugin manifest's version to a crate version — so a
# PR that bumps one and forgets the other would otherwise ship a release
# whose attached plugin archive still claims the previous version. This is
# the schema-drift check's shape applied to that gap: catch it in CI, in
# seconds, rather than after a mismatched release is already published.
#
# Exit codes: 0 versions agree, 1 they differ, 2 the check could not run.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
plugin_json="$repo_root/.claude-plugin/plugin.json"

if [ ! -f "$plugin_json" ]; then
  echo "error: $plugin_json does not exist" >&2
  exit 2
fi

workspace_version="$("$repo_root/scripts/get_workspace_version.sh")"

plugin_version="$(jq -r '.version' "$plugin_json")"
if [ -z "$plugin_version" ] || [ "$plugin_version" = "null" ]; then
  echo "error: $plugin_json has no string \"version\" field" >&2
  exit 2
fi

if [ "$workspace_version" != "$plugin_version" ]; then
  echo "Plugin version drift: the crates' Cargo.toml version is $workspace_version," \
    "but .claude-plugin/plugin.json is $plugin_version." >&2
  echo "Bump .claude-plugin/plugin.json's \"version\" to match in this PR." >&2
  exit 1
fi

echo "plugin.json version ($plugin_version) matches the workspace version."
