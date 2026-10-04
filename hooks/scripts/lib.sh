#!/usr/bin/env bash
# Shared by the rusty_remind_me hooks. Sourced, never run.
#
# A hook must never block or fail the session, so nothing here uses `set -e`
# and every caller ends in `exit 0`. Claude Code passes the hook's input as
# JSON on stdin: session_id, transcript_path, cwd, hook_event_name, plus
# `prompt` (UserPromptSubmit), `trigger` (PreCompact), `reason` (SessionEnd)
# and `stop_hook_active` (Stop).
#
#   REMIND_ME_HOOKS=0           turns every hook off
#   REMIND_ME_PROMPT_CONTEXT=0  turns only the per-prompt injection off
#   REMIND_ME_BIN               the binary to call (default: rusty-remind-me)

BIN="${REMIND_ME_BIN:-rusty-remind-me}"
HOOK_JSON="$(cat 2>/dev/null || true)"

hooks_disabled() { [ "${REMIND_ME_HOOKS:-1}" = "0" ]; }
have_bin() { command -v "$BIN" >/dev/null 2>&1; }

# hook_field NAME: a top-level field of the hook input, as text ("" if absent).
hook_field() {
  printf '%s' "$HOOK_JSON" | python3 -c '
import json, sys
try:
    v = json.load(sys.stdin).get(sys.argv[1])
except Exception:
    v = None
print(v if isinstance(v, str) else ("" if v is None else json.dumps(v)), end="")
' "$1" 2>/dev/null || true
}

# hook_init: read the common fields and tell the CLI who is writing.
hook_init() {
  SESSION_ID="$(hook_field session_id)"
  CWD="$(hook_field cwd)"
  CWD="${CWD:-$PWD}"
  TRANSCRIPT="$(hook_field transcript_path)"
  export REMIND_ME_CWD="$CWD" REMIND_ME_WRITTEN_BY=hook
  if [ -n "$SESSION_ID" ]; then export REMIND_ME_SESSION_ID="$SESSION_ID"; fi
}

# emit_context EVENT: stdin is Markdown; print the hook's JSON answer.
# Hook stdout is capped at 10,000 characters, so the text is cut at 8,000.
emit_context() {
  python3 -c '
import json, sys
text = sys.stdin.read().strip()
if len(text) > 8000:
    text = text[:8000] + "\n\n...(truncated)"
if not text:
    print(json.dumps({"continue": True}))
else:
    print(json.dumps({"continue": True, "hookSpecificOutput": {
        "hookEventName": sys.argv[1], "additionalContext": text}}))
' "$1" 2>/dev/null || echo '{"continue": true}'
}

# brief_text: stdin is `context --json` output; print its Markdown.
brief_text() {
  python3 -c '
import json, sys
try:
    print(json.load(sys.stdin).get("context", ""), end="")
except Exception:
    pass
' 2>/dev/null || true
}

missing_bin_message() {
  echo '{"continue": true, "systemMessage": "rusty-remind-me is not on PATH, so the rusty_remind_me plugin cannot read or save memories. Build it (cargo build --release -p rusty-remind-me) and put target/release on PATH, or `cargo install --path crates/apps/rusty_remind_me/crates/remind_me_cli` from a Rusty-Mill/rusty_mill checkout."}'
}
