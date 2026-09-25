#!/usr/bin/env bash
# Fail if this port's SCHEMA_VERSION has fallen behind `remind_me`'s.
#
# ARCHITECTURE.md §1 Tenet 3 makes an identical SQLite schema the definition of
# parity, and `remind_me` reads `PRAGMA user_version` on open and skips
# migrating anything already at its own target. A port that claims a version
# whose migrations it has not performed is therefore not merely behind — it
# makes the reference *skip* the very steps that would fix it.
#
# This exists because that drift opened and went unnoticed for a day. The
# reference merged its issues #167 and #220, moving 27 -> 29, and nothing in
# either repo said a word: the port's own tests all passed, because they
# compared the port against itself. It was found by hand.
#
# Usage:
#   scripts/check_schema_drift.sh                     # clones the reference
#   scripts/check_schema_drift.sh /path/to/remind_me  # uses a local checkout
#
# Exit codes: 0 versions agree, 1 they differ, 2 the check could not run.
#
# ---------------------------------------------------------------------------
# The distinction between exit 1 and exit 2 is the whole design.
#
# A drift check that cannot find one of the constants must not compare two
# empty strings, find them equal, and report success. That failure mode is
# precisely what this file is defending against, so it is worth being blunt:
# every extraction below asserts it matched *exactly one* line and that the
# captured text is a number, and anything else exits 2 rather than comparing.
# A check that silently passes is worse than no check, because it also
# suppresses the suspicion that would have prompted a manual look.

set -euo pipefail

# A `cd` builtin echoes the resolved directory to stdout whenever CDPATH is
# set and the target isn't found relative to `.` first — invisible in a
# terminal, but fatal here: `repo_root="$(cd ... && pwd)"` below then
# captures two lines and every path built from it silently gains a leading
# `$CDPATH_HIT\n`, so `[[ -f "$file" ]]` reports a real file as missing. Unset
# it before the first `cd`, not after: by the time symptoms show up the
# variable has already done its damage.
unset CDPATH

REFERENCE_REPO="https://github.com/baileyrd/remind_me.git"
PORT_FILE="crates/remind_me_core/src/db/migrations.rs"
REFERENCE_FILE="remind_me_mcp/db.py"

# Anchored to the start of the line so a comment, a docstring mention or a
# comparison against the constant cannot be mistaken for its definition.
PORT_PATTERN='^pub const SCHEMA_VERSION: i32 = [0-9]+;$'
REFERENCE_PATTERN='^_SCHEMA_VERSION = [0-9]+$'

die() { printf '\033[1;31mschema-drift: %s\033[0m\n' "$*" >&2; exit 2; }
info() { printf '\033[1;32m==>\033[0m %s\n' "$*"; }

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# ---------------------------------------------------------------------------
# Locate the reference
# ---------------------------------------------------------------------------

cleanup_dir=""
# `return 0` is load-bearing, not tidiness. An EXIT trap whose last command
# fails overwrites the status the script was exiting with, so the obvious
# one-liner -- `[[ -n "$cleanup_dir" ]] && rm -rf "$cleanup_dir"` -- returns 1
# whenever there is nothing to clean, which is exactly the in-parity path.
# This check reported failure on success until that was fixed.
#
# shellcheck disable=SC2317  # invoked by the EXIT trap below, not inline
cleanup() {
    if [[ -n "$cleanup_dir" ]]; then
        rm -rf "$cleanup_dir"
    fi
    return 0
}
trap cleanup EXIT

if (( $# >= 1 )); then
    reference_root="$1"
    [[ -f "$reference_root/$REFERENCE_FILE" ]] \
        || die "no $REFERENCE_FILE under $reference_root"
else
    cleanup_dir="$(mktemp -d)"
    reference_root="$cleanup_dir/remind_me"
    # A blobless partial clone of one branch: this needs two integers, not
    # history. --filter keeps it fast without the surprises of --depth 1 in a
    # repo that may later want a tag.
    git clone --quiet --filter=blob:none --no-checkout \
        "$REFERENCE_REPO" "$reference_root" \
        || die "could not clone $REFERENCE_REPO"
    git -C "$reference_root" checkout --quiet HEAD -- "$REFERENCE_FILE" \
        || die "could not read $REFERENCE_FILE from $REFERENCE_REPO"
fi

# ---------------------------------------------------------------------------
# Extract, refusing to guess
# ---------------------------------------------------------------------------

# Print the single number defined by $2 in file $1, or exit 2 explaining why
# it could not. Deliberately not `grep -o ... | head -1`: taking the first of
# several matches is how a check starts reporting on the wrong line.
extract_version() {
    local file="$1" pattern="$2" label="$3"
    [[ -f "$file" ]] || die "$label: $file does not exist (was it moved?)"

    local matches count
    matches="$(grep -E "$pattern" "$file" || true)"
    count="$(printf '%s' "$matches" | grep -c . || true)"

    if [[ "$count" -eq 0 ]]; then
        die "$label: no line in $file matches /$pattern/.
    The constant was probably renamed or reformatted. This is a FAILURE, not a
    pass: the check cannot tell you whether the versions agree, and reporting
    success would hide exactly the drift it exists to catch."
    fi
    if [[ "$count" -gt 1 ]]; then
        die "$label: $count lines in $file match /$pattern/, expected 1:
$matches"
    fi

    # Take the number after the `=`, not the first number on the line: the
    # Rust declaration is `SCHEMA_VERSION: i32 = 29`, and a bare `[0-9]+`
    # happily returns the 32 from the type.
    local version
    version="$(printf '%s' "$matches" | sed -E 's/^.*=[[:space:]]*([0-9]+).*$/\1/')"
    [[ "$version" =~ ^[0-9]+$ ]] || die "$label: extracted '$version', not a number"
    printf '%s' "$version"
}

port_version="$(extract_version "$repo_root/$PORT_FILE" "$PORT_PATTERN" "port")"
reference_version="$(extract_version \
    "$reference_root/$REFERENCE_FILE" "$REFERENCE_PATTERN" "reference")"

# ---------------------------------------------------------------------------
# Compare
# ---------------------------------------------------------------------------

info "port      (rusty_remind_me): SCHEMA_VERSION  = $port_version"
info "reference (remind_me):       _SCHEMA_VERSION = $reference_version"

if [[ "$port_version" == "$reference_version" ]]; then
    info "in parity at v$port_version"
    exit 0
fi

cat >&2 <<EOF

$(printf '\033[1;31mSCHEMA VERSION DRIFT\033[0m')

  reference (remind_me):       $reference_version
  port      (rusty_remind_me): $port_version

ARCHITECTURE.md Tenet 3 makes an identical schema the definition of parity,
and the two implementations are expected to read the same database file.

This is not only "the port is behind". \`remind_me\` skips any migration whose
version is already stamped, so a database this port writes at $port_version will
never receive the reference's steps up to $reference_version, on any machine.

To close it:

  1. python3 scripts/regenerate_schema.py --reference /path/to/remind_me
  2. port whatever data migrations the new versions perform -- the schema
     regeneration only carries DDL, and a version can be data-only (v29 was)
  3. bump SCHEMA_VERSION in $PORT_FILE
  4. update the literal in the_schema_version_is_the_references_current_one
     (crates/remind_me_core/tests/schema_test.rs)

EOF
exit 1
