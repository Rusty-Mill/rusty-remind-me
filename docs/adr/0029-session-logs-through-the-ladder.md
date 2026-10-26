# ADR-0029: Every session is captured, refined, and searched refined-first

Status: Accepted (2026-10-26); step 1 implemented
Date: 2026-10-26

## Context

ADR-0026 made the plugin save each Claude Code session through its `Stop`,
`PreCompact` and `SessionEnd` hooks. Sessions run from claude.ai Projects (the
cloud) never arrive. They run in a fresh container:

- user-level `~/.claude` settings do not sync, and the plugin is not among the
  synced claude.ai plugins, so no capture hook is registered;
- there is no `rusty-remind-me` binary, and `capture-transcript` writes to a
  local store; remind-me is reachable only as the remote MCP server;
- the transcript lives in the container's `~/.claude/projects` and is deleted
  with it, and `SessionEnd` may never fire when the container is reclaimed.

Importing local JSONL files (Mill Board #47 to #49) cannot reach those
sessions: their transcripts are never on the user's disk. The local hook
already covers CLI sessions.

The ladder above the raw capture (`decompose`, `promote`, ADR-0026's
provenance) exists but nothing drives it, and search ranks raw dialogs, which
match almost any query, alongside the facts drawn from them.

## Decision

1. **Raw rung: one capture per session, wherever it ran.**
   - `remind_me_capture_transcript(session_id, transcript, reason?, project?,
     git_remote?, git_branch?, git_sha?, cwd?)` exposes
     `transcript::capture_transcript` over MCP, so a hook on a machine with no
     CLI captures through the remote server and its bearer auth. The caller's
     context is stamped as given; its cwd is never probed on the server.
   - A transcript is at most `MAX_TRANSCRIPT_BYTES` (3 MiB), so it fits the
     remote server's 4 MiB body once escaped into JSON. A longer session sends
     its tail, cut at a line; the dialog keeps only the newest 200,000
     characters anyway.
   - Dedupe is by session id, not content hash: a growing transcript has a
     new hash every turn. The existing upsert replaces the pair and records a
     `recapture` revision.
   - The dialog keeps text and one line per tool call; tool results, thinking
     and sidechains are dropped. With `REMIND_ME_ARCHIVE_DIR` set, the latest
     raw transcript is archived under `session:<id>` (one copy per session,
     the previous blob unlinked) with the dialog's span over it, so
     `remind_me_source` returns what the dialog dropped. Retention is the
     archive's existing age and size limits.
2. **Cloud transport.** A hook committed to the repo's `.claude/settings.json`
   on `Stop` and `PreCompact` posts the transcript to the remote server when
   `CLAUDE_CODE_REMOTE=true` and `REMIND_ME_URL` and `REMIND_ME_TOKEN` are
   set. It is throttled to once a minute, skipped when re-entrant, and always
   exits 0. The environment allows the server's host and holds the token as a
   secret.
3. **Fact rung.** A scheduled routine decomposes captures not updated for two
   hours into `decision` (with `metadata.rationale`), `reference` (PR, merge,
   issue activity), `learning` (gotchas) and `preference` facts, each linked to
   its capture and stamped with the session. A recapture flags the capture
   for decomposition again; supersession by subject and predicate keeps a
   re-run from duplicating.
4. **Summary rung.** The same routine promotes `fact_to_scenario` candidates
   with at least `MIN_SCENARIO_FACTS` (3) facts on one entity drawn from at
   least two sessions, listing every source. The persona rung is unchanged.
5. **Search.** `remind_me_search` gains `include_raw` (default false), which
   leaves `dialog` and `skeleton` memories out of the ranking; `category:
   "dialog"` or a `session_id` filter turns it on. Results carry their
   `capture_id`. When nothing refined matches, the best raw dialogs come back
   in their own section. Hook context injection follows the same rule.

## Consequences

- A cloud session is captured with no CLI and no file sync. The cost is a
  server URL, a network allowance and a token in each cloud environment.
- `remind_me_capture_transcript` is outside the `core` and `standard` tool
  profiles, so a server run with a narrowed profile refuses it. A server that
  takes hook captures runs the `full` profile.
- Search stops returning raw dialogs by default: a behaviour change for
  existing callers, opt-out per call.
- Mill Board #48 and #49 are superseded; #47 remains a one-time backfill of
  local history.
- Delivered in four PRs: the tool and archive (step 1), the search default,
  the cloud hook, and the routine with the two-session rule.
