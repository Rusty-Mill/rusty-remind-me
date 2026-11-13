# Rust adherence audit

Audited at `9aff941` (main, 2026-11-12), 0.4.2. Read cold: source, manifests,
CI config and the 207 commits of history in this clone. No prior review, ADR
verdict or expert prompt was used as a source of findings.

Scope: the five workspace crates under `crates/` (about 132k lines of Rust
including tests). `vendor/aws-sigv4` is a patched upstream copy and is out of
scope.

Method: mechanical sweeps over non-test code (everything before a file's
`#[cfg(test)] mod`), then a read of each hot spot the sweeps surfaced, then
commit bodies for the traps. Counts marked "≈" come from line-based scripts
and are approximate; every file:line cited was opened and read.

## Summary

| # | Preference | Verdict |
|---|---|---|
| 1 | `Result` + `?`, no `unwrap`/`expect` | Core adheres. `remind_me_mcp` drifts: 72 `unwrap()` in one function |
| 2 | Types, illegal states, immutability | Mixed. Good at the edges added recently; the central `Memory` row is stringly typed |
| 3 | Async only for real I/O concurrency | Adheres strongly. One manifest leak (`tokio` unconditional in core) |
| 4 | Explicit, flat, short functions | Mostly adheres (94% of functions ≤ 40 lines), with three severe outliers |
| 5 | Composition, no early abstraction | Adheres. One single-implementation trait |
| 6 | Tests and docstrings | Adheres strongly on tests. About 12% of `pub` items lack a doc comment |
| 7 | Never swallow errors | Mostly adheres: discards are commented. A handful are genuinely silent |
| 8 | Minimal deps, secrets, boundary validation | Adheres. One unused dependency, one over-broad one |
| 9 | Explicit config, no globals, modular | Weakest area: 109 env settings read ambiently across 44 files; about 25 process-wide statics |

The three changes that would move the most: split
`McpServer::dispatch_request`, turn on `clippy::unwrap_used` for non-test
code, and give `Memory` typed fields for `status`, `outcome` and
`memory_type`.

## 1. `Result` and `?` over panics

**Adherence**

- `remind_me_core` non-test code has 8 `unwrap()` and 17 `expect()` in about
  60k lines. `panic!`/`unreachable!`/`todo!` appear twice
  (`db/engine/mod.rs:519`, a test-support helper; `promotion.rs:328`).
- Library errors are enums with named variants throughout: `StoreError`
  (`db/store.rs:141`), `KindError` (`kinds.rs:102`), `PromotionError`
  (`promotion.rs:84`), `ImportPathError` (`import_paths.rs:166`), and a dozen
  more.
- The stdio loop catches a panic per request and answers with a JSON-RPC
  error instead of dying (`remind_me_mcp/src/lib.rs:3608-3616`), and relies
  on `parking_lot::Mutex` not poisoning so the store stays usable.
- Lock poisoning is recovered, not unwrapped, at about 45 sites
  (`lock().unwrap_or_else(|e| e.into_inner())`, e.g. `audio_import.rs:285`).

**Drift**

- `remind_me_mcp/src/lib.rs` has 72 `unwrap()` in non-test code, all inside
  `dispatch_request`. 66 are `serde_json::to_string_pretty(..).unwrap()` on a
  reply (first at `:1655`; also `:1792`, `:1826`, `:1850`, `:1885`, `:1945`,
  `:1952`, `:1986`). The same file uses `unwrap_or_default()` for the same
  call at `:95`, so there is no single convention. The panic guard above
  makes these survivable, which is why they have not hurt; it is a net, not
  a licence.
- `scheduler.rs:165`, `:171`, `:175` and `watchdog.rs:416`, `:438`, `:497`,
  `:551`, `:559`, `:563` unwrap or `expect` a `std::sync::Mutex`, against the
  recover-from-poison idiom used everywhere else. One panic while either
  lock is held turns every later lock into a second panic.
- Three thread spawns `expect` (`promotion.rs:811`, `scheduler.rs:325`,
  `watcher.rs:811`). Spawn fails under resource exhaustion; each caller
  already returns a handle type that could carry the `io::Error`.
- Avoidable by construction, not by proof comment:
  `importer.rs:861` (`as_array().unwrap()` after an `is_some_and` check six
  lines up; `if let Some(items)` removes both), `obsidian_import.rs:438`
  (`marker.unwrap()` after `matches!(marker, Some(..))`),
  `vitality.rs:470` (`get_mut(..).unwrap()`; `entry().or_default()`),
  `promotion.rs:328` (`unreachable!("refused above")`; a two-variant enum for
  the promotable rungs removes the arm), and four
  `as_object_mut().expect("just built an object")`
  (`capture.rs:56`, `project_tool.rs:176`, `routes.rs:637`,
  `remind_me_mcp/src/lib.rs:386`) where building a `serde_json::Map` first
  needs no assertion.
- No lint enforces any of this. There is no `[lints]` table in any manifest
  and no `#![deny(..)]`; CI runs `clippy -- -D warnings` with default lints
  only (`.github/workflows/ci.yml:74`). `remind_me_remote/src/handler.rs:206`
  still carries `#[allow(clippy::unwrap_used)]`, a leftover from the monorepo
  where that lint was on. The rule was lost when the repository stood alone
  again (`a2635ab`).

## 2. Ownership and the type system

**Adherence**

- `MemoryKind` is a closed enum with `FromStr`, and per-kind metadata rules
  are checked in one pure module (`kinds.rs:19-84`, `:178`).
- Validated newtypes with private fields: `RepoUrl`, `RepoPath`, `TickListId`
  (`project.rs:137`, `:189`, `:222`).
- Three-state results where a bool would lie: the embedder resolves to
  `Off | Unusable { reason } | Ready` (`embedder.rs:1039`), introduced after a
  blank setting was read as a model change and cleared every stored vector
  (`561ed95`, `040d417`).
- `Store<'a>` carries the process lock as a guard field, so "holding a store"
  and "holding the lock" cannot disagree (`db/store.rs:22-29`).
- Small enums at the API edge: `ResponseFormat`, `SearchDetail`,
  `RetrievalStrategy`, `Rung`, `TagMode` (`models.rs:6`, `:20`, `:31`,
  `:1136`, `:1598`).

**Drift**

- `Memory` (`models.rs:76-176`) is the type everything passes around, and
  its closed-set fields are open strings: `category: String`,
  `memory_type: Option<String>`, `status: Option<String>`,
  `outcome: Option<String>`, `capture_method: String`, plus eight timestamps
  as `String`. The legal values live in doc comments (`:113`, `:165`, `:170`,
  `:174`) and in `&str` arrays (`kinds.rs:87`, `:92`, `:95`, `:98`).
  `MemoryKind` exists but `Memory` does not use it. About 64 non-test lines
  compare against the literals `"done"`, `"abandoned"`, `"active"` or
  `"archived"`.
- 117 non-test signatures return `Result<_, String>` (heaviest:
  `remind_me_cli/src/main.rs` 10, `wiki_llm.rs` 9, `db/engine/copy.rs` 8,
  `surface.rs` 7, `wiki_compile.rs` 7, `embedder.rs` 7). Two error types are
  a `String` in a tuple struct: `EmbedError(pub String)` (`embedder.rs:149`)
  and `HttpError(pub String)` (`sync/http.rs:18`). A caller cannot tell a
  timeout from a refused connection from a malformed body without matching
  text.
- `StoreError::Engine(String)` and `::Legacy(String)` (`db/store.rs:149-152`)
  flatten the engine's error to text, losing the source chain.
  `StoreError::NotFound` displays as SQLite's `"Query returned no rows"`
  because "callers and tests have long matched on" the message
  (`db/store.rs:160-162`); the variant exists, so the message match is debt.
- The MCP and daemon seam is `&str` in, `Option<serde_json::Value>` out
  (`Handler`, `remind_me_mcp/src/lib.rs:3562`; `Service::mcp`,
  `daemon/server.rs:29`). Replies are built by hand: 132 literal
  `"isError": true` objects in `lib.rs`. A typed `ToolReply` with one
  renderer would make a malformed reply unrepresentable.
- About 64 non-test `as` numeric casts against 26 `try_from`/`try_into`
  (e.g. `archive.rs:157`, `raw.len() as i64`).

## 3. Async only where it pays

**Adherence**

- `remind_me_core`, `remind_me_mcp`, `remind_me_api` and the CLI contain no
  `async fn`. Servers are blocking `std::net` with explicit deadlines
  (`remind_me_api/src/lib.rs:172-182`, `webhook.rs:755`); background work is
  named threads with a stop flag and a condvar (`scheduler.rs:160-177`).
- Async is confined to `remind_me_remote`, where an axum-family server and
  the MCP streaming transport need it (`server.rs:203`, `auth.rs:149`).
  Blocking store work there is moved off the reactor
  (`oauth/mod.rs:55-60`, `spawn_blocking`).
- The one async call inside core builds its own current-thread runtime and
  blocks on it, so the synchronous `create_backup` does not depend on its
  caller having a runtime (`cloud_backup.rs:273-281`).

**Drift**

- `tokio = { version = "1.0", features = ["full"] }` is an unconditional
  dependency of `remind_me_core` (`crates/remind_me_core/Cargo.toml`). Its
  only use is `cloud_backup.rs:276`, inside `#[cfg(feature =
  "cloud-backup")]`. A default build compiles the full tokio feature set for
  nothing. It should be `optional = true` under `cloud-backup` with `rt`
  only.

## 4. Explicit, flat, short

Of ≈2,470 non-test functions: 157 exceed 40 lines, 21 exceed 100, 77 nest
deeper than three levels, 16 deeper than four.

**Adherence**

- Guard clauses and `let … else` are the house style in code written since
  October (`api_keys.rs:133-135`, `pid.rs:161`).
- Injected-dependency twins keep the pure part explicit and testable:
  `resolve_db_path()` wraps `resolve_db_path_from(lookup, home)`
  (`db/mod.rs:88-93`); `poll_once_with`, `serve_once_from`,
  `check_for_update_at` follow the same shape.
- Comments state why, with the incident that proved it
  (`db/queries.rs:130-136`, `archive.rs:159-163`).

**Drift**

- `McpServer::dispatch_request` (`remind_me_mcp/src/lib.rs:615`) is ≈2,940
  lines and nests 12 deep. It holds the JSON schema for every tool inline,
  then every tool's body as a match arm. It is 42% of the crate's largest
  file (6,947 lines) and the reason for `#![recursion_limit = "512"]`
  (`lib.rs:9`).
- `main` in `remind_me_cli/src/main.rs:591` is ≈514 lines, 6 deep.
- `import_content` (`importer.rs:1107`) is ≈439 lines, 5 deep.
- Next tier, each over 130 lines: `search_memories_deadlined`
  (`db/queries.rs:678`, ≈218), `rank_rrf` (`retrieval.rs:363`, ≈188),
  `pull_mempalace` (`mempalace_import.rs:278`, ≈163), `decompose`
  (`capture.rs:310`, ≈142), `pull_dbs` (`dbs_import.rs:339`, ≈139),
  `parse_chat` (`importer.rs:842`, ≈138).
- Small but deep: `get_memories` (`remind_me_mcp/src/lib.rs:65`, 6 levels in
  32 lines; a helper returning `Result` and one error renderer flattens it),
  `check_auth` (`remind_me_api/src/lib.rs:320`, 5 levels),
  `parse_configure_args` (`remind_me_cli/src/main.rs:87`, 6 levels).
- Implicit behaviour: a request with no `id` is answered as id `1` at eight
  sites (`remind_me_mcp/src/lib.rs:622`, `:669`, `:677`). JSON-RPC defines a
  request without an id as a notification that gets no reply.
- `serverInfo.version` is the literal `"0.1.0"`
  (`remind_me_mcp/src/lib.rs:661`); the crate is 0.4.2.
  `daemon/wire.rs:115` uses `env!("CARGO_PKG_VERSION")` for the same purpose.

## 5. Composition, no early abstraction

**Adherence**

- Five traits in the workspace, no trait hierarchy, no generic framework.
  `Embedder` (`embedder.rs:163`) has three production implementations;
  `WikiWriter` (`wiki_compile.rs:67`) one plus six test doubles; `Service`
  (`daemon/server.rs:27`) one plus three; `Handler`
  (`remind_me_mcp/src/lib.rs:3562`) two.
- Seams are plain function parameters where a trait would be ceremony:
  `&dyn Fn(&Memory) -> bool` (`vectors.rs:252`), an injected compiler
  closure for the dashboard endpoint (`auto_compile.rs:27`).
- Shared code is extracted on the second use, and history shows it being
  done: `legacy_upgrade` after a second copy of fold/apply/publish landed
  (`f4962b3`); `edit_distance` moved to `text` when a second caller appeared
  (`9b327d5`).

**Drift**

- `Notifier` (`notifications.rs:65`) has one implementation,
  `WebhookNotifier` (`:87`), and no test double. `configured_notifiers()`
  returns `Vec<Box<dyn Notifier>>` (`:114`) holding at most that one.
- Duplication a second call site should already have collapsed:
  - Two hand-rolled HTTP request readers with the same `ContentLength` enum
    and `read_head` (`remind_me_api/src/http.rs:124`, `:155`;
    `webhook.rs:233`, `:266`), with limits that have drifted (8 MiB vs
    10 MiB body, `http.rs:22` vs `webhook.rs:87`).
  - Four hand-rolled HTTP clients over `TcpStream`
    (`embedder.rs:339`, `query_expansion.rs:111`, `sync/http.rs:68`,
    `telemetry.rs:300`). Chunked decoding was fixed in one (`9daba9f`,
    `c881940`).
  - Four copies of the path-keyed model cache
    (`audio_import.rs:276-290`, `image_import.rs:140-154`,
    `reranker.rs:271-285`, `embedder.rs:799-811`).

## 6. Tests and docstrings

**Adherence**

- 2,387 `#[test]` functions: 1,949 in core, 226 api, 144 mcp, 58 cli, 10
  remote plus 36 `#[tokio::test]`. No `#[ignore]`. No core module was found without inline tests or
  a file under `tests/` that names it.
- Failure and boundary cases are the norm, and named as sentences:
  crash-safety (failed publication then retry, torn temp blob; `e210719`),
  DST gaps and folds (`0f20c6a`, `be3235a`), races driven by a barrier
  rather than a sleep (`d3751da`), redaction overlap (`9ef2ec3`).
- Bug-fix commits state that the new test fails on the previous code
  (`e210719`, `9172736`, `411981c`, `38c9933`).
- Search quality has a golden-set eval gated in CI (`d9c4672`).
- CI runs each optional feature on its own runner, a combined-features job,
  and Windows (`.github/workflows/ci.yml:79-170`).

**Drift**

- ≈237 of ≈1,960 `pub` items have no doc comment. Concentrated in
  `models.rs` (26; `Memory` at `:76` and its leading fields are among them), `routes.rs` (14), `render.rs` (14), `rate_limit.rs` (12),
  `embedder.rs` (11), `project.rs` (10). `missing_docs` is not enabled.
- `remind_me_core/src/lib.rs` has no crate-level `//!` doc, and exports 88
  modules as `pub` with three glob re-exports (`:98-101`). The public
  surface is everything, so "docstrings on the public surface" has no
  boundary to hold to.
- Feature matrix gaps are deliberate but real: `ocr`, `audio` and
  `local-embed` are clippy-only, `ann` and `rerank` are test-only
  (`ci.yml:94-103`).

## 7. Never silently swallow errors

**Adherence**

- Of 55 non-test `let _ =`, the ones that discard a real failure carry a
  comment saying why and what the backstop is: the embed after a successful
  write (`db/queries.rs:130-136`), retention prune after an archive
  (`archive.rs:159-163`), embedding-meta bookkeeping (`vectors.rs:134-138`).
  The rest are socket shutdowns, thread joins and `writeln!` into a `String`.
- Degraded states are reported instead of hidden: an unparseable key file is
  logged and treated as empty, failing closed (`api_keys.rs:137-145`);
  `session_files` returns `failed` beside the folders it could list
  (`d3751da`); an unusable embedder names the missing variable in status
  (`561ed95`).
- History shows the rule being enforced in review: a discarded join result
  let a test pass without its stub being probed (`435b92d`).

**Drift**

- `remind_me_mcp/src/lib.rs:616`: `serde_json::from_str(request_json).ok()?`.
  A malformed line gets no reply and no log. JSON-RPC wants `-32700`; nothing
  in the crate emits it.
- `remind_me_mcp/src/lib.rs:1950`:
  `references::search_json(..).unwrap_or_else(|_| json!(res))`. A store
  error while attaching references becomes a successful reply without them.
- `remind_me_api/src/lib.rs:180`: `let _ = self.serve_one(&mut stream);`,
  uncommented. Every I/O failure serving a dashboard request vanishes. Same
  shape at `webhook.rs:786`.
- `remind_me_api/src/routes.rs:106`: `let _ = capture_snapshot(store);`. The
  comment explains why the capture happens on read, not why its failure is
  ignored; a failing snapshot yields a silently empty trend.
- Embedding failures after a write are dropped at five sites
  (`db/queries.rs:135`, `:293`, `history.rs:194`, `transcript.rs:642`,
  `consolidation.rs:543`). The reasoning is sound, but nothing is counted or
  logged, so "why is semantic search missing this memory" has no trace.
  `metrics.rs` already has counters.
- Socket timeouts are set with `.ok()` or `let _` at six sites
  (`pid.rs:164-165`, `query_expansion.rs:113-114`, `sync/http.rs:70-71`,
  `telemetry.rs:302-303`, `remind_me_api/src/lib.rs:178-179`,
  `webhook.rs:775-776`). If the call fails the read is unbounded, which is
  the hang the timeout exists to prevent. The wiki client bounds the whole
  call instead (`fc5acf9`).
- Serialisation failures become `{}` at `project_tool.rs:216` and
  `webhook.rs:584`, `:591`, `:598`.
- Diagnostics are 72 bare `eprintln!` across 20 files, with no levels and no
  common prefix. Acceptable for a stdio MCP server (stdout is the protocol),
  but it is a convention nobody wrote down.

## 8. Dependencies, secrets, boundary validation

**Adherence**

- Heavy trees are optional features, each with a comment giving the cost and
  the reason (`crates/remind_me_core/Cargo.toml`: `pdf` 71 crates,
  `cloud-backup` 233, `ann` a five-minute C++ build). `default = []`.
- Version pins say why (`rusqlite` 0.41, `rten` 0.24, `aws-sdk-s3`
  `default-features = false`).
- `cargo-deny` is a blocking CI job; every advisory ignore carries a reason;
  git sources are limited to one repository (`deny.toml`).
- No secrets in the tree. The only key-shaped strings are AWS's published
  example key used as a redaction fixture (`project.rs:1488`).
- Secrets are compared as digests in constant time
  (`remind_me_api/src/lib.rs:323-324`); cloud backup refuses a plaintext
  upload without an explicit opt-in and defines no credential variables of
  its own (`cloud_backup.rs:3-27`); outbound capture is scrubbed before it
  leaves (`redact.rs`, `c61cff4`, `0f8e1dc`).
- Boundaries are validated before work starts: head and body caps
  (`http.rs:22-24`), path validators for every importer and exporter
  (`import_paths.rs:198-242`, `export.rs:178`), per-field bounds as named
  constants (`models.rs:62-63`, `:532-533`), typed deserialisation of tool
  input (41 `serde_json::from_value` in `remind_me_mcp/src/lib.rs`).
- Unauthenticated writes are refused by default
  (`remind_me_api/src/lib.rs:359-367`).

**Drift**

- `rusty_percent` is declared in `crates/remind_me_core/Cargo.toml` and
  referenced nowhere in that crate. `cargo-shear` found two like it before
  (`b684ec9`); it is not in CI.
- `tokio` with `features = ["full"]` in core (see §3).
- `sha256` is a whole crate used for `digest(..)[..16]` fingerprints
  (`importer.rs:86`, `archive.rs:192`, `daemon/wire.rs:115`) in a workspace
  that already has `rusty_crypto_key` and `rusty_hex`. Worth a check, not a
  finding.
- `deny.toml` allows duplicate versions and wildcards
  (`multiple-versions = "allow"`, `wildcards = "allow"`), with a "revisit"
  note. The lockfile holds 490 packages.
- Every manifest's `repository` still points at `Rusty-Mill/rusty_mill`.
- `deny_unknown_fields` appears once in the workspace, so a misspelt argument
  (`tag` for `tags`) is accepted and ignored.

## 9. Explicit configuration, no globals, modular layout

**Adherence**

- Crate boundaries are clean and one-directional: `api` and `mcp` depend
  on `core`, `remote` on `mcp`, and the CLI on all four. Core has no HTTP framework; the remote
  crate owns tokio and axum (`remote.rs:8-12` states the rule).
- Every setting is a named `pub const …_ENV` beside the code that reads it,
  with a doc comment.
- Where config is gathered it is a struct built once: `WebhookConfig`
  (`webhook.rs:113`), `GateConfig` (`remind_me_remote/src/auth.rs:86`),
  `RrfConfig` (`retrieval.rs:146`), `AutoOptions` (`wiki_compile.rs:73`).
- The daemon fingerprints the settings that matter and refuses a client
  whose environment differs (`daemon/settings.rs`).

**Drift**

- 109 distinct `REMIND_ME_*` variables are read by 83 `env::var` calls in 44
  non-test files, most at the moment of use deep in a call path
  (`embedder.rs:1018-1118`, `retrieval.rs:41`, `:135`, `:655`,
  `query_expansion.rs`, `transcript.rs:99`). There is no `Config` value;
  behaviour depends on ambient process state at call time. The cost is
  visible in the tests: 386 calls into a `test_env` shim, per-file env
  locks, a `clippy.toml` ban on `std::env::set_var`, and at least four
  commits fixing env races or leaks between tests (`df47869`, `e1d0c83`,
  `1f21e3a`, `e08c48c`).
- About 25 process-wide mutable statics outside tests, among them
  `HANDSHAKE_CLIENT` (`sync/mod.rs:83`), `GLOBAL` watchdog
  (`watchdog.rs:396`), `SHARED` rate limiter (`rate_limit.rs:202`),
  `EXPANSION_CACHE` (`query_expansion.rs:174`), `AVAILABILITY`
  (`embedder.rs:981`), `COUNTERS` (`metrics.rs:65`), `DEFAULT_WRITER`
  (`context.rs:278`), `RUNNING` (`wiki_compile.rs:39`), `LAST_NUDGED_TOTAL`
  (`promotion.rs:691`), and three `LIVE` thread registries
  (`scheduler.rs:245`, `promotion.rs:734`, `watcher.rs:731`). Several are
  defensible as one-per-process (metrics, watchdog). `HANDSHAKE_CLIENT` is
  not: caller identity is per connection, and it is already patched with a
  thread-local override (`sync/mod.rs:103-106`) for the remote path.
- `remind_me_core` is 104k lines in 88 public modules: domain rules, the
  engine adapter, two HTTP servers, four HTTP clients, a daemon, a
  self-updater, a watchdog, OCR, speech-to-text. Domain logic is not free of
  I/O. The pure modules exist and say so (`kinds.rs`, `date_phrase.rs`,
  `time_window.rs`), but nothing separates them structurally from
  `webhook.rs` or `updater.rs`.
- Names that no longer match contents: the `sync` module holds write
  provenance and the shared HTTP client after sync was removed
  (`sync/mod.rs:1-14`, `1c89d71`); `core/src/remote.rs` holds remote
  *configuration*, the server lives in the `remind_me_remote` crate.
- Mixed lock types: `parking_lot` in 4 files, `std::sync::Mutex` in about 24.
  The choice is load-bearing (the panic guard in §1 depends on the database
  lock not poisoning) and is stated only in one doc comment.

## Candidate rules

Recurring patterns, traps and judgment calls found in the code and history
that the nine preferences do not cover.

**C1. Check, then write, under the same handle.** The most repeated bug
class in the last 60 commits. A value read under one store handle was acted
on under another, and something changed in between: wiki compile marking a
batch whose source was edited (`3b381ea`), a page overwritten while the
model wrote (`9e51adb`), a citation dropped between last write and mark
(`83a1201`), a lease that was a read then a separate write (`ab06c4a`),
watcher and hook captures interleaving (`d3751da`).

**C2. Never hold the store across a slow call.** A `Store` from
`Database::store()` holds the process lock for its lifetime
(`db/store.rs:9-14`). The first auto-compile held one across every model
call and stalled every other tool for minutes (`9e51adb`). Take a short
handle per step and re-validate (C1).

**C3. Publish, then delete.** Two schema upgrades cleared the old log before
the new blob was durable and lost data on a crash (`e210719`, `9172736`).
The order is: read, write temp, fsync, rename, fsync the directory, only
then remove the source; every step resumable.

**C4. Unset, blank and malformed are three states.** A blank or unparseable
setting was read as a different valid value and cleared every stored vector
(`561ed95`, `040d417`). A setting that feeds an identity or a destructive
comparison must resolve to off / unusable-with-reason / ready, never to a
default.

**C5. A deadline bounds the whole call.** Per-read idle timeouts let a
trickling peer hold a call open forever (`9e51adb`, `fc5acf9`), and DNS
cannot be interrupted in `std`, so it runs on a helper thread the caller
abandons (`83a1201`).

**C6. A cache key must cover everything the verdict depends on.** The
availability cache was keyed on time alone, then nearly on an identity that
left out the endpoint (`e1d0c83`).

**C7. Write all fields before the write that stamps `updated_at`.** That
stamp is what queues the row; a later same-timestamp update is dropped
(`411981c`).

**C8. Opaque tokens for continuation, validated on return.** Watermarks
became `(created_at, id)` pairs, JSON-encoded, checked against the furthest
one ever issued, after timestamp-only marks integrated sources the caller
never saw (`45c4b6c`, `2b438f0`).

**C9. A floor or filter needs an exemption list decided up front.** Default
search hid decisions after about 150 days because the dormancy floor applied
to every kind (`4921c3c`).

**C10. Strings bound for a terminal or a model are escaped at the edge.**
Stored names reached the terminal verbatim and could drive it (`81e3ee4`).

**C11. Env in tests goes through `test_env`, under one lock per variable.**
Two module-local locks over one variable serialise nothing (`df47869`);
`setenv` racing C `getenv` in SQLite's initialiser segfaulted the test
binary about once in 400 runs (`test_env.rs:8-41`).

**C12. Assert that the stub was reached.** A discarded join result let a
cached verdict pass a test whose stub was never probed (`435b92d`).

**C13. Protocol version echo is a promise.** The server echoed any requested
`protocolVersion` and then failed that revision's required field
(`593f793`).

**C14. A parser for free text infers nothing on doubt.** "since March 2025"
matched "since March" and dropped the year (`6f9722b`); a leading sign or
decimal point was discarded before reading a count (`5822679`).

## Draft Rust skill pack

Only what a capable model would not know without having read this
repository. Each item is a convention, a trap with its evidence, or a
judgment call.

```markdown
# rusty-remind-me: Rust working notes

## Shape
- Sync by default. Core, mcp, api and the CLI are blocking std::net plus
  named threads with a stop flag and condvar. Async lives only in
  remind_me_remote. If core needs one async call, build a current-thread
  runtime inside that function and block on it (cloud_backup.rs:273).
- Dependency direction: api and mcp depend on core, remote on mcp, the CLI
  on all four. Core must not gain an HTTP framework or a tokio-dependent
  type.
- Heavy or native dependencies are optional features with default = [], and
  the manifest comment states the crate count or build cost and the reason.
  Feature-off, the capability reports "unavailable"; it does not fail.
- No implicit downloads. Model-backed features take explicit file paths.

## Store
- `Database::store()` returns a `Store` that holds the process lock until it
  drops. Never keep one across a network call, a model call or a sleep.
  Open a short handle per step. (9e51adb: minutes-long stall of every tool.)
- Background threads use `SecondarySource::store()`, which takes no process
  lock.
- Anything decided from a read must be re-read under the handle that writes.
  If it changed, stop and leave the work pending; do not overwrite.
  (3b381ea, 83a1201, ab06c4a, d3751da.)
- Multi-row writes go through `store.transaction(..)`.
- Set every field before the write that stamps `updated_at`; that stamp
  queues the row and a second same-timestamp write is dropped. (411981c.)
- The db lock is `parking_lot::Mutex` on purpose: it does not poison, which
  is what lets the stdio loop catch a panicking tool call and keep serving
  (remind_me_mcp lib.rs:3595). Do not swap it for std.
- For `std::sync::Mutex`, the idiom is
  `lock().unwrap_or_else(|e| e.into_inner())`, never `unwrap()`.

## On-disk upgrades
- Order is read, write temp, fsync, rename, fsync dir, then delete the old
  form. Each step must be resumable from any earlier state. Use
  `db::engine::legacy_upgrade`; do not write a second copy. (e210719,
  9172736, f4962b3.)
- An upgrade PR carries three tests: failed publication then retry,
  interruption after publication, torn temp file.
- Record layouts are versioned (`@1`, `@2`). A new field means a new layout
  and a reader for the old one. Upgrades are one-way; say so in the PR.

## Settings
- Each setting is a `pub const NAME_ENV: &str` with a doc comment, next to
  its reader. Empty string means unset.
- Read through an injectable lookup: `foo()` calls
  `foo_from(|k| std::env::var(k).ok(), ..)`. Test the `_from` form.
- A setting that feeds an identity or a destructive check resolves to
  Off / Unusable { reason } / Ready. Blank and unparseable are Unusable and
  name the variable. Never fall back to a default there. (561ed95, 040d417:
  every stored vector cleared.)
- A cache of a verdict is keyed on a fingerprint of every setting the
  verdict depends on, endpoint included, not on time or model identity
  alone. (e1d0c83.)
- A new setting the daemon should care about goes in
  `daemon/settings.rs`'s fingerprint. A per-client secret is carried but
  excluded from the difference. (02dbcac.)

## Tests
- Never call `std::env::set_var` or `remove_var`; clippy.toml bans them.
  Use `test_env::set_var` / `remove_var`. Other crates include the file with
  `#[path]`.
- One lock per variable, shared by every module that touches it. A second
  module-local lock is a bug. (df47869.)
- A guard that changes env restores the inherited value on drop. (1f21e3a.)
- Tests that must not start model work wrap themselves in the model-stages
  guard; an inherited REMIND_ME_* value on a developer machine will
  otherwise start it. (38c9933, e08c48c.)
- Race tests use a barrier at the window, not a sleep, and must fail
  without the fix. State "fails on main" in the commit. (d3751da.)
- A stub server test asserts the stub was hit. Do not discard its join
  result. (435b92d.)
- To hold a port closed, bind a socket and never listen. A connected
  client's port is not exclusive on Linux. (8558aa9.)
- Assert the bound on a whole call, not on the phase that trips it: Windows
  buffers uploads, so the read times out where Linux times out the write.
  (83a1201.)
- Each test owns its scratch directory. (45c4b6c.)
- Tool counts are asserted. Adding or removing an MCP tool means updating
  them and the surface list (surface.rs), in the same PR. (1e7db4a.)

## Network code
- HTTP clients and servers here are hand-rolled over TcpStream. Before
  writing a fifth client, reuse `sync::http`.
- The deadline starts before name resolution and covers connect, every
  write and every read. An idle timeout is not a deadline. DNS runs on a
  helper thread that is abandoned at the deadline; an IP literal skips it.
  (fc5acf9, 83a1201.)
- Decode chunked bodies and require the terminator after the trailer
  section. (9daba9f, c881940.)
- Refuse non-finite and non-numeric vector elements at the boundary; sort
  similarities with `total_cmp`. (a78a0dc.)
- One connection at a time with a ten-second write deadline: work that
  takes longer answers 202 from its own thread and is polled. (ffeff0d.)

## MCP surface
- stdout is the protocol. Diagnostics go to stderr with `eprintln!`.
- Every reply passes through `handle_request`; cross-cutting reply fields
  are stamped there, not per arm. (593f793.)
- Replies default to JSON; `response_format` must be honoured on every path
  (a parsed-then-ignored value returned JSON for a markdown request, #224).
- Tool input is deserialised into a typed struct from `models.rs`, with
  bounds as `*_MIN` / `*_MAX` constants beside the struct.
- A wrong-typed argument is refused. Do not fall back to recomputing.
  (2b438f0.)
- A call forwarded by the remote connector carries caller meta and is
  stamped with no project, cwd, git or session context. (bad1e5f.)
- A tool named like a lookup must not write. (`remind_me_entity` once
  upserted; models.rs:41-47.)

## Judgment calls
- Best-effort side work after a successful write (embedding, retention,
  snapshot bookkeeping) may be discarded with `let _ =`, but only with a
  comment naming the backstop. Prefer counting it.
- Continuation tokens are opaque, JSON-encoded, and validated against the
  furthest one issued. A timestamp alone is not a position. (45c4b6c,
  2b438f0.)
- Truncation and watermarks must cut at the same place. If a brief omits a
  source, the mark must not pass it. (72f6ebc.)
- Automatic rewrites of user content are guarded on what must survive
  (length, headings, links, code spans), refused once with the reason, then
  stopped. (f4d36ba: first live run lost three facts at 90% length.)
- A default filter that hides rows needs its exemptions decided when it is
  added. Decisions and preferences never fall below the dormancy floor.
  (4921c3c.)
- Free-text inference is off by default until measured, and infers nothing
  when the phrase is ambiguous (a trailing year, a sign, a decimal).
  (23bfdd0, 6f9722b, 5822679.)
- Calendar arithmetic is done on naive local time and resolved explicitly:
  a repeated time takes the earlier instant, a skipped time the first
  instant after the jump. Hours are elapsed time. (0f20c6a, be3235a.)
- Text bound for a terminal has control characters escaped at display only.
  (81e3ee4.)
- Redaction coalesces overlapping matches before replacing, and scans must
  stay linear on repeated anchors. (9ef2ec3, eb6b522, 3045aa9.)
- In-process exclusion is enough for anything one process owns; a stored
  lease that is read then written is not atomic. (ab06c4a.)
- Removing a subsystem: check what periodic work it was also doing.
  Removing the sync worker removed the only outbox prune. (51db983,
  4831c1c.)
- `links = "sqlite3"` allows one rusqlite per graph; an advisory fix can
  force a rusqlite major bump. (1af4885.)
- `[patch.crates-io]` does not carry through git dependencies; patches are
  copied into this workspace. (a2635ab.)
- `rten` must match the major that `ocrs` links, or the build fails with
  "expected Model, found Model".

## Known debt, do not copy
- `dispatch_request` (mcp lib.rs:615) is one 2,900-line match. New tools go
  in their own function returning a Result, called from a one-line arm.
- `serde_json::to_string_pretty(..).unwrap()` in that function. New code
  propagates.
- `Result<_, String>` and `EmbedError(String)`. New fallible code gets an
  enum.
- `Memory`'s `status`, `outcome`, `memory_type` are strings. Parse to
  `MemoryKind` or the `kinds` constants at the boundary; do not compare
  literals inline.
- `id.unwrap_or(json!(1))`. A request without an id is a notification.
```
