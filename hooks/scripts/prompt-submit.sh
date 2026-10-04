#!/usr/bin/env bash
# UserPromptSubmit hook: inject the few memories that match this prompt, under
# a small budget. The standing brief was injected at session start, so this
# asks for hits only. Short prompts and slash commands are skipped.
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"
off='{"continue": true}'
hooks_disabled && { echo "$off"; exit 0; }
[ "${REMIND_ME_PROMPT_CONTEXT:-1}" = "0" ] && { echo "$off"; exit 0; }
hook_init
PROMPT="$(hook_field prompt)"
case "$PROMPT" in /*) echo "$off"; exit 0 ;; esac
[ "${#PROMPT}" -ge 12 ] || { echo "$off"; exit 0; }
have_bin || { echo "$off"; exit 0; }

"$BIN" context --cwd "$CWD" --prompt "$PROMPT" --hits-only --budget 3000 --json 2>/dev/null \
  | brief_text | emit_context UserPromptSubmit
exit 0
