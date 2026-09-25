---
description: Search rusty_remind_me and answer from what it finds
argument-hint: <query> [--limit N]
allowed-tools: Bash(rusty-remind-me search *)
---

Search results for "$ARGUMENTS":

!`rusty-remind-me search $ARGUMENTS`

Answer using only the memories above. If the output shows a shell error
(e.g. "command not found"), say `rusty-remind-me` isn't on `PATH` and needs
to be built/installed first. If the search ran but found nothing relevant,
say so plainly instead of guessing or filling in from general knowledge.
