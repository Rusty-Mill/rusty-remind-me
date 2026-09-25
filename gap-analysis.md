# Gap Analysis — `rusty_remind_me` vs. `remind_me`

**Run date:** 2026-08-07 (surface counts re-derived; earlier revisions
2026-08-05 and 2026-08-06)
**Target:** `baileyrd/rusty_remind_me` @ `68ae0a9`, 6 crates
**Reference (pinned):** `baileyrd/remind_me` @ `f199a11` — **v1.54.0**
**Previous run:** 2026-08-03 against target `a2cce8b` / reference `935eb98`.
That analysis is superseded in full: its entire gap table was worked to
completion (issues #100–#122, PRs #123–#166), and every headline number it
reported has moved.

**2026-08-08 re-verification.** This document's headline table was derived by
reading both codebases, and said so. A follow-up pass ran the two
implementations against one shared database and one shared shell instead —
the method the "What is guarded, and what is only true" section below already
recommended — and found two things reading had missed, both closed in
[#231](https://github.com/baileyrd/rusty_remind_me/pull/231):

- `remind_me_list` / CLI `list`, Markdown format, was missing the
  `**Showing N of M memories**` pagination header the reference's
  `_fmt_memories` prepends when it's given a `total` (crud.py:189-196). The
  shared `render_memories_markdown` was correct for
  `remind_me_list_reminders`, which never passes a `total` — the gap was in
  reusing that renderer for the one caller that needed the header too.
- `scripts/check_schema_drift.sh` — the one drift check this document calls
  "guarded automatically" — silently exited 2 ("could not determine") on any
  machine with `CDPATH` set, because `cd` echoes its resolved directory to
  stdout under `CDPATH`, corrupting the script's `repo_root` capture. It never
  went red; it just stopped being able to compare. Re-run clean against true
  `origin/main` afterward: 29/29, confirming the schema row below still holds.

Neither changes the headline numbers. Both are recorded here as the pattern,
not the exception: this is the second and third time a live run found what
two prior revisions of this document, reading, did not (see "Drop-in verified
against a live database, 2026-08-07" below for the first).

**2026-08-08, later the same day: the dashboard and remote connector went
live too.** Following up on the "no dashboard" mistake a few paragraphs
above (#233, corrected in the "Standing state" section below) with an actual
production cutover — not another read — found two more real gaps and closed
both, then finished the switch: `remind-me-hub`, this machine's stdio MCP
client, the dashboard, and the claude.ai remote connector are now all served
by `rusty_remind_me`. Only Postgres (unchanged — same engine, new app layer
only) and this machine's already-running stdio sessions (pick up the switch
on their next restart, not mid-session) are still what they were.

- `GET /api/stats` answered with `remind_me_stats`'s MCP-tool shape
  (`total_memories`/`total_imports`, no `tags`) instead of the reference's
  own dashboard-specific shape (`total`/`imports`/`tags`, `api.py:531-562`)
  — two different response shapes in the reference itself, conflated into
  one here. The vendored JSX reads `stats.total`/`stats.tags` with a `||0`
  fallback, so the wrong field names failed by rendering every count as zero
  rather than by erroring. Closed in
  [#234](https://github.com/baileyrd/rusty_remind_me/pull/234), found by
  actually loading the dashboard in a browser against a copy of the real
  337MB production database before deploying it — the same method, applied
  one route earlier than last time.
- `rusty-remind-me api <port>` had no equivalent to the reference's
  `--ui-host`: the bind host was a hard-coded `127.0.0.1`, with no way to
  reach a Tailscale IP or any other non-loopback interface — exactly what
  this production dashboard needed, tunnelled publicly from
  `100.83.168.90:5199`. Not degraded, unreachable. Closed in
  [#235](https://github.com/baileyrd/rusty_remind_me/pull/235): an optional
  second positional argument, `rusty-remind-me api <port> [host]`, matching
  how this subcommand already takes its config from argv rather than the
  environment.
- The two connector path divergences logged earlier today (`~/.remind_me`
  vs `~/.remind-me` for the token file and OAuth state file) were exercised
  for real, not just documented: `REMIND_ME_REMOTE_TOKEN_FILE` and
  `REMIND_ME_REMOTE_OAUTH_STATE_FILE` pointed at the reference's real paths,
  then verified against production — a real registered claude.ai OAuth
  client from the live `oauth.json` (4 registered, 475 issued access tokens)
  authorized correctly (`302`, not `invalid_client`), a bogus client id was
  rejected, the real legacy bearer token from the live `connector_token`
  authenticated (`200`), and a wrong one didn't (`401`). Rehearsed first
  against copies with distinct inodes from the live files, confirmed via
  `lsof` before ever pointing at the real paths — the previous rehearsal in
  this same session skipped that discipline and triggered an unplanned
  schema migration on the live database (harmless, data-only, auto-backed
  up, but avoidable).

---

## Headline: surface parity holds — and surface was never the whole claim

Every number below was re-derived independently for this revision, from both
codebases, rather than carried forward.

| Surface | `remind_me` v1.54.0 | `rusty_remind_me` | Covered |
| --- | --- | --- | --- |
| MCP tools | 61 | 61 + **2** target-only | **100%** |
| HTTP API routes | 25 | 25 | **100%** |
| Peer-server routes | 7 | 7 | **100%** |
| SQLite schema version | 29 | 29 | **match** |
| SQLite tables / indexes / triggers | — | no missing object | **100%** |
| Import formats | 12 extensions | 12 | **100%** |

The previous run reported 70% tool coverage, 80% routes, 83% tables, and a
schema 8 steps behind. All of it closed.

Two corrections to the 2026-08-05 numbers, both found by re-deriving rather
than re-reading:

- **Target-only tools are 2, not 1** — `remind_me_entity_upsert` and
  `remind_me_wiki_import`. The earlier count missed one.
- The route comparison needs care: `/api/reminders/{token}.ics` has no matching
  string literal in the target, because it is served by prefix dispatch
  (`api_reminders_ics`). A naive literal diff reports it missing. It is not.

### The caveat this table earned

**A 100% here means names and paths match. It has never meant the responses
do.** This document said "the port has reached surface parity" on 2026-08-05
and was correct. The sweep that followed (issues #196–#205) then found roughly
forty divergent *response fields* behind those matching tool names — missing
`count`, `annotated`, `status`, `shared_entities`, and six `Memory` fields.

The same shape recurred twice more:

- **#167 closed the missing `list` subcommand and left the missing flags** on
  the two subcommands that already existed. `add` and `search` silently folded
  `--category`, `--tags`, `--limit` and `--json` into their positional text
  (#216, fixed in #220).
- **The drop-in claim was true for data and false for configuration** — the two
  implementations could not be pointed at one file (#218, fixed in #219).

Read the table as *coverage of the enumerable surface*, and assume nothing
about behaviour behind it that is not separately tested.

**Method.** The two codebases share no structurally diffable surface (Python
package vs. Cargo workspace), so `cargo public-api` does not apply — the
assessment path is `spec`, same as the last run. Tools were extracted from
`@mcp.tool(name=…)` registrations against the target's dispatch table; routes
from `api.py`'s `Route(...)` list and `peer_server.py`'s path dispatch; schema
by parsing every `CREATE TABLE/INDEX/TRIGGER` in the reference package and
materializing the target's `schema_*.sql` into a real SQLite database, then
diffing object-by-object.

### Reference drift since the last pin — already absorbed

The reference advanced 5 commits past `935eb98`. Two are substantive, and
**both were already ported before this run began**:

| Reference commit | What | Target |
| --- | --- | --- |
| `e7523d7` | Claude Code JSONL envelope unwrap in the importer | present — `importer.rs:223`, same branch placement |
| `ebf555e` | `server_status` stops reporting a recovered sync as failing | present — `sync/worker.rs` `superseded_error`, with `sync_error_supersession_test.rs` |

The remaining three are a lint fix and two merge commits.

### Drift absorbed on 2026-08-06

The reference then merged three changes of its own, which put the port two
schema versions behind for the first time since this document was written.
All three are now ported; the schema row above reads 29/29 as a result.

| Reference issue | What | Schema | Port |
| --- | --- | --- | --- |
| #167 | the client sends the hub's `since_seq` cursor | v27 → v28 (`sync_log.last_pull_seq`) | `sync/pull.rs`, with `sync_seq_cursor_test.rs` |
| #220 | a `reference` memory_type, and refiling for it | v28 → v29 (data only) | `vitality.rs` + `db/migrations.rs`, with `reference_memory_type_test.rs` |
| #219 | keyset pagination for contradiction candidates | none | `contradictions.rs` |

Two of these were worse on the port than on the reference, which is worth
recording because it is the general shape of this kind of drift:

- **#220 crosses the shared database.** Tenet 3 means both implementations read
  the same file, so a `reference` row written by `remind_me` and read here fell
  through the decay table's catch-all and aged at 0.10 rather than 0.03 —
  silently, with nothing on either side reporting a disagreement.
- **#167 was half-built here.** `remind_me_hub` already *served* `since_seq`;
  only the client never asked for it. The port therefore shipped the fix and
  could not use it.

**This drift is now checked automatically.** `scripts/check_schema_drift.sh`
compares this repo's `SCHEMA_VERSION` against the reference's
`_SCHEMA_VERSION` on `remind_me`'s default branch, and CI runs it on every PR
plus daily on a schedule — daily because the job compares against *another
repository*, so it can turn red with nothing here changing, which is exactly
how this drift opened. It distinguishes "the versions differ" (exit 1) from
"the check could not determine them" (exit 2) and never treats a failed
extraction as a pass; the previous state of the world was that a manual read
was the only signal, and it arrived a day late.

Regenerating the schema for this also surfaced a real bug in the reference:
`_ensure_schema` had come to require `row_factory = sqlite3.Row`, which its own
contract does not ask for, so `scripts/regenerate_schema.py` — the ADR-0007
method — could not run at all. Fixed upstream in `remind_me` #228. Its whole
test suite was blind to it because every caller sets the factory a line before
calling in.

### Drop-in verified against a live database, 2026-08-07

The schema table above says the two implementations *can* share a file. That had
never actually been done. It has now: a real `remind_me`-created v29 database was
copied, opened by the port, written to, and handed back.

| Step | Result |
| --- | --- |
| port reads the reference's rows | all present, metadata and timestamps intact |
| port writes a row | ok |
| schema version after the port touched it | **29 — no migration fired** |
| reference re-opens and reads the port's row | every column sane |
| reference writes again afterward | ok |

**Drop-in on the data is real.** Three divergences turned up in the doing, none
of which any amount of reading had surfaced:

| Issue | What | Status |
| --- | --- | --- |
| [#216](https://github.com/baileyrd/rusty_remind_me/issues/216) | `add`/`search` swallowed the reference's CLI flags into their positional text — silently, because a `join` cannot reject an unknown flag the way `argparse` does | closed, [#220](https://github.com/baileyrd/rusty_remind_me/pull/220) |
| [#218](https://github.com/baileyrd/rusty_remind_me/issues/218) | The two could not be aimed at one file: different variables (`REMIND_ME_DB_PATH` vs `REMIND_ME_MCP_DIR`) *and* different defaults, the port's relative to the working directory | closed, [#219](https://github.com/baileyrd/rusty_remind_me/pull/219) |
| [#217](https://github.com/baileyrd/rusty_remind_me/issues/217) | Memory ids diverge in format (`sha256(content+ts)[:12]` vs `mem_` + uuid4) in a shared column | closed, [#221](https://github.com/baileyrd/rusty_remind_me/pull/221) — documented and pinned, **not changed**; see `docs/adr/0016` |

#218 is the one worth remembering. Setting the port's variable against the
reference is *ignored* — both commands succeed, print sensible output, and
operate on different databases. That is how a test write ended up in a real
memory store during this very investigation.

**Method note.** Every one of these came from running the two implementations
against one file. None came from reading either codebase, and the preceding two
revisions of this document did not find them.

---

## Scope definition

`rusty_remind_me` still has no `ROADMAP.md`. `ARCHITECTURE.md` §1 Tenet 3
remains the hand-curated scope statement and is again treated as the
definition of parity:

> **Data Parity with `remind-me`**: Identical SQLite schema and JSON tool
> signatures for drop-in interoperability.

**That tenet is satisfied to its letter.** Schema version, every schema object,
every tool name, and every route match, and as of 2026-08-07 the drop-in claim
has been executed against a live reference database rather than inferred from
the schema.

Its letter is narrower than it reads, though, and the three findings of
2026-08-07 all landed in the margin. "Identical JSON tool signatures" says
nothing about the *fields in the response*, which is where forty divergences
sat (#196–#205). "Drop-in interoperability" says nothing about *finding the
same file*, which took two differently-named variables and did not work by
default (#218). Neither omission is a failure of the tenet — it is a scope
statement, not a test suite — but treating it as the definition of parity means
the definition stops short of what a user would call parity.

What follows is the residue: things the reference has that the port does not,
which sit outside the tenet's letter but inside "this is the successor."

`remind_me`'s `BACKLOG.md` is the reference's own improvement backlog, not a
roadmap for the port, and is again deliberately **not** used as the scope
source.

---

## Gap table

| ID | Symbol | Category | Source | Platforms | Reference | Breaking? | Size | Issue |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| **C1** | `cli list` | CLI subcommand | spec | both | `cli.py:74` — `SUBCOMMANDS = {"add","search","list"}` | no | S | [#167](https://github.com/baileyrd/rusty_remind_me/issues/167) |
| **D1** | `watchdog` | module | spec | both | `watchdog.py` (ref issue #128) | no | S | [#168](https://github.com/baileyrd/rusty_remind_me/issues/168) |
| **E5** | `sidecars` | module | spec | both | `sidecars.py` | no | M | [#169](https://github.com/baileyrd/rusty_remind_me/issues/169) |
| **E1** | the hub | deployable | spec | — | `hub/` — 1,341 LOC, 10 routes | — | XL | **not filed — scope decision; now ported, ADR-0015** |

All three filed gaps are pure additions. None touches an existing public
signature, so none is `breaking-change`-labelled.

### C1 — the `list` CLI subcommand

The reference dispatches `add`/`search`/`list`; the target dispatched
`add`/`search` and ten others, but not `list`. The tool logic
(`remind_me_list`, `queries::list_memories`) already existed — only the
command-line route was missing.

**Incompletely closed, and worth recording as the pattern.** #167 built `list`
a real flag parser and stopped there, because this row named a missing
*subcommand*. The missing *flags* on `add` and `search` — the two subcommands
that already existed — were never in scope and survived another five months of
"100% CLI coverage" (#216, closed by #220). A gap defined by the wrong unit
closes at the wrong boundary.

### D1 — the stuck-call watchdog

Nothing in the target reported *which* tool call was hung; the only external
symptom was the client's timeout.

Worth recording, because it constrains any future work here: the reference
uses `faulthandler.dump_traceback_later`, which dumps every thread's stack
including one blocked in synchronous CPU-bound code. **Rust has no stdlib
equivalent.** A faithful port needs a stack-unwinding crate — a new
third-party dependency for a diagnostic. The implementation reports identity
and duration instead, and says so in its own module docs.

**Closed behind a feature.** `stack-dumps` (Linux-only, off by default) now
dumps every thread's stack exactly as the reference does, by `ptrace`-ing this
process from a short-lived child. Feature-off behaviour is unchanged, so the
paragraph above still describes the default build. The estimate in it was
optimistic in one respect worth correcting: the cost is a *system* library
(`libunwind-ptrace`) and permission to `ptrace`, not merely a crate. See
`docs/adr/0014`.

### E5 — sidecar processes

No counterpart existed: `Command::new` appeared only in `updater.rs` for
git/cargo one-shots, and `REMIND_ME_TUNNEL` nowhere.

The finding that shaped the implementation: **the reference's teardown
guarantee is Windows-only.** Its `_job()` returns `None` immediately when
`sys.platform != "win32"`, so on Linux and macOS the reference orphans its
sidecars on abnormal exit exactly as a naive implementation would. The gap
against the reference was therefore one platform/exit cell, not four — see
`docs/adr/0013`.

**Closed in [#186](https://github.com/baileyrd/rusty_remind_me/pull/186).**
That one cell is now matched: children are assigned to a Windows Job object
with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, taking the workspace's first FFI
dependency (`windows-sys`, target-gated to `cfg(windows)`). ADR-0013 is
amended in place. The Unix abnormal-exit row still orphans, in both
implementations — deliberately, since closing it would overshoot the
reference. The Windows path is type-checked against
`x86_64-pc-windows-gnu`, not runtime-tested; CI is ubuntu-only.

### E1 — the hub

**Deliberately not filed.** `hub/main.py` is 1,341 LOC of Python serving
`/sync/pull`, `/sync/push`, `/count`, `/health`, `/metrics`, `/stats`,
`/admin/compact_tombstones`, deployed as a container (Quadlet, Compose, Fly,
Railway) with its own Postgres.

This is not an addition to the Rust workspace — it is a separate deployable in
another language and runtime, and the target already implements the
*peer* side of the same protocol (all 7 peer routes). Whether the successor
ships its own hub, or keeps syncing against the existing Python one, is a
scope decision that belongs to a human. Filing it as a `parity-gap` issue
would imply the loop should work it unattended, which it should not.

**Resolved: ported.** The reasoning above is kept as the record of why this was
never filed, but the decision has since been made and taken. The answer was to
ship a hub, with storage behind a `HubStore` trait — Postgres as a drop-in for
an existing deployment, SQLite for a self-hosted one wanting no database server
(`docs/adr/0015`). `crates/remind_me_hub` serves all ten routes, and its
deployment packaging — Containerfile, `setup.sh`, `client-setup.sh`, and
Quadlet/Compose/Fly/Railway templates — followed in
[#190](https://github.com/baileyrd/rusty_remind_me/pull/190).

**Asked, answered, and ported.** All ten routes now exist as `remind_me_hub` /
`rusty-remind-me-hub`, with storage behind a trait: Postgres (a drop-in for an
existing deployment, legacy migration included) and SQLite (self-hosted, one
file, no server). `docs/adr/0015` records the decision.

The port found a real bug in the reference's legacy migration — its
trailing-zero regex turns `.500000` into `.5`, which sorts *before* the
client's own value under `COLLATE "C"` and so corrupts both the pull cursor and
LWW conflict resolution. That is the one deliberate behavioural divergence in
the whole hub, and it was found by running the migration against a live
Postgres rather than by reading it.

---

## Deliberate divergences

Places the port knowingly differs. Each is a decision, not a gap, and is listed
here so "parity" is never read as "identical".

| What | Port | Reference | Why |
| --- | --- | --- | --- |
| `response_format` default | JSON | Markdown | Flipping it would break every existing caller to imitate a limitation. Markdown is opt-in and fully available (#206, #211). **The defaults still differ.** |
| CLI `search` output | Markdown, `--json` opts in | same | Matched deliberately in #220 — and it means `search` output changed for existing scripts. |
| Memory id format | `mem_` + uuid4 | `sha256(content+ts)[:12]` | The reference's collides on identical content within one timestamp resolution. `docs/adr/0016` |
| Vector store | `vec_embeddings` + Rust cosine scan | `vec0` virtual table | No loadable `sqlite-vec` available to this crate. Neither side reads the other's vectors; the shared `vec_chunks` rowid map matches exactly. `docs/adr/0002` |
| Sync pull cursor | advances only over records that **applied** | advances over every record received | The reference can skip a record it failed to apply and never see it again. |
| Hub legacy migration | keeps trailing zeros | regex strips them | A real bug in the reference: `.500000` → `.5` sorts *before* the client's own value under `COLLATE "C"`, corrupting both the pull cursor and LWW resolution. `docs/adr/0015` |
| `estimated_tokens` | `(len / 4).max(1)` | `len / 4` | Bare division estimates zero tokens for content under 4 characters. |
| Sidecar teardown, Unix abnormal exit | orphans | orphans | Matched deliberately. The reference's job-object guarantee is Windows-only; closing the Unix case would overshoot. `docs/adr/0013` |

Entity and relation ids are deliberately **not** on this list: they are
content-addressed in both, byte for byte, because the determinism is how two
peers agree on one entity without coordinating. Pinned by
`id_format_test.rs` against values computed by the reference itself.

---

## What is guarded, and what is only true

Worth separating, because the difference is where the next surprise comes from.

**Guarded automatically:** the schema version, by
`scripts/check_schema_drift.sh` on every PR and daily on a schedule — daily
because it compares against *another repository* and can turn red with nothing
here changing.

**True, but only checked when someone re-runs this analysis by hand:** the tool
list, the route lists, response field sets, and CLI flag sets. Every one of the
three 2026-08-07 findings lived in that second category. Nothing currently
notices if the reference adds a tool, changes a response field, or adds a flag.

That is the standing hole in "parity is structurally guaranteed", and it is
larger than any specific unported feature.

---

## Verified as *not* gaps

Recorded because each looked like one:

- **`memories_vec`** — the reference's `vec0` virtual table for embedding
  vectors has no target counterpart. This is a documented, deliberate
  divergence, not an omission: `docs/adr/0002` records why the target uses a
  plain `vec_embeddings` table and a Rust-side cosine scan (no loadable
  `sqlite-vec` extension available to this crate), and notes that neither side
  reads the other's vector store, so a shared database is unaffected. The
  `vec_chunks` rowid map — the part that *is* shared — matches exactly.
- **`webhook_server.py`** — present as `webhook.rs`, including the
  constant-time token comparison and the off-unless-a-secret-is-set default.
- **`ics_export.py`** — present as `ics.rs`.
- **`query_expansion.py`** — present as `expansion.rs`.
- **`exporter.py`** — present as `export.rs`.
- **`storage_interfaces.py`** — interface documentation, no runtime behavior.
- **`benchmarks/`** — a reference-side evaluation harness, not served surface.
- Python packaging (`pyproject.toml`, `uv.lock`, hatchling).

---

## Stop-and-ask items

Under the parity-loop skill's rules these are never auto-implemented. All
three were put to a human; all three came back yes and are done.

1. ~~**E1, the hub**~~ — **asked, answered, and done.** A new deployable in
   another language, so the scope decision came first, as it should have. The
   answer was to port it, with both storage backends behind a trait; see
   `docs/adr/0015`.
2. ~~**The Windows Job object for E5**~~ — **asked, answered, and done in
   [#186](https://github.com/baileyrd/rusty_remind_me/pull/186).** It was
   stopped-and-asked precisely because it needed a direct `windows-sys`
   dependency against a workspace that had deliberately had no FFI dependency
   at all (`docs/adr/0012` took the same decision against `libc`). The answer
   was to take it, target-gated to `cfg(windows)`; ADR-0013 is amended in
   place rather than quietly contradicted. ADR-0012's refusal of `libc`
   stands — that probe has a pure-`std` alternative and `CreateJobObjectW`
   does not.
3. ~~**A stack-dumping crate for D1**~~ — **asked, answered, and done.** The
   framing in the row above was too generous to itself: there is no pure-Rust
   way to dump another thread's stack, so the real cost was a *system* library
   (`libunwind-ptrace`) plus permission to `ptrace`, not just a crate. Taken
   behind an off-by-default, Linux-only `stack-dumps` feature. The in-process
   signal-handler alternative was rejected because capturing a backtrace is not
   async-signal-safe and would deadlock precisely when the diagnostic fires.
   `docs/adr/0014` records it.

None was taken unattended. **All three are now decided and done** — E1 was the
last, and its answer landed in [#189](https://github.com/baileyrd/rusty_remind_me/pull/189)
(the hub) and [#190](https://github.com/baileyrd/rusty_remind_me/pull/190)
(its deployment packaging).

---

## Sequencing

All three filed gaps are independent — no ordering constraint. They were
worked C1 → D1 → E5, smallest first.

---

## Standing state, 2026-08-07

**The gap table is empty.** C1, D1 and E5 are closed; E1 was decided and ported.
All three stop-and-ask items were put to a human, answered, and done. The
reference has no commits past `f199a11` to absorb, and the schema is in parity
at v29.

**Open issues are not parity gaps.** #207 (symbolic compression), #208
(refinement ladder), #209 (provider abstraction beyond Ollama) and #212 (raw
envelope retention) are all `enhancement`, and none traces to `remind_me`'s
`BACKLOG.md` — which this analysis deliberately does not use as a scope source
anyway. Working them moves the port *ahead* of the reference rather than toward
it, which is a different decision.

**What would find the next gap.** Not another read of both codebases — two
revisions of this document did that and missed all three of the 2026-08-07
findings. Running the two implementations against one database found them in an
afternoon. The next revision should start there.

Four smaller things noticed in passing and deliberately not filed, recorded so
they are not re-discovered as novel:

- `MemoryListInput` derives `Default`, which yields `limit: 0` — the
  `#[serde(default = "default_list_limit")]` attribute applies only when
  deserializing. Anything constructing the struct in Rust rather than from JSON
  silently gets a zero limit.
- `remind_me_search` in the MCP dispatch layer always returns JSON and ignores
  `response_format` entirely, unlike the twelve tools fixed in #211. The
  reference honours it there and defaults to Markdown.
- **Correction, same day: there is a dashboard.** The bullet this replaced
  (committed in #233) claimed `rusty-remind-me` has no counterpart to
  `--serve-ui` anywhere in the workspace. Wrong. `crates/remind_me_api`
  serves one: `dashboard/App.jsx` vendored verbatim from the reference,
  `GET /` wired into the same `ROUTES` table as the 25 `/api/*` routes the
  headline table above already counted, live since 2026-07-30 (ADR-0008,
  issue #78). The error was checking only `rusty-remind-me --help`'s
  subcommand list — the stdio CLI binary — and never looking at the separate
  REST API server (`rusty-remind-me api [port]`) the dashboard actually runs
  under. Left standing here as the record of the mistake rather than
  scrubbed, matching this document's own convention of correcting itself in
  place (see the 2026-08-05 corrections above).
- **The remote connector's token and OAuth state file defaults silently
  diverge from the reference's.** `resolve_connector_token`'s default
  (`remote.rs:127`, `default_token_file`) is `~/.remind_me/connector_token`
  (underscored) — the reference persists at `<MEMORY_DIR>/connector_token`,
  which in every real deployment is `~/.remind-me` (hyphenated, ARCHITECTURE.md
  §1 Tenet 3's shared directory). `default_oauth_state_file` has the same
  divergence for `~/.remind_me/oauth.json`. An unmodified switch of
  `--serve-remote` to `rusty-remind-me remote` would not error — it would
  silently mint a fresh connector token and start from an empty OAuth client
  store at the wrong path, orphaning every already-registered claude.ai
  connector. `REMIND_ME_REMOTE_TOKEN_FILE` and `REMIND_ME_REMOTE_OAUTH_STATE_FILE`
  override both and must be pointed at the reference's paths explicitly for a
  real cutover. Found switching a live deployment (2026-08-08) — with 4
  registered OAuth clients and 475 issued access tokens at stake, the
  connector switch itself was deliberately stopped short and left on the
  reference pending this fix, unlike the hub and stdio server, which did move.
