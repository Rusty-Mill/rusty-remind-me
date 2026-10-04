# ADR-0026: Memories record their context, and the plugin captures without being asked

Status: Accepted (2026-10-04); implemented
Date: 2026-10-04

## Context

The 2026-10-04 review (`docs/reviews/2026-10-04-context-capture-review.md`)
found that a memory recorded what was said and almost nothing about where,
by whom, how sure, or whether it held. No row knew its project, branch or
session. A conversation was saved only if the model remembered to call a
tool, and the one plugin hook listed the eight newest memories whatever the
project. References to issues, commits and paths lived inside free text.

## Decision

1. **Schema v32 adds thirteen columns to `memories`.** Context: `project`,
   `session_id`, `git_remote`, `git_branch`, `git_sha`, `cwd`. Trust:
   `valid_from`, `valid_until`, `confidence`, `verified_at`, `outcome`.
   Provenance: `written_by`, `capture_method`. All nullable or defaulted, so
   an old row reads with defaults.
2. **Two engine tables, no SQLite mirror (ADR-0025).** `memory_references`
   (kind, normalised value, label) and `sessions` (project, branch, start and
   end commit, links to the summary and work-log memories).
3. **Engine records are positional, so a layout change bumps the table's
   schema tag and ships an open-time upgrade.** `memories` moved from `@1` to
   `@2` with a reader for the old layout, on the node and on the hub. Later
   work must follow the same rule.
4. **Context comes from the environment, at the boundary.** `context.rs`
   reads `REMIND_ME_CWD`, `REMIND_ME_SESSION_ID`, `REMIND_ME_WRITTEN_BY` and
   `REMIND_ME_MODEL` through the daemon's session variables, so a daemon
   serving many directories stamps each caller's own. The git probe is
   cached per directory for 30 seconds. `written_by` resolves as: a valid
   override, else `human` for the CLI, `hook` for a hook, `importer:<name>`
   for an importer, `model` for an MCP process.
5. **Kinds are validated, not free text.** `memory_type` is a closed set. A
   `decision` must carry `metadata.rationale`. Validation runs on the MCP and
   CLI entry points; the core reclassify and decompose paths stay permissive
   because existing callers rely on it. Per-kind data lives in `metadata`, not
   in new engine fields.
6. **Retrieval honours trust.** Ranking multiplies by `confidence`, and an
   expired or not-yet-valid memory is scaled by 0.25 and marked, never hidden.
   A decision or action item whose `outcome` is `reverted` or `abandoned` no
   longer feeds persona promotion. A hook or importer write counts half as
   feedback.
7. **Write-time passes are rules, not a model.** `extract.rs` finds issues,
   pull requests, commits, URLs, paths and handles; `redact.rs` replaces
   secrets before anything is stored; attachments keep a SHA-256 and a label.
   A bare `#123` is not extracted, because it is ambiguous.
8. **The plugin captures and recalls through hooks.** `SessionStart` and
   `UserPromptSubmit` inject context; `Stop`, `PreCompact` and `SessionEnd`
   save the transcript through `capture-transcript`, one capture per session
   with its dialog replaced on each run. `Stop` is throttled to once a minute
   and skipped when re-entrant. Every hook exits 0 and degrades to
   `{"continue": true}`.
9. **The MCP `initialize` reply carries `instructions`** saying when to
   capture, so a client without the plugin is told too.

## Consequences

- A memory can be asked for by project, branch, session or writer, and a
  session can be replayed with `remind_me_session_timeline`.
- A session that ends without anyone calling a memory tool still leaves a
  capture and, when the repository changed, a work log.
- Every memory row is larger. Existing engine stores upgrade on first open,
  and the upgrade is one way: an older build refuses the store, for every
  read and write, and leaves it unchanged (checked by hand against the
  previous build). The refusal says which version to use or to restore a
  backup. The engine crate itself carries no format version; compatibility is
  the per-table schema tag.
- A session's memories are found by scanning live memories and filtering in
  Rust, since the engine has no `session_id` index. Fine at hook cadence; add
  an index if it shows up in a profile.
- Redaction is pattern-based and will miss a secret with no recognisable
  shape. It lowers the chance of storing credentials and does not remove it.
- `remind_me_references` and `remind_me_session_timeline` are only in the
  `full` tool profile, because `core` is capped below 20 tools and
  `remind_me_resolve` filled it. Widening `core` is a separate decision.

## Not done

- A `dump` command to replace `sqlite3 memory.db` for inspection.
- Scoping the dashboard's `PageFilter` search by project.
- Reading `_meta.model` from MCP calls; only `REMIND_ME_MODEL` sets the model id.
- Cloud upload of engine-directory backups.
