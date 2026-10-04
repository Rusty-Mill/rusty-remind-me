#!/usr/bin/env bash
# Stop / PreCompact / SessionEnd hook: save the conversation, so a session
# that ends without anyone calling a memory tool still leaves a record.
#
#   capture.sh stop|precompact|end
#
# One capture per session: each run replaces the session's dialog. Stop fires
# after every turn, so it is skipped inside a stop hook and throttled to once
# a minute; PreCompact (before the transcript is shortened) and SessionEnd
# always run. SessionEnd also closes the session and records the work log.
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"
hooks_disabled && exit 0
hook_init
REASON="${1:-stop}"
have_bin || exit 0
[ -n "$SESSION_ID" ] && [ -f "$TRANSCRIPT" ] || exit 0

if [ "$REASON" = "stop" ]; then
  [ "$(hook_field stop_hook_active)" = "true" ] && exit 0
  STATE="${TMPDIR:-/tmp}/rusty-remind-me-hooks"
  mkdir -p "$STATE" 2>/dev/null || true
  # The session id names a file: keep it to safe characters.
  STAMP="$STATE/$(printf '%s' "$SESSION_ID" | tr -c 'A-Za-z0-9_-' '_').last"
  NOW="$(date +%s)"
  LAST="$(cat "$STAMP" 2>/dev/null || echo 0)"
  [ $((NOW - LAST)) -ge 60 ] 2>/dev/null || exit 0
  printf '%s' "$NOW" >"$STAMP" 2>/dev/null || true
fi

"$BIN" capture-transcript "$TRANSCRIPT" --session-id "$SESSION_ID" --cwd "$CWD" \
  --reason "$REASON" --json >/dev/null 2>&1 || true

if [ "$REASON" = "end" ]; then
  "$BIN" session end --session-id "$SESSION_ID" --cwd "$CWD" \
    --reason "$(hook_field reason)" >/dev/null 2>&1 || true
fi
exit 0
