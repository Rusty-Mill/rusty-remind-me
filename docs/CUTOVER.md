# Client Cutover Runbook

**What this is.** A runbook plus a dated record of the first real one: migrating
every process on a machine that talks to `remind_me`/`rusty_remind_me` from the
Python reference to this port, without downtime and without losing sync state.
**What this is not.** An architecture decision about the Rust codebase — see
`docs/adr/` for those. This document is operational: it describes *consumers*
(other people's config files, other apps' supervisors) that live outside this
repo, and is host-specific by nature. Keep it current anyway; it is the only
place this information is written down at all (see Lesson 1).

---

## Why this exists

The 2026-08-08 production switch recorded in `gap-analysis.md` and
`RELEASE_NOTES.md` replaced the **server-side** infrastructure — the sync hub,
the dashboard/API, the `claude.ai` remote connector — with `rusty_remind_me`.
That switch and the **client** cutover (every stdio MCP process that talks to
that infrastructure) are two separate migrations. The server-side switch was
declared complete; three independent client processes were still running the
Python reference for the next several hours, undetected, because nothing
checks client configs as part of "the switch." Conflating the two is the
mistake this document exists to prevent from repeating.

---

## Feature parity gaps (found scoping a cutover on `home-pc-win`, 2026-08-08)

This host's topology differs from the inventory below: one shared process
(Windows Task Scheduler task `remind-me-mcp-http`, launching
`~/.remind-me/serve-mcp.ps1`) serves the MCP HTTP endpoint
(`127.0.0.1:8767`), the peer-sync receiver (`0.0.0.0:8766`), and the hub
push/pull worker all at once — every local client (Claude Desktop via
`mcp-remote`, Claude Code natively, this harness's own `remind-me-win` MCP
entry) points at that one fixed URL, so a cutover here needs zero client
config edits, only a backend-process swap. A second Task Scheduler process
(`python -m remind_me_mcp --serve-ui --ui-port 5199`) is the dashboard
sidecar, riding the first process's Windows Job object.

`rusty-remind-me remote` (port-overridden to 8767) already replicates the
combined shape: `McpServer::new` wires up `SyncPeer` (the port-8766
receiver, gated on `REMIND_ME_SYNC_SECRET`) and `SyncWorker` (the hub
push/pull loop, gated on node_id + hub_url + secret) unconditionally
(`crates/remind_me_mcp/src/lib.rs:351-357`) — no separate "sync" subcommand
needed.

The cutover was **deliberately deferred** on this host: `serve-mcp.ps1` also
sets `REMIND_ME_RERANK=onnx` and `REMIND_ME_QUERY_EXPANSION=hyde`, and the
Rust port's support for those was not at parity.

1. **Reranking — now closed.** `remind_me_core::reranker` supports it, but
   only behind `cargo build --features remind_me_core/rerank` (not in a
   default build) and only against a `.rten`-format model — the reference's
   HuggingFace cache
   (`~/.remind-me/models/models--cross-encoder--ms-marco-MiniLM-L6-v2`) is
   ONNX/safetensors, not `.rten`. Converted directly from the cached ONNX
   export, no re-download and no PyTorch needed:
   ```bash
   pip install --target <short-path-dir> rten-convert   # onnx export already in the HF cache
   python -m rten_convert <cache>/onnx/model.onnx out.rten --no-infer-shapes
   ```
   `--no-infer-shapes` works around a Windows-only bug in `rten-convert`
   0.22.0: `onnx.shape_inference.infer_shapes_path` writes through a
   `tempfile.NamedTemporaryFile` handle that is still open when its own C
   extension tries to reopen the same path, so the default (shape-inference
   on) path always raises `PermissionError` on Windows. Skipping inference
   costs a runtime graph optimization, not correctness.
   Verified against real weights (`cargo run -p remind_me_core --example
   verify_rerank --features rerank`, deleted after use): a deliberately
   RRF-inverted 4-candidate list was correctly reordered, on-topic
   candidates first, with sane cross-encoder logits (`status() == Ready`).
   Converted model kept at `~/.remind-me/rerank/ms-marco-MiniLM-L6-v2.rten`
   + `~/.remind-me/rerank/tokenizer.json`; point
   `REMIND_ME_RERANK_MODEL_PATH`/`REMIND_ME_RERANK_TOKENIZER_PATH` at those
   and build with `--features remind_me_core/rerank` (or add a
   `rerank = ["remind_me_core/rerank"]` passthrough to
   `remind_me_cli/Cargo.toml` so plain `--features rerank` works from the
   `rusty-remind-me` package without the `package/feature` qualifier).

2. **Query expansion (HyDE) — now closed.** Implemented from scratch —
   `remind_me_core::query_expansion`, ported from the reference's
   `query_expansion.py`: `REMIND_ME_QUERY_EXPANSION=hyde` sends the query to
   a local Ollama daemon's `/api/generate` (`REMIND_ME_HYDE_MODEL`, default
   `llama3.2`; `REMIND_ME_HYDE_TIMEOUT`, default 15s) for a short
   hypothetical passage, capped at 600 chars, cached per query (bounded
   LRU). Any failure — daemon down, model missing, timeout, empty response —
   silently falls back to the plain query, never an error. The expansion
   text is fused into the search vector by
   `remind_me_core::vectors::fuse_query_embedding` (query text embedded with
   `EmbedRole::Query`, expansion text with `EmbedRole::Passage`, mean then
   L2-renormalised — the reference's own `db._fuse_query_embedding`), wired
   into `search_memories_budgeted` ahead of `semantic_search_scored`.
   One deliberate simplification: the reference coalesces concurrent
   callers racing the same uncached query behind a `threading.Event`, so two
   threads never both pay a redundant generation; this port omits that
   (cache-only, no coalescing) since it changes resource cost, not any
   caller-visible result — see the module's own doc comment if that ever
   needs revisiting. Ollama was not running on this host to verify against
   a real LLM, so the HTTP client contract (request shape, response
   parsing, timeout/failure fallback, cache hit avoiding a second request)
   is covered by 11 tests against a fake `/api/generate` server
   (`tests/query_expansion_test.rs`), the same pattern
   `ollama_embedder_test.rs` already established for `OllamaEmbedder`. The
   fusion math and its effect on real rankings is covered separately
   (`tests/vectors_test.rs`): a fake embedder proves an expansion text can
   flip which of two candidates wins over the plain query alone.

3. **Embeddings — now closed.** Added `remind_me_core::embedder::OnnxEmbedder`
   (`REMIND_ME_EMBEDDING_BACKEND=onnx`), an in-process ONNX bi-encoder via
   `rten` — the same pure-Rust runtime `reranker`/`ocr` already use, behind
   a new `local-embed` feature — mean-pooled over non-padding tokens and
   L2-normalised, matching the reference's `_Embedder._embed_forward`
   exactly. Takes explicit `REMIND_ME_ONNX_MODEL_PATH`/
   `REMIND_ME_ONNX_TOKENIZER_PATH` (`.rten` + `tokenizer.json`), same
   no-implicit-download convention as `rerank`. Converted the reference's
   already-cached `sentence-transformers/all-MiniLM-L6-v2` ONNX export the
   same way as the cross-encoder above (`rten-convert --no-infer-shapes`);
   kept at `~/.remind-me/embed/all-MiniLM-L6-v2.rten` +
   `~/.remind-me/embed/tokenizer.json`. Verified against real weights
   (`cargo run -p remind_me_core --example verify_onnx_embed --features
   local-embed`, deleted after use): 384-dimensional output, norms ~1.0,
   and a real semantic gap — cosine similarity 0.53 between the query and
   an on-topic passage vs. 0.016 against an off-topic one. `resolve_embedder`/
   `available_embedder` now return `Box<dyn Embedder>` instead of the
   concrete `OllamaEmbedder` so both backends share one call path — every
   existing caller kept working via `&*embedder`/`&**embedder` at the few
   sites that needed it (`Box<dyn Embedder>` does not `as`-cast to
   `&dyn Embedder` directly; deref does). 6 tests cover backend dispatch and
   graceful-failure paths for both build configurations
   (`tests/onnx_embedder_test.rs`), plus 4 new `tests/vectors_test.rs` cases
   for `fuse_query_embedding` itself; the full existing `vectors_test.rs`/
   `reranker_test.rs`/`ollama_embedder_test.rs`/`injectable_embedder_test.rs`
   suites (83 tests total across the six directly-touched files) pass
   unchanged.

Cutover on this host was no longer blocked by a missing Rust *search*
capability. It found one more gap anyway — see the cutover record below.

---

## home-pc-win cutover, executed (2026-08-09)

**One more gap, found only by actually cutting over — not by reading.**
The port-8767 role this host runs is not what `rusty-remind-me remote`
is. Python's `--serve-mcp` (standalone, no `--serve-ui`) calls the raw
FastMCP HTTP transport directly — `remind_me_mcp/__main__.py:546-553` —
with **no auth middleware**; `remind_me_mcp/config.py:254-256` states this
outright: *"Standalone MCP HTTP mode (--serve-mcp without --serve-ui) ...
stays unauthenticated by design, relying on its localhost-only default
bind."* `remind_me_remote` (`rusty-remind-me remote`) is the port of the
reference's *different*, always-token-gated `--serve-remote` mode — built
for claude.ai reaching in over the internet
(`crates/remind_me_remote/src/auth.rs`: every `/mcp` request needs a
matching `/mcp/<token>` path or `Authorization: Bearer`). There is no Rust
equivalent of Python's trust-loopback, zero-auth local mode. Cutting over
with the bare `http://127.0.0.1:8767/mcp` URLs every client already had
configured would have 401'd Claude Desktop, Claude Code, and this
harness's own `remind-me-win` connection simultaneously.

**Resolved by adding auth, not by adding code.** Generated a
`REMIND_ME_REMOTE_TOKEN` and moved every client to the secret-path form
(`http://127.0.0.1:8767/mcp/<token>`, no header support required) rather
than building a new no-auth mode — `remind_me_remote`'s existing gate is
the *more* secure posture, and Python's own "unauthenticated by design"
choice was never reviewed against exposing the same port to whatever else
is on this machine.

**What actually happened:**
1. Built `--features "remind_me_core/rerank remind_me_core/local-embed"`.
2. Verified read-only first: `rusty-remind-me stats`/`search` against the
   real `~/.remind-me/memory.db` (14758 memories) before touching anything
   live.
3. Replaced `serve-mcp.ps1` with `serve-mcp-rust.ps1` — same
   `REMIND_ME_NODE_ID=home-pc-win`/`HUB_URL`/`SYNC_SECRET`/`SYNC_INTERVAL`,
   plus the rerank/ONNX/HyDE paths from the "Feature parity gaps" section
   above, plus the new `REMIND_ME_REMOTE_MCP=1`/`REMOTE_HOST=127.0.0.1`/
   `REMOTE_PORT=8767`/`REMOTE_TOKEN`. One value had to be added that
   Python never needed: `REMIND_ME_EMBEDDING_BACKEND=onnx` — Python's
   `EMBEDDING_BACKEND` defaults to `"onnx"` when unset
   (`remind_me_mcp/config.py:182`), this port defaults to disabled instead
   (matching its own "off until asked for" convention across every
   optional backend), so leaving it unset here would have silently
   dropped semantic search entirely.
4. Stopped the Python process (`Stop-Process -Force`) — its dashboard
   sidecar (port 5199) died with it, confirming the Job-object relationship
   `serve-mcp.ps1`'s own comment described. Started
   `rusty-remind-me remote` and, since no second Scheduled Task could be
   created non-interactively (`schtasks /Create` → Access denied without
   an elevated/interactive prompt), folded the dashboard
   (`rusty-remind-me api 5199 127.0.0.1`) into `serve-mcp-rust.ps1` as a
   detached child instead of its own task.
5. Updated the "remind-me-mcp-http" Scheduled Task's target to
   `serve-mcp-rust.ps1` (`schtasks /Change`) so the swap survives reboot.
6. Verified after: `netstat` shows the new PID holding `0.0.0.0:8766` and
   `127.0.0.1:8767`; `GET /health` → `200 {"status":"ok"}`; bare `/mcp` →
   `401` (confirms the gap above is real and now closed); a full
   `initialize` JSON-RPC handshake through
   `http://127.0.0.1:8767/mcp/<token>` → `200` with
   `serverInfo.name: rusty_remind_me`; dashboard `GET /` → `200`.
7. Updated Claude Desktop (`claude_desktop_config.json`) and Claude Code
   (`.claude.json` → `mcpServers.remind-me-win`) to the token URL. This
   harness's own `remind-me-win` connection is the same entry — per
   Lesson 3, it will not pick up the change until this omp session itself
   restarts; memory tools 401 for the rest of this conversation.

**Not migrated in this pass:** `REMIND_ME_QUERY_EXPANSION=hyde` is
configured and will silently no-op exactly as before — no Ollama daemon is
running on this host, so HyDE was already a no-op under Python too. Not a
regression, just still unverified against a real LLM (see Lesson 8).

**New consumer inventory row for this host** (this table's existing rows
are the *other* — Linux — host; see Lesson 1, rebuild before reusing):

| Consumer | Config | Supervisor | Action taken |
| --- | --- | --- | --- |
| Claude Desktop | `claude_desktop_config.json` → `mcpServers.remind-me` (via `mcp-remote`) | none (spawned per launch) | URL updated to `/mcp/<token>`; app restart still needed to reconnect |
| Claude Code | `.claude.json` → `mcpServers.remind-me-win` | none (spawned per launch) | URL updated to `/mcp/<token>`; app restart still needed to reconnect |
| omp agent (this harness) | same `.claude.json` entry as Claude Code | none (spawned per session) | same edit; **this session will not pick it up until restarted** |
| shared MCP+sync+peer process | Scheduled Task `remind-me-mcp-http` → `serve-mcp-rust.ps1` | Task Scheduler, at logon | Python stopped, `rusty-remind-me remote` started; task target updated |
| dashboard/API | folded into `serve-mcp-rust.ps1` (detached child, no task of its own) | none — see step 4 above | Python stopped (died with parent), `rusty-remind-me api` started |

---

## work-pc-win cutover, executed (2026-08-10)

A second, independently-scoped host — `work-pc-win`, not the same machine as
`home-pc-win` above. Its topology was different again: no shared HTTP process
and no Task Scheduler entry at all. Two consumers, both spawning their own
process:

| Consumer | Config | Transport (before) | Transport (after) |
| --- | --- | --- | --- |
| Claude Desktop | `%APPDATA%\Claude\claude_desktop_config.json` → `mcpServers.remind-me` | stdio, `C:\dev\remind_me\.venv\Scripts\python.exe -m remind_me_mcp` | stdio, `rusty-remind-me.exe server` |
| Claude Code / omp (this harness) | `~/.claude.json` → `mcpServers.remind-me` | Streamable HTTP, `http://127.0.0.1:8767/mcp` (Python `--serve-mcp`, unauthenticated-by-design, started by hand via `start-mcp-http.vbs`) | stdio, `rusty-remind-me.exe server` |

**Design decision: collapse the HTTP consumer to stdio instead of adopting
`remind_me_remote`'s token-gated mode.** home-pc-win needed the HTTP
standalone port because one process served MCP *and* the peer-sync receiver
*and* the dashboard for three consumers at once — killing it meant killing
all three. Here, the port-8767 process served exactly one consumer (Claude
Code) and nothing else depended on it: no dashboard, no Task Scheduler entry,
not even autostart (`start-mcp-http.vbs` had to be run by hand, and per
`mcp-http.log` the last process had already exited five days earlier). With
no shared-process constraint, stdio is strictly simpler than standing up
`remind_me_remote` for a single local caller: no port, no token, no
"unauthenticated by design" gap to close, one fewer moving part. Claude
Desktop was already stdio; Claude Code now matches it. `start-mcp-http.ps1`/
`.cmd`/`.vbs` and the stale `mcp_server.pid` moved to
`~/.remind-me/legacy-python-mcp-http/` rather than deleted.

**What actually happened:**
1. Built `--features "remind_me_core/rerank remind_me_core/local-embed"`.
   Neither client config set `REMIND_ME_RERANK`/`REMIND_ME_QUERY_EXPANSION`/
   `REMIND_ME_EMBEDDING_BACKEND`, meaning both ran on the reference's
   defaults — reranking on (`BAAI/bge-reranker-base`) and ONNX embeddings on
   (`sentence-transformers/all-MiniLM-L6-v2`) — so parity needed both, not
   neither. Converted both from the already-cached ONNX exports the same way
   as home-pc-win (`rten-convert --no-infer-shapes`; the Windows
   `shape_inference` `PermissionError` bug is still live). HyDE was never
   configured on this host either, same as home-pc-win.
2. **Toolchain was broken, not just missing.** `rust-toolchain.toml` pins
   1.97.0; rustup's installed copy of it was missing the `cargo`/`rustc`
   components outright (`cargo.exe`: "is not installed for the toolchain"),
   and reinstalling the component hit `detected conflict:
   lib\rustlib\manifest-cargo-*` — a corrupted local install, not a network
   or config problem. Building against the already-complete
   `stable-x86_64-pc-windows-msvc` toolchain instead (`cargo
   +stable-x86_64-pc-windows-msvc`) hit a second, unrelated problem: this
   host's PATH resolves `link.exe` to Git for Windows' coreutils `link`
   (hard-link creation) ahead of any MSVC linker, and there is no Visual
   Studio C++ workload installed to fall back to at all (`link.exe` under
   either Visual Studio Program Files tree: zero matches). Building against
   `stable-x86_64-pc-windows-gnu` instead — already fully installed, and
   rustup's own default toolchain on this host — sidestepped both problems:
   it links with the bundled MinGW `gcc`/`ld`, never touches `link.exe`, and
   needed no repair. `rust-version = "1.94"` in the workspace `Cargo.toml`
   already says the 1.97 pin is aspirational, not a hard floor.
3. Verified against a copy first, not the live file (Lesson 4): copied the
   real 289 MB `memory.db`, ran `stats`/`search` against the copy, watched
   the schema auto-migrate v27 → v29 cleanly (this host's Python checkout —
   `C:\dev\remind_me`, `pyproject.toml` says 1.54.0 — only ever wrote up to
   v27; the reference's own drift from v27 to v29 documented earlier in this
   file had never reached this particular clone). Confirmed the embedder and
   reranker actually produce real signal, not just "configured": on a
   throwaway 3-memory database, `vec_score`/`rerank_score` came back
   populated (`null` on the full copy at first, because CLI `search` and a
   no-op `remind_me_reindex` don't exercise them — `remind_me_reindex` has no
   `limit` argument despite one being a reasonable guess, so passing one is
   silently ignored and reindexes the whole store) — and the reranker
   ordered a solar-panel/board-meeting/pizza-topping triple correctly by
   actual relevance, with sane cross-encoder logits.
4. **A newly-built binary run from a path under `C:\Users\<user>\...` was
   blocked outright, not slow.** This host runs Trellix (McAfee) Endpoint
   Security with an Exploit Prevention rule, "Running files from common user
   folders" (T1562.001), that denies execution of anything launched from
   under the user profile — including `~/.remind-me/bin/`, a directory this
   pass created for exactly this purpose. `os error 5` ("Access is denied")
   gave no indication why; the actual cause only showed up in
   `Get-WinEvent`/the Application log (Trellix Endpoint Security, event
   1092). Not worked around — moved the binary to `C:\tools\lang\
   rusty-remind-me\rusty-remind-me.exe`, the same non-profile convention
   every other dev tool on this host already uses (`rustup`, `uv`, `git`),
   which is unaffected by the rule. Confirmed via the same event log that no
   corresponding block fired for that path. A second, unrelated ~40s stall
   right after (running `stats` against the real file with no timeout
   override) was not this rule — it was the real v27→v29 migration actually
   running against the live 290 MB file, confirmed by `PRAGMA user_version`
   still reading 27 immediately after an artificially-truncated attempt, and
   29 with intact row counts and `PRAGMA integrity_check` after letting it
   finish. Worth remembering as its own lesson: a "hang" on a freshly-cut-over
   binary can be legitimate first-touch migration cost, not a fault — check
   `PRAGMA user_version` and `integrity_check` before assuming otherwise.
5. Updated both `claude_desktop_config.json` and `.claude.json` in place —
   same env var names throughout (`REMIND_ME_NODE_ID`/`_CLIENT`/`_HUB_URL`/
   `_SYNC_SECRET`/`_PEER_PORT`/`_SYNC_INTERVAL`/`_STATIC_PEERS` unchanged from
   what Python was already reading), plus `REMIND_ME_DEFAULT_RESPONSE_FORMAT=
   markdown` (the reference always rendered Markdown for MCP tool text; this
   port defaults to JSON) and the four rerank/embed model-path variables.
   Not run through `rusty-remind-me configure`: that subcommand only knows
   Claude Desktop/Antigravity/Cursor/Codex, not `~/.claude.json`, and writes
   an additive `mcpServers.rusty-remind-me` key rather than replacing the
   existing `remind-me` entry — a cutover wants the latter.
6. Verified after, against the real file, by spawning each config's exact
   `command`/`args`/`env` and driving the real MCP stdio protocol
   (`initialize` → `remind_me_server_status`) rather than trusting the JSON
   edit: both report `schema: v29 (expected v29)`, `embeddings: active`,
   `mcp: active`, `memories: 14627`, `backups: 9` (the pre-migration
   snapshot fired automatically).

**Not migrated in this pass:** `REMIND_ME_QUERY_EXPANSION`/HyDE — unused
under Python on this host already, so still a no-op, not a regression.

**Not stopped, because nothing was running.** Unlike home-pc-win, no live
Python process needed killing — the standalone HTTP server (PID from a stale
`mcp_server.pid`) had already exited days earlier, and Claude Desktop's
stdio Python child dies with the parent app, which was not running during
this pass. Per Lesson 3, both edits take effect on each client's *next*
restart, not this session — this omp session itself still holds whatever
connection (or lack of one) it started with.

`uv tool` still has `remind-me-mcp v1.54.0` installed globally and
`C:\dev\remind_me` still exists untouched — kept per Lesson 7, not removed
cold.
---

## The consumer inventory

Every process that can open `~/.remind-me/memory.db` or call into
`remind_me`/`rusty_remind_me`, as found on this host on 2026-08-08. Rebuild
this table (don't trust it) before the next cutover — see Lesson 1.

| Consumer | Config | Supervisor | Found running | Action |
| --- | --- | --- | --- | --- |
| Claude Code | `~/.claude.json` → `mcpServers.remind-me` | none (spawned per launch) | already `rusty-remind-me server` | none |
| omp agent (this harness) | `~/.omp/agent/mcp.json` | none (spawned per session) | Python `remind-me-mcp` v1.51.0 (`uv tool` install) | config edited; **required a full session restart** to take effect |
| hermes-dashboard | `~/.hermes/config.yaml` → `mcp_servers.remind-me` (shared) | `systemctl --user` (`hermes-dashboard.service`) | Python `python -m remind_me_mcp` (`~/remind_me/.venv`) | config edited; `systemctl --user restart hermes-dashboard.service` |
| hermes-gateway | same config file as above | `systemctl --user` (`hermes-gateway.service`) | same Python process, second instance | same config edit; `systemctl --user restart hermes-gateway.service` |
| `remind-me-hub` | `systemctl --user` unit → podman `remind-me-hub:latest` | `systemctl --user` | already the Rust image (91MB; old image cold-tagged `python-rollback`/`1.5.0`, 193MB) | none |
| `remind-me-connector` (claude.ai remote) | `systemctl --user` unit | `systemctl --user` | already `rusty-remind-me remote` | none |
| `remind-me-ui` (dashboard/API) | `systemctl --user` unit | `systemctl --user` | already `rusty-remind-me api 5199 100.83.168.90` | none |
| `remind-me-postgres` | `systemctl --user` unit → podman postgres | `systemctl --user` | unrelated engine, unchanged by either implementation | none |

Two consumers were already correct because they were the pilot for the
server-side switch. The other two (omp, hermes) were missed because nothing
enumerates "who has their own copy of the MCP launch command" — each is a
config file inside a *different* application's own state directory, none of
which this repo owns or can lint.

---

## Lessons learned

1. **No single registry of consumers exists, and one is needed.** This table
   was built by cross-referencing `ps -ef`, `ss -tlnp` (to catch the
   `REMIND_ME_PEER_PORT` listener), `systemctl --user list-units`, and a
   manual read of three independently-owned config files. There is no
   `remind-me consumers` command. Until there is, this document *is* the
   registry — update the table above whenever a consumer is added, removed,
   or its config file moves.

2. **"Production switch" and "client cutover" are different events.**
   Finishing the server-side switch (hub, connector, dashboard) implies
   nothing about client state. Treat them as separate checklists with
   separate completion criteria; don't declare victory on one and assume the
   other followed.

3. **A stdio client cannot hot-swap.** Editing a config file never affects an
   already-spawned process — the child keeps running the old binary until the
   *parent* (the client application itself) restarts and re-reads its config.
   Confirmed directly: after editing `~/.omp/agent/mcp.json`, `ps` still
   showed the old Python pid, unchanged, until the omp session itself was
   restarted. This is the same caveat `gap-analysis.md` already recorded for
   the server-side switch ("this machine's already-running stdio sessions...
   pick up the switch on their next restart, not mid-session") — it applies
   just as literally to every other stdio client, and it is easy to forget
   because editing the file *feels* like the fix.

4. **Verify against the live file before editing any config.** Before
   touching a single config, run `rusty-remind-me <verb>` by hand with the
   target consumer's exact environment variables against the real
   `~/.remind-me/memory.db`, and confirm sane output. This is the same
   discipline `gap-analysis.md`'s 2026-08-07 entry already learned the hard
   way ("None came from reading either codebase") — re-reading documentation
   is not a substitute for one real command against the real file.

5. **Recycle through the supervisor, not the subprocess.** `hermes-dashboard`
   and `hermes-gateway` are `systemctl --user` units whose main process
   spawns the MCP server as a child (through a watchdog wrapper). Restart the
   *unit* (`systemctl --user restart <name>`) so the whole tree re-execs
   against the new command line — signalling the child subprocess directly is
   not equivalent and was not attempted here for exactly that reason.

6. **Two things that look like bugs during this kind of audit and are not:**
   - **Multiple co-located processes sharing one `REMIND_ME_NODE_ID`**
     (here, `ai-server`, across omp + both hermes processes) **is correct**
     when they share one `REMIND_ME_MCP_DIR`. Node identity models the shared
     local *database*, not the process — `node_id`/`client` are stamped per
     row precisely so a node can tell its own writes apart from a peer's
     (`docs/adr/0004-sync-protocol-and-conflict-resolution.md`). `client`
     (`omp_agent`, `hermes_agent`, …) is the per-process distinction; `node_id`
     is not supposed to be.
   - **Only one of several co-located processes holds
     `REMIND_ME_PEER_PORT`** (default 8766, `remind_me_core::sync::DEFAULT_PEER_PORT`).
     The processes that lose the bind race simply never start their direct
     peer-push *receiver* — hub-mediated pull/push sync is a separate,
     unaffected path. Confirmed post-cutover: `ss -tlnp` showed exactly one
     holder, `mcp-stderr.log` showed continued clean `Hub sync complete`
     cycles, zero bind-error log lines anywhere.

7. **Keep the old install cold instead of deleting it.** Neither
   `~/remind_me/.venv` (the reference checkout) nor the `remind-me-mcp` v1.51.0
   `uv tool` install were removed. They cost nothing idle and are the entire
   rollback plan below — deleting them before confidence is established would
   turn a config edit back into a reinstall.

8. **"Zero client-config edits" was itself wrong, and only cutting over for
   real found it.** The "Feature parity gaps" section above predicted this
   host's cutover would need no client-config changes, reasoning purely
   from what port every client already pointed at. That reasoning never
   checked whether the *auth model* on both ends matched — Python's
   standalone `--serve-mcp` is unauthenticated by design (trust-loopback);
   `rusty-remind-me remote` is a port of the reference's *different*,
   always-token-gated `--serve-remote` mode. Two builds serving the same
   protocol on the same port are not interchangeable if one gates and the
   other does not. The lesson isn't "always add auth" or "always match the
   reference's posture" — it's that a topology assumption made from reading
   config files, however carefully, is still a hypothesis until the actual
   swap is attempted end-to-end. See the home-pc-win cutover record above
   for how this one was found and closed.

---

## Runbook: cutting a client over

1. **Enumerate.** `ps -ef | grep -iE remind`, `ss -tlnp | grep <peer port>`,
   `systemctl --user list-units 'remind-me-*'` plus every other app's own
   units, and a manual read of every known client config. Don't trust the
   table above without re-deriving it — see Lesson 1.
2. **Classify each hit**: config file location, current `command`/`args`/env,
   and its supervisor (systemd unit vs. bare per-session/per-launch process
   with nothing watching it).
3. **Verify first.** Run `rusty-remind-me <verb>` from a shell carrying that
   consumer's exact `REMIND_ME_*` environment against the real
   `REMIND_ME_MCP_DIR`. Confirm real, sane output before editing anything.
4. **Edit in place.** Swap `command`/`args` to the `rusty-remind-me server`
   binary path. Copy every `REMIND_ME_*` variable byte-for-byte from the
   existing entry. Add `REMIND_ME_PEER_BIND=127.0.0.1` and
   `REMIND_ME_DEFAULT_RESPONSE_FORMAT=markdown` to match the already-migrated
   reference config (Claude Code's, in this case) so tool output shape does
   not shift for existing callers mid-cutover.
5. **Recycle correctly.** `systemctl --user restart <unit>` for
   systemd-managed consumers. A full process/application restart for anything
   with no supervisor (this is the step that is easy to skip — see Lesson 3).
6. **Verify after.** `ps aux | grep remind_me_mcp` (or the reference's module
   name) returns nothing; the new process tree shows `rusty-remind-me`;
   `systemctl --user show <unit> -p NRestarts` reads `0` (no crash loop); a
   functional smoke test (e.g. `remind_me_stats`) through that consumer's own
   tool surface returns real data from the real database.
7. Repeat until the inventory has no legacy rows left, then re-run step 1 one
   more time — the enumeration is the part most likely to have missed
   something, not the edit.

---

## Rollback

Per consumer: revert `command`/`args` to the Python invocation recorded in
this document's "Found running" column, recycle the same way (systemd
restart, or full session/app restart). No data migration in either direction
— both implementations open the same SQLite file and schema version
(`README.md` § Database Location; `docs/adr/0016-memory-ids-are-opaque.md`),
so rollback is a config-and-restart operation, not a data operation. Keep the
reference install cold (Lesson 7) until it's actually safe to remove it.
