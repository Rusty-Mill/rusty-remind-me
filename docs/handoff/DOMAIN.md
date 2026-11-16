---
name: rusty-remind-me
kind: domain
version: 0.1.0-draft
harvested: 2026-11-16 @ 94699a6
expert: rusty-remind-me-expert
pack_root: docs/handoff/
---

# rusty-remind-me: domain pack

Loaded into every session on this project. It is a digest: ask
`rusty-remind-me-expert` for anything not here.
Tags: [stated] ask before changing. [default] may be revised through the expert.

## Intent
One persistent memory that every model, agent and bot shares, so all of them work
from the same knowledge base [stated] (D1). One remote MCP server holds one store [default] (ADR-0032).

## Scope
- In: store and search, vitality, entities, projects, decisions, captures, wiki, imports; MCP (stdio, remote), REST API, dashboard.
- Out: multi-node sync and the hub (ADR-0032); schema parity with Python `remind_me` (ADR-0023); Mill Board, rusty_tick; programme order (`millwright-expert`).

## Hard lines
- No store change before the storage port and the R3 benchmark pick the store [stated] (D2).
- Ask before schema or data migrations, public API changes, deletions, dependency or toolchain bumps [stated] (D3).

## Shape
`remind_me_core` (domain and store, blocking) ← `remind_me_api` (hand-rolled HTTP) and `remind_me_mcp` (stdio);
`remind_me_remote` (the only async crate) → mcp; `remind_me_cli` → all four. Store: `rusty_multimodal_db` engine
in-process, one port trait per table group with an engine adapter and a conformance suite (ADR-0035).

## Traps and conventions
- `Database::store()` holds the process lock until dropped: never keep a `Store` across a model or network call or a sleep; take a short handle per step (db/store.rs:9-14; 9e51adb).
- Background threads use `SecondarySource::store()`, which takes no process lock (db/store.rs:110).
- Re-read under the handle that writes; if the source changed, stop and leave the work pending (3b381ea, 83a1201, ab06c4a, d3751da).
- The db lock is `parking_lot::Mutex` because it does not poison: the stdio loop catches a panicking tool and keeps serving (db/mod.rs:37; remind_me_mcp lib.rs:3614).
- Records are bincode-positional: a new field is a new layout (`@1`, `@2`) and a one-way upgrade through `db::engine::legacy_upgrade` (fold, publish durably, then drop the old log). Ask first (D3); prefer `metadata` JSON (revisions_v1.rs; e210719, 9172736).
- An upgrade PR carries the three tests in revisions_v1.rs:187-243: failed publish then retry, interrupted after publish, torn temp blob.
- A setting that feeds an identity or a destructive check resolves to Off / Unusable { reason } / Ready; blank and unparseable are Unusable, never a default (embedder.rs:1036; 561ed95).
- A verdict cache is keyed on every setting it depends on, endpoint included (e1d0c83); a setting the daemon cares about joins the fingerprint in daemon/settings.rs (02dbcac).
- Tests never call `std::env::set_var`/`remove_var` (clippy.toml bans them): use `test_env::set_var`, and the variable's one shared `ENV_LOCK` across modules (test_env.rs:18-37; df47869).
- Adding or removing an MCP tool updates the exact lists in remind_me_mcp tests/tool_profiles_test.rs and surface.rs in the same PR.
- Cross-cutting reply fields are stamped once in `handle_request`, not per arm (remind_me_mcp lib.rs:604; 593f793).
- `dispatch_request` is a ~2,900-line match (lib.rs:615): a new tool is its own function returning `Result`, called from a one-line arm. Don't copy its `to_string_pretty(..).unwrap()` or `id.unwrap_or(json!(1))`.
- Tool input is a typed struct in models.rs with `*_MIN`/`*_MAX` bounds beside it; a wrong-typed argument is refused, not recomputed (2b438f0); `response_format` holds on every path (#224).
- A tool named like a lookup never writes: `remind_me_entity` reads, `remind_me_entity_upsert` writes (models.rs:39-47).
- A call forwarded by the remote connector gets no cwd, git or session context; only an explicit `project` (tests/forwarded_context_test.rs; bad1e5f).
- `Memory.status`, `outcome`, `memory_type` are strings: parse to `kinds::MemoryKind` at the boundary, no inline literal compares (models.rs:117-171).
- `Result<_, String>` and `EmbedError(String)` are debt; new fallible code gets an enum (embedder.rs:149).
- Model and embedding calls run under one deadline that starts before DNS (resolved on an abandoned helper thread) and covers connect, writes and reads (embedder.rs:385; fc5acf9, 83a1201). The webhook client `sync::http` has only per-I/O timeouts.
- The API serves one connection at a time: work longer than its write timeout answers 202 from its own thread and is polled (remind_me_api lib.rs:170, 284; ffeff0d).
- Wiki compile watermarks are `(created_at, id)` pairs validated against the furthest issued; truncation and the mark cut at the same place (wiki_fs.rs:40; 45c4b6c, 72f6ebc).
- Automatic rewrites of user content are guarded on length, headings, links and code spans, refused once with the reason, then stopped (f4d36ba).
- A default filter that hides rows decides its exemptions up front: decisions and preferences never fall below the dormancy floor (vitality.rs:382; 4921c3c).
- Removing a subsystem: check its periodic work. Removing the sync worker removed the only outbox prune (51db983).
- `[patch.crates-io]` does not carry through git dependencies, so patches are vendored here (Cargo.toml:32; a2635ab). `rten` must match the major `ocrs` links (remind_me_core Cargo.toml:51-55).

## Ask
Design, scope or behaviour questions go to `rusty-remind-me-expert` before Nano.
