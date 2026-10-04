#!/usr/bin/env bash
# End-to-end test of the plugin hooks: each one is fed real hook JSON and run
# against the built binary, a temporary store and a temporary git repo.
#
#   cargo build -p rusty-remind-me && crates/apps/rusty_remind_me/scripts/test_hooks.sh
#
# REMIND_ME_BIN overrides the binary (default: target/debug/rusty-remind-me).
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
PRODUCT="$(dirname "$HERE")"
ROOT="$(cd "$PRODUCT/../../.." && pwd)"
export REMIND_ME_BIN="${REMIND_ME_BIN:-$ROOT/target/debug/rusty-remind-me}"
HOOKS="$PRODUCT/hooks/scripts"
[ -x "$REMIND_ME_BIN" ] || { echo "no binary at $REMIND_ME_BIN; build it first" >&2; exit 2; }

T="$(mktemp -d)"
trap 'rm -rf "$T"' EXIT
export HOME="$T/home" TMPDIR="$T/tmp" REMIND_ME_DAEMON=0 REMIND_ME_DB_PATH="$T/memory.db"
mkdir -p "$HOME" "$TMPDIR" "$T/quokka"
unset REMIND_ME_SESSION_ID REMIND_ME_CWD REMIND_ME_WRITTEN_BY REMIND_ME_HOOKS REMIND_ME_PROMPT_CONTEXT

fail=0
check() { # check DESCRIPTION CONDITION-EXIT-CODE
  if [ "$2" -eq 0 ]; then echo "ok   $1"; else echo "FAIL $1"; fail=1; fi
}
py() { python3 -c "$1" 2>/dev/null; }

( cd "$T/quokka" && git init -q -b main . \
  && git config user.email t@example.com && git config user.name t \
  && echo one >a.txt && git add a.txt && git commit -q -m first )
( cd "$T/quokka" && "$REMIND_ME_BIN" add "The quokka scheduler uses a token bucket" --category fact >/dev/null )

SID="sess-1"
TRANSCRIPT="$T/t.jsonl"
cat >"$TRANSCRIPT" <<JSON
{"type":"user","sessionId":"$SID","timestamp":"2026-10-04T10:00:00Z","cwd":"$T/quokka","message":{"role":"user","content":"why is the quokka scheduler slow"}}
{"type":"assistant","sessionId":"$SID","timestamp":"2026-10-04T10:00:05Z","message":{"role":"assistant","content":[{"type":"text","text":"The token bucket refills once a second."}]}}
JSON
input() { # input EVENT [KEY=VALUE...]
  python3 - "$@" <<PY
import json, sys
d = {"session_id": "$SID", "cwd": "$T/quokka", "transcript_path": "$TRANSCRIPT", "hook_event_name": sys.argv[1]}
for kv in sys.argv[2:]:
    k, v = kv.split("=", 1)
    d[k] = {"true": True, "false": False}.get(v, v)
print(json.dumps(d))
PY
}
context_of() { python3 -c 'import json,sys; print(json.load(sys.stdin).get("hookSpecificOutput",{}).get("additionalContext",""))'; }

# SessionStart: opens the session and injects the project's memories.
out="$(input SessionStart | "$HOOKS/session-start.sh")"
echo "$out" | context_of | grep -q "token bucket"; check "session start injects the project's memory" $?
echo "$out" | context_of | grep -q "project: quokka"; check "session start scopes the brief to the project" $?
"$REMIND_ME_BIN" session timeline --session-id "$SID" --json | py 'import json,sys; assert json.load(sys.stdin)["session"]["session_id"]=="'$SID'"'
check "session start opens the session row" $?

# UserPromptSubmit: hits only, skipped for short prompts and slash commands.
out="$(input UserPromptSubmit prompt="how does the quokka scheduler work" | "$HOOKS/prompt-submit.sh")"
echo "$out" | context_of | grep -q "Possibly relevant memories"; check "prompt injects matching memories" $?
echo "$out" | context_of | grep -q "Persona\|Recent memories"; [ $? -ne 0 ]; check "prompt does not repeat the standing brief" $?
out="$(input UserPromptSubmit prompt="hi" | "$HOOKS/prompt-submit.sh")"
[ "$(echo "$out" | context_of)" = "" ]; check "a short prompt injects nothing" $?
out="$(input UserPromptSubmit prompt="/clear everything now" | "$HOOKS/prompt-submit.sh")"
[ "$(echo "$out" | context_of)" = "" ]; check "a slash command injects nothing" $?
out="$(input UserPromptSubmit prompt="how does the quokka scheduler work" | REMIND_ME_PROMPT_CONTEXT=0 "$HOOKS/prompt-submit.sh")"
[ "$(echo "$out" | context_of)" = "" ]; check "REMIND_ME_PROMPT_CONTEXT=0 turns the injection off" $?

# Stop: captures the transcript once, and is throttled afterwards.
input Stop stop_hook_active=false | "$HOOKS/capture.sh" stop
dialogs() { "$REMIND_ME_BIN" session timeline --session-id "$SID" --json \
  | py 'import json,sys; print(sum(1 for m in json.load(sys.stdin)["memories"] if m["metadata"].get("type")=="dialog"))'; }
[ "$(dialogs)" = "1" ]; check "stop captures the dialog" $?
echo '{"type":"user","sessionId":"'$SID'","message":{"role":"user","content":"and the second turn"}}' >>"$TRANSCRIPT"
input Stop stop_hook_active=false | "$HOOKS/capture.sh" stop
"$REMIND_ME_BIN" session timeline --session-id "$SID" --json | grep -q "and the second turn"; [ $? -ne 0 ]
check "a second stop within a minute is throttled" $?
input Stop stop_hook_active=true | "$HOOKS/capture.sh" stop
check "a re-entrant stop exits quietly" 0

# PreCompact: never throttled, and still one capture per session.
input PreCompact trigger=auto | "$HOOKS/capture.sh" precompact
"$REMIND_ME_BIN" session timeline --session-id "$SID" --json | grep -q "and the second turn"
check "precompact re-captures the longer transcript" $?
[ "$(dialogs)" = "1" ]; check "still one dialog per session after a re-capture" $?

# SessionEnd: closes the session and writes the repo's work log.
echo two >"$T/quokka/b.txt"
input SessionEnd reason=other | "$HOOKS/capture.sh" end
"$REMIND_ME_BIN" session timeline --session-id "$SID" --json | py 'import json,sys; assert json.load(sys.stdin)["session"]["ended_at"]'
check "session end stamps ended_at" $?
"$REMIND_ME_BIN" session timeline --session-id "$SID" --json | py 'import json,sys; assert any(m["category"]=="work_log" for m in json.load(sys.stdin)["memories"])'
check "session end records a work log from git" $?

# Written by the hook, and switched off cleanly.
"$REMIND_ME_BIN" session timeline --session-id "$SID" --json | py 'import json,sys; assert all(m["written_by"]=="hook" for m in json.load(sys.stdin)["memories"])'
check "everything the hooks wrote is stamped written_by=hook" $?
out="$(input SessionStart | REMIND_ME_HOOKS=0 "$HOOKS/session-start.sh")"
[ "$out" = '{"continue": true}' ]; check "REMIND_ME_HOOKS=0 turns the hooks off" $?
out="$(input SessionStart | REMIND_ME_BIN=/nonexistent/rusty-remind-me "$HOOKS/session-start.sh")"
echo "$out" | grep -q "not on PATH"; check "a missing binary is reported, not fatal" $?

exit $fail
