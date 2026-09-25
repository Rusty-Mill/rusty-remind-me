#!/usr/bin/env bash
# Fail if the committed schema_*.sql files are not what
# `scripts/regenerate_schema.py` actually produces from the reference.
#
# `check_schema_drift.sh` catches the SCHEMA_VERSION *number* falling behind
# the reference's. It says nothing about the DDL: `schema_tables.sql`,
# `schema_indexes.sql` and `schema_triggers.sql` (crates/remind_me_core/src/db)
# are generated verbatim by regenerate_schema.py and their headers say
# "do not hand-edit" -- but nothing ran that generator and diffed its output
# against what's committed. A hand-edit to a generated file that does not
# match what the generator would actually produce -- wrong column, wrong
# constraint, a typo in a trigger body -- passed CI silently as long as the
# version number was untouched. (#277)
#
# Usage:
#   scripts/check_schema_regen_drift.sh                     # clones the reference
#   scripts/check_schema_regen_drift.sh /path/to/remind_me  # uses a local checkout
#
# Exit codes: 0 no drift, 1 the generated output differs, 2 the check could
# not run.
#
# ---------------------------------------------------------------------------
# Same discipline as check_schema_drift.sh: a check that cannot run must not
# report success. Every step below that can fail for a reason other than
# "the schema actually drifted" exits 2, not 1, so a broken reference clone
# or a regenerate_schema.py that errored out is never mistaken for parity.

set -euo pipefail

# See check_schema_drift.sh for why this must be unset before the first `cd`.
unset CDPATH

REFERENCE_REPO="https://github.com/baileyrd/remind_me.git"
REFERENCE_PACKAGE="remind_me_mcp"
SCHEMA_DIR="crates/remind_me_core/src/db"
SCHEMA_FILES=(schema_tables.sql schema_indexes.sql schema_triggers.sql)

die() { printf '\033[1;31mschema-regen-drift: %s\033[0m\n' "$*" >&2; exit 2; }
info() { printf '\033[1;32m==>\033[0m %s\n' "$*"; }

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# ---------------------------------------------------------------------------
# Locate the reference and a scratch output dir, cleaning up either way.
# ---------------------------------------------------------------------------

cleanup_dir=""
# `return 0` is load-bearing here too -- see check_schema_drift.sh's comment
# on the same line. An EXIT trap whose last command fails clobbers the exit
# status the script was already reporting.
# shellcheck disable=SC2317  # invoked by the EXIT trap below, not inline
cleanup() {
    if [[ -n "$cleanup_dir" ]]; then
        rm -rf "$cleanup_dir"
    fi
    return 0
}
trap cleanup EXIT

cleanup_dir="$(mktemp -d)"
scratch_out="$cleanup_dir/regenerated"
mkdir -p "$scratch_out"

if (( $# >= 1 )); then
    reference_root="$1"
    [[ -f "$reference_root/$REFERENCE_PACKAGE/db.py" ]] \
        || die "no $REFERENCE_PACKAGE/db.py under $reference_root"
else
    reference_root="$cleanup_dir/remind_me"
    # A blobless partial clone, same as check_schema_drift.sh: this needs the
    # reference's schema-relevant source, not its history. Unlike that
    # script's single-file checkout, regenerate_schema.py imports the whole
    # $REFERENCE_PACKAGE package (db.py pulls in ann_index, config,
    # embeddings), so the checkout below is a directory, not one file.
    git clone --quiet --filter=blob:none --no-checkout \
        "$REFERENCE_REPO" "$reference_root" \
        || die "could not clone $REFERENCE_REPO"
    git -C "$reference_root" checkout --quiet HEAD -- "$REFERENCE_PACKAGE" \
        || die "could not read $REFERENCE_PACKAGE from $REFERENCE_REPO"
fi

# ---------------------------------------------------------------------------
# Regenerate into the scratch dir -- never in place, so a failed or partial
# run can't leave the working tree's committed files half-overwritten.
# ---------------------------------------------------------------------------

info "regenerating schema_*.sql from $reference_root"
if ! python3 "$repo_root/scripts/regenerate_schema.py" \
    --reference "$reference_root" --out "$scratch_out"; then
    die "regenerate_schema.py failed -- see its output above"
fi

# ---------------------------------------------------------------------------
# Diff each generated file against what's committed.
# ---------------------------------------------------------------------------

drifted=()
for f in "${SCHEMA_FILES[@]}"; do
    committed="$repo_root/$SCHEMA_DIR/$f"
    generated="$scratch_out/$f"
    [[ -f "$committed" ]] || die "$f: not found at $SCHEMA_DIR (was it moved?)"
    [[ -f "$generated" ]] || die "$f: regenerate_schema.py did not write it"

    if ! diff -u "$committed" "$generated" >&2; then
        drifted+=("$f")
    fi
done

if [[ "${#drifted[@]}" -eq 0 ]]; then
    info "no drift: committed schema_*.sql match what regenerate_schema.py produces"
    exit 0
fi

cat >&2 <<EOF

$(printf '\033[1;31mSCHEMA REGENERATION DRIFT\033[0m')

  hand-edited, out of sync with the generator: ${drifted[*]}

$SCHEMA_DIR/schema_*.sql are generated verbatim by regenerate_schema.py and
must not be hand-edited (see migrations.rs's module docs). The diff above is
what regenerating now would change.

To close it:

  1. python3 scripts/regenerate_schema.py --reference /path/to/remind_me
  2. review the diff -- if it's not what you expected, the reference has
     moved and check_schema_drift.sh should also be failing
  3. commit the regenerated files

EOF
exit 1
