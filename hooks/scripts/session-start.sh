#!/usr/bin/env bash
# SessionStart hook: open the session's row, then inject a brief scoped to the
# project and branch: persona, reminders due, the project's state, what is
# open (blockers, decisions, action items) and recent activity. Without it
# the model would have to decide to search first.
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"
hooks_disabled && { echo '{"continue": true}'; exit 0; }
hook_init
have_bin || { missing_bin_message; exit 0; }

if [ -n "$SESSION_ID" ]; then
  "$BIN" session start --session-id "$SESSION_ID" --cwd "$CWD" >/dev/null 2>&1 || true
fi
"$BIN" context --cwd "$CWD" --budget 2000 2>/dev/null | emit_context SessionStart
exit 0
