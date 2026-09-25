---
description: Save a memory to rusty_remind_me
argument-hint: <text to remember> [--category NAME] [--tags a,b,c]
allowed-tools: Bash(rusty-remind-me add *)
---

Store this in long-term memory via `rusty_remind_me`. The write already
happened below — do not call any memory-related tool to redo it.

!`rusty-remind-me add $ARGUMENTS`

Report back in one line: the memory ID and category from the output above.
If the output instead shows a shell error (e.g. "command not found"), tell
me `rusty-remind-me` isn't on `PATH` and needs to be built/installed first —
don't try to save the memory any other way.
