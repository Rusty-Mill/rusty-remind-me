#!/usr/bin/env bash
# SessionStart hook: surface recently-written memories as context, without
# requiring the model to decide to call the MCP `list`/`search` tool first.
# Must never block session start: any failure here degrades to a plain
# `{"continue": true}` rather than a non-zero exit.
set -euo pipefail

BIN="rusty-remind-me"

if ! command -v "$BIN" >/dev/null 2>&1; then
  echo '{"continue": true, "systemMessage": "rusty-remind-me is not on PATH, so the rusty_remind_me plugin cannot inject recent memories. Build it (cargo build --release -p rusty-remind-me) and put target/release on PATH, or `cargo install --path crates/apps/rusty_remind_me/crates/remind_me_cli` from a Rusty-Mill/rusty_mill checkout."}'
  exit 0
fi

MEMORIES="$("$BIN" list --limit 8 2>/dev/null || true)"

if [ -z "$MEMORIES" ]; then
  echo '{"continue": true}'
  exit 0
fi

printf '%s' "$MEMORIES" | python3 -c '
import json
import sys

MAX_CONTEXT_CHARS = 8000  # hook stdout is capped at 10,000 chars total

memories = sys.stdin.read()
context = "## Recent memories (rusty_remind_me)\n\n" + memories
if len(context) > MAX_CONTEXT_CHARS:
    context = context[:MAX_CONTEXT_CHARS] + "\n\n...(truncated)"

print(json.dumps({
    "continue": True,
    "hookSpecificOutput": {
        "hookEventName": "SessionStart",
        "additionalContext": context,
    },
}))
'
