# Review: what `rusty_remind_me` captures, and what it should

Date: 2026-10-04. Scope: the information and context a memory carries, how it
gets there, and what the store cannot answer today. Not a code-quality review.

## 1. What is captured today

**One memory row** (`crates/remind_me_core/src/models.rs:60`, schema v31):

| Group | Fields |
| --- | --- |
| Text | `content`, `category`, `tags`, `memory_type` (set only by `reclassify`) |
| Structure | `subject` / `predicate` / `object`, entity mentions and relations (manual) |
| Lineage | `capture_id`, `source_capture_id`, `superseded_by`, `doc_id` + `chunk_index`, revisions |
| Provenance | `source` (free string), `node_id` (env), `client` (env or MCP `clientInfo`), `created_at`, `updated_at` |
| Lifecycle | `vitality`, `base_weight`, `decay_rate`, `access_count`, `accessed_at`, `status`, `deleted_at`, `remind_at`, `sensitive` |
| Open | `metadata` JSON, no schema; writers use `title`, `type`, `linked_*`, `import_id`, `filename`, `section`, `ingest`, `promoted_from`, and `code_refs` (`{path, mtime, size}`, only with `REMIND_ME_CODE_ROOTS` set) |

**Write paths.** `remind_me_add` (its MCP schema advertises content, category,
tags, source, sensitive; the input type also accepts metadata, the triple and
entities, unadvertised, `remind_me_mcp/src/lib.rs:491`), `remind_me_auto_capture`
(`capture.rs:112`: verbatim dialog + summary, two rows linked three ways),
`remind_me_decompose` (atomic facts with triples and entities), 12 import
formats, a polling folder watcher, and the promotion ladder
(capture → fact → scenario → persona, `promotion.rs`) with provenance edges.

**Plugin.** The MCP `initialize` reply carries no `instructions` text, so no
client is told when to capture. One `SessionStart` hook (`hooks/scripts/session-start.sh`) that
injects the 8 most recent memories, by recency only. Two slash commands wrap the
CLI (`remember`, `recall`). Nothing fires at `Stop`, `PreCompact` or
`SessionEnd`, so saving a conversation depends on the model remembering to call
`auto_capture`.

## 2. What the store cannot answer

- *Which project or repo was this about?* No project, cwd, remote URL or branch
  on any row. `grep project crates/remind_me_core/src/{models,capture}.rs` finds
  only wiki "wings".
- *Which session produced it, and what else happened in that session?* No
  session id, no episode grouping.
- *Is this still true, and how sure were we?* No validity window, no confidence,
  no verification state. Contradiction handling is supersede-or-nothing.
- *Why was this decided, what was rejected, did it stick?* Decisions are free
  text under `category: fact`. No rationale, alternatives or outcome fields.
- *Which PR, issue, commit, file or URL does this point at?* References live
  inside `content` and are found only by keyword search.
- *What did we actually do?* Captures hold what was said. Files changed, tests
  run and tasks closed are not recorded unless the summary mentions them.
- *Who or what wrote this, and how trustworthy is it?* `client` says which app.
  Nothing says model, hook or human, nor whether the capture was automatic.
- *When in the conversation was this said?* The chat importer keeps only role
  and content (`importer.rs:137`). Timestamps, session ids and tool blocks
  survive only in the optional byte archive (`REMIND_ME_ARCHIVE_DIR`).

## 3. Improvements, ranked

Estimates assume one person familiar with the crates. "Schema" means a
migration in `db/migrations.rs` plus the engine store in `db/engine/`.

### Quick wins (under half a day each, no schema)

- **Advertise what `remind_me_add` already accepts.** Add `metadata`,
  `subject`/`predicate`/`object` and `entities` to its MCP schema. Clients
  cannot send structure the schema hides.
- **Tell the client when to capture.** Put the capture policy in the MCP
  `initialize` reply's `instructions` field, so every client, not only the
  Claude Code plugin, learns to call `auto_capture` and `add` for durable facts.
- **Close two lineage holes.** `remind_me_annotate` should run the same
  contradiction supersession `decompose` runs, and `remind_me_provenance`
  should follow `source_capture_id` as well as the `promotions` table.

### Do now

1. **Automatic capture at session end and before compaction.** Add `Stop`,
   `PreCompact` and `SessionEnd` hooks. Claude Code passes `session_id`,
   `transcript_path` and `cwd`; a `rusty-remind-me capture-transcript <path>`
   subcommand parses the JSONL, writes the dialog half immediately and leaves
   the summary to `decompose`/`promotion_candidates`. Idempotent on
   `session_id` so repeated `Stop` events update one capture. Removes the
   single biggest loss: sessions that end without a save. ~1 day. No schema.
2. **Context envelope on every write.** New typed columns `project`,
   `session_id`, `git_remote`, `git_branch`, `git_sha`, `cwd`. Filled from the
   hook environment and from the MCP server's own `std::env::current_dir()`
   and `git rev-parse` at startup, with the daemon settings precedence already
   used for `REMIND_ME_CLIENT`. Exposed as `project:` and `branch:` search
   filters and in the Markdown renderer. ~2 days. Schema v32.
3. **Relevance-aware session start.** Replace `list --limit 8` with a search
   keyed on the current project and branch, plus `bootstrap: true` for the
   persona, plus open reminders and open action items (see 5). Add a
   `UserPromptSubmit` hook that injects the top 3 hits for the prompt under a
   token budget. Depends on 2. ~1 day.
4. **Automatic entity and reference extraction at write time.** A pure,
   rule-based pass in core: `owner/repo#123`, 7-40 hex commit shas, URLs,
   file paths, `@handles`, and capitalised multi-word names, resolved through
   the existing alias table. Stored as entity mentions plus a new `references`
   table (`memory_id`, `kind`, `value`). LLM extraction via `extract_batch`
   stays for the rest. Shrinks the annotation backlog without a model call.
   ~2 days. Schema.
5. **Typed memory kinds with validated metadata.** Promote `memory_type` to a
   closed enum and validate `metadata` per kind at the boundary: `decision`
   (rationale, alternatives, status, adr), `action_item` (due, status,
   owner), `preference`, `fact`. `remind_me_add` rejects a malformed kind
   rather than storing it. Markdown rendering per kind. ~2 days. Schema.

### Next

6. **Validity window and confidence.** `valid_from`, `valid_until`,
   `confidence` (0..1), `verified_at`. Retrieval down-ranks expired rows;
   `contradiction_candidates` prefers the newer, higher-confidence side;
   `stale_candidates` gains a real signal. ~2 days. Schema.
7. **Session episodes.** A `sessions` table (`session_id`, `client`,
   `project`, `started_at`, `ended_at`, `summary_memory_id`) and a
   `remind_me_session_timeline` tool. Answers "what did we do in rusty_mill
   last Tuesday" without a search query. Depends on 1 and 2. ~2 days.
8. **Work log derived from the repo, not the transcript.** At `SessionEnd`
   record `git diff --stat` against the session's starting sha, files touched,
   and the last test command's exit status, as a `work_log` memory linked to
   the session. Cheap, exact, and independent of what the summary says.
   Depends on 2 and 7. ~1 day.
9. **Outcome tracking.** `outcome` on decisions and action items (`done`,
   `abandoned`, `reverted`, `superseded`) set by a `remind_me_resolve` tool.
   Outcomes feed `base_weight` and persona promotion so a reverted decision
   stops shaping the persona. Depends on 5. ~2 days.
10. **Writer provenance.** `written_by` (`human`, `model:<id>`, `hook`,
    `importer:<name>`) and `capture_method` (`manual`, `auto`). Lets ranking
    and `feedback` weight a hook's verbatim capture differently from a
    model's summary. ~1 day. Schema.

### Later

11. **Keep what the chat importer drops.** It already reads Claude Code JSONL
    but discards timestamps, session ids and tool blocks. Keep timestamps and
    `session_id` on the row (feeding 7), summarise tool blocks as a work log
    (feeding 8), and point the watcher at `~/.claude/projects/` by default.
    Retroactive capture of everything before item 1 ships. ~2 days.
12. **Secret redaction at the boundary.** Scan content for key and token
    shapes before insert, redact, and tag `redacted`. Matches the existing
    "validate external input at the boundary" rule and keeps credentials out
    of a database that syncs to a hub. ~1 day.
13. **Attachment references.** Hash plus path or URL for images, PDFs and
    files a memory is about, never the bytes. ~1 day, after 4.

## 4. Suggested order

1 → 2 → 3 is the shortest path to "nothing is lost and the right memories
show up unprompted". 4 and 5 then make what is stored queryable by structure.
About two working weeks for the "do now" set.
