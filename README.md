# `rusty_remind_me`

> High-performance, persistent long-term memory engine and Model Context Protocol (MCP) server written in Rust, built on the **Rusty Mill** ecosystem.

> **Lives in the Rusty Mill monorepo.** Since v0.2.1 this product is developed
> and released from [`Rusty-Mill/rusty_mill`](https://github.com/Rusty-Mill/rusty_mill)
> under `crates/apps/rusty_remind_me` (imported with full history). The
> standalone [`baileyrd/rusty_remind_me`](https://github.com/baileyrd/rusty_remind_me)
> repository is frozen at v0.2.0. Paths below are relative to this directory
> unless they say otherwise; `cargo` commands run from the monorepo root.

`rusty_remind_me` is a native Rust port of `remind-me`. It equips AI assistants (such as Claude Desktop, Antigravity, Cursor, OpenAI Codex, and custom LLM agents) with persistent, searchable memory using SQLite FTS5 full-text search, ACT-R inspired memory vitality decay, Reciprocal Rank Fusion (RRF) search ranking, a structured Knowledge Graph entity system, and automated markdown wiki compilation.

---

## Key Features

- **Hybrid Search Engine**: FTS5 BM25 keyword matching combined with Reciprocal Rank Fusion (RRF) rank scoring.
- **Semantic Search, Reranking & Query Expansion**: Optional vector search (Ollama or an in-process ONNX bi-encoder), cross-encoder reranking, and HyDE query expansion — each off by default, each degrading to keyword-only on any failure.
- **ACT-R Memory Vitality Model**: Time-based exponential decay, write-time priors by category/source, and bridge protection (decay rate halved for frequently accessed memories).
- **Forward-Compatible Model Context Protocol (MCP) Server**: Stdio JSON-RPC MCP server with dynamic protocol version negotiation (supporting `2024-11-05` through upcoming 2026 releases), resources, prompts, dynamic tool change notifications (`listChanged`), and tool execution.
- **Automated Client Setup**: Built-in `configure` command & scripts for 1-click MCP setup across **Claude Desktop**, **Antigravity**, **Cursor**, and **Codex**.
- **REST API Server**: Synchronous HTTP daemon hand-rolled on `std::net::TcpListener` (no framework, not even `tokio`) exposing endpoints for health checks, memory storage, FTS5 retrieval, and stats.
- **Multi-Node Sync & Remote Connector**: Push/pull sync against a hub or discovered peers (static list or Tailscale), plus a Streamable HTTP remote MCP connector (secret-path/bearer or OAuth 2.1) for network-reachable clients like claude.ai.
- **Knowledge Graph Entities**: Canonical entity deduplication, alias resolution, and relation traversal up to 3 hops.
- **Markdown Wiki Synthesis**: Topic-based wiki page compilation, queryable topic search, and
  bulk import of Markdown directories (`wiki-import`) — the ingestion path for
  [`dbs export-wiki`](https://github.com/baileyrd/Daily-Backup-System).
- **Rusty Mill Ecosystem**: Part of the `Rusty Mill` project family; no `rusty_*` crates are wired in as dependencies yet — current dependencies are ordinary crates.io crates (`serde`, `tokio`, `rusqlite`, `chrono`, `uuid`).

---

## Installation & Setup

A full walkthrough from a clean clone to a working setup — build, connect a
client (Claude Code plugin or any other MCP client), verify it, then
optionally add multi-node sync and the other off-by-default features.

### Prerequisites

- **Rust 1.94+** with Cargo.
- Nothing else is required for the base install. Semantic search's `ollama`
  backend and HyDE query expansion need a running
  [Ollama](https://ollama.com) daemon — optional, see [Search
  Quality](#search-quality-embeddings-reranking--query-expansion) below.

### 1. Clone & build

```bash
git clone https://github.com/Rusty-Mill/rusty_mill
cd rusty_mill
cargo build --release -p rusty-remind-me
```

Always pass `-p`: a bare `cargo build` at the monorepo root builds every
Rusty Mill crate. This produces `target/release/rusty-remind-me` (at the
monorepo root). Put it on `PATH` either by adding `target/release` to it, or
by installing it directly:

```bash
cargo install --path crates/apps/rusty_remind_me/crates/remind_me_cli
```

Verify it's reachable:

```bash
rusty-remind-me stats
```

A fresh install prints an empty store's stats rather than erroring — that's
expected; the database (`~/.remind-me/memory.db` by default) is created on
first use.

Prebuilt binaries for Linux, macOS (Intel/Apple Silicon), and Windows are
also published on the [Releases
page](https://github.com/Rusty-Mill/rusty_mill/releases) for every
tagged version (tags `rusty-remind-me-vX.Y.Z`; v0.2.0 and earlier are on the
[standalone repo's Releases
page](https://github.com/baileyrd/rusty_remind_me/releases)) — each includes both `rusty-remind-me` and
`rusty-remind-me-hub` — if you'd rather skip the build step. See [Releases
and versioning](#releases-and-versioning) below for how those get produced.

### 2. Connect it to a client

**Claude Code plugin** (recommended if you're using Claude Code): the
monorepo ships a marketplace at its root (`.claude-plugin/marketplace.json`,
pointing at this directory's plugin manifest, described in [Claude Code
Plugin](#claude-code-plugin) below), so no separate marketplace repo is
needed —

```bash
claude plugin marketplace add Rusty-Mill/rusty_mill
claude plugin install rusty-remind-me@rusty-mill
```

An existing install from the old `rusty-remind-me` marketplace
(`baileyrd/rusty_remind_me`) keeps working: that marketplace's entry now
points at this directory of the monorepo, so updates arrive from here either
way. Switching to `rusty-remind-me@rusty-mill` is still recommended.

`rusty-remind-me` must already be on `PATH` (step 1) — the plugin's
`.mcp.json` runs it as a subprocess, it isn't bundled. Verify with
`claude plugin list` (shows `enabled`) and `claude mcp list` (shows the
`rusty-remind-me` server `Connected`); a fresh Claude Code session should
then show recent memories injected by the `SessionStart` hook, and
`/rusty-remind-me:remember`/`/rusty-remind-me:recall` should work as slash
commands.

**Any other MCP client** (Claude Desktop, Antigravity, Cursor, Codex, or a
generic MCP-config-reading client): run the auto-configurator, which merges
the `rusty-remind-me` MCP server entry into every client config it finds —

```bash
rusty-remind-me configure
```

See [Automated Client
Setup](#automated-client-setup-claude-desktop-antigravity-cursor-codex)
below for the PowerShell/Python equivalents and exactly which config files
this touches. Restart the client afterward — a running MCP client does not
pick up a new stdio server until it restarts.

### 3. Verify end to end

```bash
rusty-remind-me add "Installed rusty_remind_me" --category general
rusty-remind-me search "installed"
```

Through whichever client you connected in step 2, ask it to recall something
you just added — that exercises the actual MCP tool path, not just the CLI.

### 4. Optional: multi-node sync via a hub

Everything above is a complete, fully-functional single-machine install —
sync is entirely opt-in. To share memories across machines, stand up a hub
once and point every node at it:

```bash
# On the hub machine — one container, no separate database server:
crates/remind_me_hub/setup.sh install
# Prints the generated SYNC_SECRET every client below needs.

# On each client machine:
REMIND_ME_SYNC_SECRET=<printed secret> rusty-remind-me configure \
    --node-id my-laptop --hub-url http://<hub-host>:8765
```

That runs the hub on its embedded storage engine. A hub still on the
Postgres or SQLite store it used to offer moves over with
`crates/remind_me_hub/setup.sh migrate`, and `crates/remind_me_hub/client-setup.sh
--node-id my-laptop --tunnel me@hub-host` automates the SSH-tunnel case.
Docker Compose, Fly.io, and Railway deployments are under
`crates/remind_me_hub/deploy/`. See [`crates/remind_me_hub/README.md`](crates/remind_me_hub/README.md)
for the full reference (routes, security posture, the store) and [Multi-Node
Sync, Hub & Remote Connector](#multi-node-sync-hub--remote-connector) below
for every client-side environment variable this `configure` call sets.

If you installed as a Claude Code plugin (step 2), see [Multi-node sync
through the plugin](#multi-node-sync-through-the-plugin) — the short version
is that ambient environment inheritance (shell export, or Claude Code
`settings.json`'s `env` key) is the supported way to get these three
variables into the plugin's MCP server process.

### 5. Optional: everything else

Each of these is independently off by default and documented in its own
section — enable only what you need:

| Want | See |
| --- | --- |
| Semantic/vector search, cross-encoder reranking, HyDE query expansion | [Search Quality](#search-quality-embeddings-reranking--query-expansion) |
| A network-reachable MCP endpoint (e.g. for `claude.ai`'s custom connector) | [Remote MCP connector](#remote-mcp-connector-remind_me_remote-rusty-remind-me-remote) |
| The REST API / dashboard | [REST API Endpoints](#rest-api-endpoints) — `rusty-remind-me api [port]` |
| Moving over from the Python `remind_me` (retired) | [Coming from the Python `remind_me`](#coming-from-the-python-remind_me) |

---

## Workspace Structure

`rusty_remind_me` is six modular crates (members of the Rusty Mill monorepo's Cargo workspace):

```
rusty_remind_me/
├── Cargo.toml                  # Workspace manifest (6 crates)
├── README.md                   # User & Developer guide
├── ARCHITECTURE.md             # Technical design & schema documentation
├── CONTRIBUTING.md             # Development & testing guidelines
├── docs/
│   └── CUTOVER.md              # Historical: the 2026-08 cutover of clients from the Python reference
├── scripts/
│   ├── configure_mcp.ps1       # PowerShell auto-configuration script for Windows
│   └── configure_mcp.py        # Cross-platform Python auto-configuration script
└── crates/
    ├── remind_me_core/         # Domain models, SQLite/FTS5 database, ACT-R decay, RRF ranking
    ├── remind_me_mcp/          # Stdio JSON-RPC MCP protocol engine & tool handlers
    ├── remind_me_api/          # Async REST HTTP server daemon (`rusty_http` + `tokio`)
    ├── remind_me_remote/       # Remote MCP connector over Streamable HTTP (claude.ai, OAuth 2.1)
    ├── remind_me_hub/          # Multi-node sync hub: push/pull relay + Tailscale peer discovery
    └── remind_me_cli/          # Unified `rusty-remind-me` binary executable
```

---

## Releases and versioning

`.github/workflows/remind-me-release.yml` (at the monorepo root) watches
every push to `main` that touches one of this product's six crate manifests.
When their `version` has moved past the last version published from the
monorepo (checked against existing `rusty-remind-me-vX.Y.Z` tags — nothing
hand-maintains a changelog of already-shipped versions), it:

1. Builds `rusty-remind-me` and `rusty-remind-me-hub` in release mode,
   natively, for `x86_64-unknown-linux-gnu`, `x86_64-apple-darwin`,
   `aarch64-apple-darwin`, and `x86_64-pc-windows-msvc` — one job per
   platform, no cross-compilation, matching the "no system binary at build
   time" convention the optional feature flags already follow (see
   `crates/remind_me_core/Cargo.toml`).
2. Packages the Claude Code plugin surface (`.claude-plugin/`, `commands/`,
   `hooks/`, `.mcp.json`) into its own archive.
3. Publishes a GitHub Release tagged `rusty-remind-me-vX.Y.Z` — prefixed,
   because the monorepo releases more than one product — with all of the
   above attached as downloadable assets.

Bumping the version is therefore the entire release process — there is no
separate "cut a release" step, and nothing here pushes a tag by hand; the
release action creates it from the version it read.

The six crates each carry their own literal `version` (the monorepo's
`[workspace.package]` belongs to other crates), and must be bumped together
with `.claude-plugin/plugin.json`'s `"version"`.
`scripts/get_workspace_version.sh` fails if the six disagree, and
`scripts/check_plugin_version.sh` fails if the plugin manifest differs from
them; both run on every PR touching this directory (the `plugin-version` job
in the monorepo's `.github/workflows/remind-me-checks.yml`), so a bump that
misses one never merges — the release's plugin archive is never a version
behind the binaries it ships alongside.

---

## Claude Code Plugin

`rusty_remind_me` also ships as a [Claude Code plugin](https://code.claude.com/docs/en/plugins) — `.claude-plugin/plugin.json` in this directory (the plugin root), bundling:

- **`.mcp.json`** — the same stdio MCP server (`rusty-remind-me server`) described above, registered automatically instead of via the `configure` command or a manual client config edit.
- **`hooks/hooks.json`** — a `SessionStart` hook (`hooks/scripts/session-start.sh`) that runs `rusty-remind-me list --limit 8` directly (no MCP round-trip, no model decision required) and injects the most recently written memories as context at the start of every session.
- **`commands/remember.md`, `commands/recall.md`** — `/rusty-remind-me:remember <text> [--category NAME] [--tags a,b]` and `/rusty-remind-me:recall <query> [--limit N]`, each running `rusty-remind-me add`/`search` directly (via the command body's `` !`...` `` bash-execution syntax) before the model ever sees the prompt. `allowed-tools` scopes each command to exactly one CLI invocation shape (`Bash(rusty-remind-me add *)` / `Bash(rusty-remind-me search *)`). Plugin-provided commands are namespaced as `/plugin-name:command-name` — the bare `/remember` is not registered, only `/rusty-remind-me:remember`.

Three different connections, each with a different trigger:

| Surface | Fires when | Model's role |
| --- | --- | --- |
| MCP tools | Model issues a `tools/call` | Decides whether and when to call |
| `SessionStart` hook | Every session start, unconditionally | None — output is injected before the model sees the prompt |
| `/rusty-remind-me:remember`, `/rusty-remind-me:recall` | User types the command | None for the CLI call itself; model only sees/summarizes the result |

The hook and the slash commands both bypass MCP entirely — they invoke the plain CLI binary as a subprocess and never speak the MCP JSON-RPC protocol. The hook degrades safely: if `rusty-remind-me` isn't on `PATH` yet, or the store is empty, it emits `{"continue": true}` (optionally with a one-line `systemMessage` nudge to build the binary) rather than failing the session. The slash commands surface the same "not on PATH" condition as plain shell output for the model to relay, rather than silently retrying some other way.

To use it, `rusty-remind-me` must be on `PATH` (`cargo build --release -p rusty-remind-me` then add `target/release` to `PATH`, or `cargo install --path crates/apps/rusty_remind_me/crates/remind_me_cli` from the monorepo root), then add the monorepo as a plugin marketplace in Claude Code (see [Connect it to a client](#2-connect-it-to-a-client)).

### Multi-node sync through the plugin

`.mcp.json` sets no `env` — Claude Code spawns a plugin's MCP server with
`{...process.env, ...pluginEnv}`, so with nothing in `pluginEnv` the
`rusty-remind-me server` process the plugin starts inherits Claude Code's own
environment in full. If `REMIND_ME_NODE_ID`, `REMIND_ME_HUB_URL`, and
`REMIND_ME_SYNC_SECRET` are already set in that environment, sync comes up
transparently — the plugin neither enables nor blocks it, and there's
nowhere here that would hardcode a shared secret into a file that ships with
the plugin.

What that inheritance does *not* change is which surface actually carries
sync: per the note below, the background sync thread is a long-lived
process's own background service (like the reminder scheduler or folder
watcher), not something the MCP connection specifically owns — the plugin's
bundled `rusty-remind-me server` runs one, but the `SessionStart` hook and
both slash commands still invoke the plain CLI as one-shot subprocesses that
exit immediately, same as `add`/`search` always have, sync thread or not. A
memory added via `/rusty-remind-me:remember` while the MCP connection is
down still lands in the local outbox; it just doesn't reach the hub until
some running `rusty-remind-me server`/`api`/`remote` process — this plugin's
or otherwise, against the same database — picks it up on its next cycle.

**Why this isn't a `userConfig`-driven install prompt.** Claude Code plugins
can declare `userConfig` fields that prompt the user for values when the
plugin is enabled — the natural-looking way to turn "set three env vars
yourself" into part of installing the plugin. Tried and abandoned: neither of
Claude Code's two documented delivery paths for a configured `userConfig`
value actually reaches this plugin's MCP server subprocess. Verified directly
by wrapping the `rusty-remind-me` binary and logging its real spawn
environment —

- `${user_config.KEY}` substitution inside `.mcp.json`'s own `env` block
  (what the plugin docs describe for "MCP and LSP server configs") does not
  resolve for an `argv=["server"]` spawn; the literal, un-substituted
  `${user_config.KEY}` string reaches the process instead. Worse than
  useless: `sync_enabled()` sees three non-empty strings and turns sync *on*
  against a garbage endpoint rather than leaving it off (this is what
  `is_unresolved_placeholder` in `sync/mod.rs` now guards against).
- `CLAUDE_PLUGIN_OPTION_<KEY>`, which Claude Code does export for a
  configured `userConfig` option, reaches this plugin's `SessionStart` hook
  invocation (`argv=["list", "--limit", "8"]`) but not the `argv=["server"]`
  MCP server spawn in the same session — confirmed by tagging captured
  environments with their argv and pid. The docs say these vars go to "hook
  processes"; that turned out to be exact, not illustrative.

Declaring `env` in `.mcp.json` is also not additive with today's ambient
inheritance even where substitution *did* work: `pluginEnv` always wins in
`{...process.env, ...pluginEnv}`, so wiring `userConfig` in would have
silently broken the shell/`settings.json`-based setup described above for
anyone already using it. Given neither delivery path works for the process
that needs the values, ambient inheritance stays the only supported way to
configure sync through this plugin.

---

## Automated Client Setup (Claude Desktop, Antigravity, Cursor, Codex)

You can automatically configure all installed AI client applications (Claude Desktop, Antigravity, Cursor, Codex / Generic MCP) to use `rusty_remind_me` as their memory backend:

### Option A: Via Built-in CLI Command
```bash
cargo run --bin rusty-remind-me -- configure
```

### Option B: Via PowerShell Script (Windows)
```powershell
.\scripts\configure_mcp.ps1
```

### Option C: Via Python Script (Cross-Platform)
```bash
python scripts/configure_mcp.py
```

Each setup option safely merges the `"rusty-remind-me"` MCP server configuration into:
- **Claude Desktop**: `%APPDATA%\Claude\claude_desktop_config.json`
- **Antigravity**: `%USERPROFILE%\.gemini\antigravity\mcp_config.json`
- **Cursor**: `%USERPROFILE%\.cursor\mcp.json`
- **Codex / Generic**: `%USERPROFILE%\.mcp\config.json`

---

## Database Location

By default the store is `~/.remind-me/memory.db`.

Two environment variables override it, most specific first:

| Variable | Names a | Notes |
| --- | --- | --- |
| `REMIND_ME_DB_PATH` | database **file** | Wins if both are set. |
| `REMIND_ME_MCP_DIR` | **directory** holding `memory.db` | Also where the wiki, API keys and connector token live. |

A leading `~` is expanded in either. A variable set to the empty string counts as unset.

The store is moving off SQLite (`docs/adr/0023-node-storage-moves-to-rusty-multimodal-db.md`). Until that ships, this is the file the node reads and writes.

## Coming from the Python `remind_me`

The Python [`remind_me`](https://github.com/baileyrd/remind_me) this project was ported from is **retired** (ADR-0023). Until the storage move above ships, this binary still opens a `memory.db` the Python server wrote, so moving over is a matter of pointing your MCP clients at `rusty-remind-me` instead:

```bash
rusty-remind-me configure                             # JSON output (the default)
rusty-remind-me configure --default-format markdown   # the Python server's output format
```

Do not run the Python server against the same `memory.db` afterwards. The two stopped being kept schema-identical when Python was retired, and the storage move will end file-level compatibility altogether.

`REMIND_ME_DEFAULT_RESPONSE_FORMAT` accepts `json` (the default) or `markdown`, and affects **only** the tools where the Python server had no `response_format` parameter at all — it returned Markdown from those and offered no JSON, whereas this port offers both and defaults to JSON so existing callers keep working. Tools that mirror a Python input model already use that model's own default and are untouched by this setting: Markdown for `search`, `list`, `wiki_list`, `stats`, `history`, `digest` and `list_reminders`, JSON for `vitality_report`. A per-call `"response_format"` argument always wins over the setting, in both directions.

A stdio client will not pick up a config change until it restarts. `docs/CUTOVER.md` records the 2026-08 cutover of every consumer on a real machine; it is kept as history.

## Multi-Node Sync, Hub & Remote Connector

Two additional crates extend a single-machine install into a synced fleet and
an internet-reachable MCP endpoint. Both are **off by default** — nothing
here changes single-machine behavior unless the corresponding environment
variables are set.

### Multi-node sync (`remind_me_hub`, `remind_me_core::sync`)

A node pushes its outbox to a hub and pulls the hub's changes back; peers are
also discovered directly (a static list, or Tailscale's local API) for
peer-to-peer push. Sync stays off until `REMIND_ME_NODE_ID`,
`REMIND_ME_HUB_URL`, and `REMIND_ME_SYNC_SECRET` are all set.

**The background sync thread needs a long-lived process to run in.**
`SyncWorker::from_env` — the loop that actually pushes/pulls on
`REMIND_ME_SYNC_INTERVAL` — is a background service like the reminder
scheduler or folder watcher (`crates/remind_me_core/src/sync/worker.rs`), not
something any single binary owns. `rusty-remind-me server`, `api`, and
`remote` each start one alongside their own work
(`crates/remind_me_cli/src/main.rs`); the one-shot CLI subcommands (`add`,
`search`, `list`, ...) don't, since they exit before a cycle could ever run.
A write from one of those one-shot commands still lands in the shared SQLite
outbox — it isn't lost — but nothing pushes it to the hub until a process
that *is* running a sync worker picks it up on its next cycle. Concretely:
one of `rusty-remind-me server`/`api`/`remote` has to be running somewhere
for sync to actually move data. Status is process-global, not tied to
whichever instance started the worker:
`remind_me_server_status` reports on whatever sync worker is live in *that*
process, the same way it already reports on the folder watcher.

| Variable | Purpose | Default |
| --- | --- | --- |
| `REMIND_ME_NODE_ID` | This node's identity, stamped on every row it writes | unset (sync off) |
| `REMIND_ME_CLIENT` | Human-readable label for this install, alongside `node_id` | `unknown` |
| `REMIND_ME_HUB_URL` | Hub this node pushes to / pulls from | unset (sync off) |
| `REMIND_ME_SYNC_SECRET` | Bearer token for `/sync/push` and `/sync/pull`, sent and required | unset (sync off) |
| `REMIND_ME_SYNC_INTERVAL` | Seconds between background sync cycles | `60` |
| `REMIND_ME_PEER_BIND` | Bind address for this node's own peer server (accepts another node's push/pull) | `0.0.0.0` — all interfaces; narrow to `127.0.0.1` behind a tunnel-only setup |
| `REMIND_ME_PEER_PORT` | Port for the peer server above, and the port every discovered peer is assumed to listen on | `8766` |

`rusty-remind-me configure --node-id ID --hub-url URL [--peer-port N] [--sync-interval SECS]`
writes both the MCP entry and this sync environment for every configured
client in one command. There is deliberately no `--secret` flag — it is read
from `REMIND_ME_SYNC_SECRET` only, so it never lands in argv (`/proc`, shell
history).

### Remote MCP connector (`remind_me_remote`, `rusty-remind-me remote`)

Serves the MCP server over Streamable HTTP instead of stdio, for connectors
that need a network endpoint — e.g. `claude.ai`'s custom connector. Supports
a legacy secret-path/bearer token and, when an issuer is configured, OAuth
2.1.

Speaks both Streamable HTTP lifecycles `rmcp` implements: the
session-managed one every client through protocol version `2025-11-25`
uses (`Mcp-Session-Id`, including `mcp-remote`-fronted clients like Claude
Desktop) keeps working unchanged, and SEP-2567's newer per-request
"discover" lifecycle for `2026-07-28`+ clients, which can call a tool in a
single POST with no `initialize`/session at all. Which one a given request
gets is decided per request from its negotiated protocol version, not by
configuration.

| Variable | Purpose | Default |
| --- | --- | --- |
| `REMIND_ME_REMOTE_MCP` | Enables the connector (`1`/`true`/`yes`) | disabled |
| `REMIND_ME_REMOTE_HOST` | Bind address | `127.0.0.1` |
| `REMIND_ME_REMOTE_PORT` | Bind port | `8768` |
| `REMIND_ME_REMOTE_TOKEN` | Connector bearer token; always wins when set | auto-generated on first use and persisted to disk |
| `REMIND_ME_REMOTE_ISSUER` | Public HTTPS origin for OAuth 2.1 discovery | unset (OAuth off; secret-path/bearer only) |

Loopback by default — expose it through an HTTPS tunnel (e.g. Tailscale
Funnel) rather than widening the bind address directly.

## Search Quality: Embeddings, Reranking & Query Expansion

Three independent, optional layers on top of FTS5 keyword search. Each is
**off by default** and degrades to keyword-only (or its next-cheapest
fallback) on any failure — a missing daemon, an unconfigured model, or a
disabled build feature is never a reason a search should fail.

### Semantic search (`remind_me_core::embedder`)

`REMIND_ME_EMBEDDING_BACKEND` selects the backend; unset, semantic search is
off entirely.

| Backend | `REMIND_ME_EMBEDDING_BACKEND` | Needs |
| --- | --- | --- |
| Ollama | `ollama` | A running Ollama daemon (`REMIND_ME_OLLAMA_URL`, default `http://localhost:11434`; `REMIND_ME_OLLAMA_EMBED_MODEL`, default `nomic-embed-text`) |
| In-process ONNX | `onnx` | Build with `--features remind_me_core/local-embed`; `REMIND_ME_ONNX_MODEL_PATH`/`REMIND_ME_ONNX_TOKENIZER_PATH` pointing at a `.rten` bi-encoder + its `tokenizer.json` — no implicit download, same convention as reranking below |

`REMIND_ME_EMBEDDING_DIM` (default `384`, sized for
`sentence-transformers/all-MiniLM-L6-v2`) must match the configured model's
actual output dimension.

### Cross-encoder reranking (`remind_me_core::reranker`)

Rescores the head of the RRF-ranked list with a cross-encoder for more
precise ordering than keyword/semantic fusion alone. Build with
`--features remind_me_core/rerank`; `REMIND_ME_RERANK` defaults to on but
does nothing until `REMIND_ME_RERANK_MODEL_PATH`/
`REMIND_ME_RERANK_TOKENIZER_PATH` name a `.rten` cross-encoder and its
`tokenizer.json` — same no-implicit-download rule.

### Query expansion (`remind_me_core::query_expansion`)

`REMIND_ME_QUERY_EXPANSION=hyde` enables HyDE (Hypothetical Document
Embeddings): a local LLM writes a short passage that would plausibly answer
the query, and that passage's embedding is fused with the query's own
before the semantic search runs — helps most on questions phrased nothing
like the memory that answers them. Generation uses the same Ollama daemon
the `ollama` embedding backend talks to.

| Variable | Purpose | Default |
| --- | --- | --- |
| `REMIND_ME_QUERY_EXPANSION` | `hyde` to enable | unset (off) |
| `REMIND_ME_HYDE_MODEL` | Ollama model that writes the passage | `llama3.2` |
| `REMIND_ME_HYDE_TIMEOUT` | Seconds before falling back to the plain query | `15` |

### Converting a HuggingFace ONNX export to `.rten`

Both `onnx` embeddings and reranking take `rten`-format models, not raw
ONNX. If a model's HuggingFace repo already ships an `onnx/model.onnx`
export (most `sentence-transformers`/cross-encoder repos do), convert it
directly — no PyTorch, no re-download:

```bash
pip install rten-convert
python -m rten_convert path/to/model.onnx path/to/model.rten
```

On Windows, `rten-convert` 0.22.0's default shape-inference step raises a
`PermissionError` (`tempfile.NamedTemporaryFile` stays open while its own C
extension tries to reopen the same path) — add `--no-infer-shapes` to skip
it; that costs a runtime graph optimization, not correctness.

## Command Line Interface (CLI)

The compiled binary `rusty-remind-me` provides subcommands for interactive management, MCP stdio protocol hosting, and REST API daemon mode:

### 1. Stdio MCP Server (Default)
Starts the stdio JSON-RPC MCP server for integration with MCP clients (Claude Desktop, Antigravity, Cursor, etc.):
```bash
rusty-remind-me server
# OR simply run without arguments:
rusty-remind-me
```

### 2. Auto-Configuration
Configures all installed MCP client applications:
```bash
rusty-remind-me configure
```

`configure` also accepts `--node-id ID --hub-url URL` (writes the sync
environment too — see [Multi-Node Sync, Hub & Remote
Connector](#multi-node-sync-hub--remote-connector) below), plus `--peer-port
N`, `--sync-interval SECS`, `--db-path PATH`, and `--default-format
json|markdown`. The sync secret always comes from `REMIND_ME_SYNC_SECRET`,
never a flag — argv is world-readable via `/proc` and shell history.

### 3. REST API Server Daemon
Starts the async HTTP REST server listening on the specified port (default: 8080):
```bash
rusty-remind-me api 8080
```

### 4. Remote MCP Connector
Serves the MCP server over Streamable HTTP for network-reachable clients
(e.g. `claude.ai`'s custom connector), instead of stdio:
```bash
REMIND_ME_REMOTE_MCP=1 rusty-remind-me remote
```
Off by default; binds `127.0.0.1:8768` unless `REMIND_ME_REMOTE_HOST` /
`REMIND_ME_REMOTE_PORT` are set. See [Multi-Node Sync, Hub & Remote
Connector](#multi-node-sync-hub--remote-connector) above for the full
environment variable reference.

### 5. Adding Memories
Stores a new memory note, fact, or preference in SQLite with automatic FTS5 indexing:
```bash
rusty-remind-me add "User prefers dark mode and Rust for low-level server development"
rusty-remind-me add "Deploy runbook lives in the ops wiki" --category engineering --tags ops,runbook
```

`--category` defaults to `general`; `--tags` is comma-separated and drops blanks. Use `--` before content that starts with dashes.

### 6. Searching Memories
Executes an FTS5 BM25 search with RRF rank fusion and ACT-R vitality scoring:
```bash
rusty-remind-me search "dark mode Rust"
rusty-remind-me search "dark mode Rust" --limit 5 --json
```

Output is Markdown by default — the same rendering `list` uses — with `--json` returning the full results including scores.

### 7. Fetching Memory by ID
Retrieves a specific memory record by its primary key:
```bash
rusty-remind-me get mem_4b307c2bd6ec4deb8c891a4b28cc592a
```

### 8. Knowledge Graph Entity Management
Upserts an entity with optional category/kind classification:
```bash
rusty-remind-me entity "Bailey Robertson" "person"
```

### 9. Markdown Wiki Synthesis
Creates or updates a structured wiki topic page:
```bash
rusty-remind-me wiki-write architecture "System Architecture" "Rusty Remind Me is a high performance memory engine written in Rust."
```

Reads a wiki page by slug:
```bash
rusty-remind-me wiki-read architecture
```

Imports a whole directory of Markdown files into the wiki (recursively):
```bash
rusty-remind-me wiki-import ./wiki-pages
```

Each file's `slug`, `title` and `topic` are read from its YAML front matter,
which is stripped from the stored `content` since those three become columns.
Any field that is absent falls back independently — `title` from the file's
first `# ` heading then its filename, `slug` derived from the resolved title,
`topic` to `general` — so a hand-written note imports without ceremony. The
import upserts on `slug`, so re-importing an updated directory revises pages
in place rather than duplicating them.

This pairs with [`daily-backup-system`](https://github.com/baileyrd/Daily-Backup-System),
whose `dbs export-wiki --out-dir DIR` writes exactly this layout — one page per
backup source and per tag, cross-linked with `[[wikilinks]]`:
```bash
dbs export-wiki --out-dir ./wiki-pages    # in the dbs repo
rusty-remind-me wiki-import ./wiki-pages
```

### 10. System Statistics
Prints total memory counts and database metrics:
```bash
rusty-remind-me stats
```

---

## REST API Endpoints

When running `rusty-remind-me api [port]`, the HTTP server exposes the routes
below (`crates/remind_me_api/src/routes.rs`'s `ROUTES` table is the
authoritative list — this table summarizes it, so check that file directly
for anything not covered here, e.g. the dashboard/knowledge-graph/wiki
surfaces):

| Method | Endpoint | Description |
| ------ | -------- | ----------- |
| `GET` | `/health` | Server health check — `{"status": "ok", "version": "0.1.0"}` |
| `GET` | `/api/stats` | Dashboard-level memory statistics |
| `GET` | `/api/memories` | List memories |
| `POST` | `/api/memories` | Store a new memory (`MemoryAddInput` JSON body) |
| `GET` | `/api/memories/{memory_id}` | Fetch one memory by ID |
| `PUT`, `PATCH` | `/api/memories/{memory_id}` | Update a memory |
| `DELETE` | `/api/memories/{memory_id}` | Delete a memory |
| `GET` | `/api/memories/search?q=...&limit=...` | Search memories — query params, not a POST body |
| `POST` | `/api/memories/bulk/delete`, `/bulk/tag`, `/bulk/reclassify` | Bulk operations over a set of memory IDs |
| `GET` | `/api/vitality` | Vitality report |
| `GET` | `/api/versions` | Version history |
| `GET` | `/api/analytics/trend` | Analytics trend data |
| `POST` | `/api/import` | Import memories |
| `GET` | `/api/export` | Export memories |
| `GET` | `/api/entity`, `/api/entities`, `/api/entity/traverse` | Knowledge-graph entity endpoints |
| `GET` | `/api/wiki`, `/api/wiki/search`, `/api/wiki/load`, `/api/wiki/status`, `/api/wiki/{slug}` | Markdown wiki endpoints |
| `POST` | `/api/wiki` | Create or replace a page — `{"title": ..., "content": ..., "log_note": ...}` (`remind_me_wiki_write`) |
| `DELETE` | `/api/wiki/{slug}` | Delete a page and its index rows (`remind_me_wiki_delete`) |
| `POST` | `/api/wiki/compile` | Compile brief; `{"mark_integrated": true}` advances the watermark (`remind_me_wiki_compile`) |
| `GET` | `/api/wiki/schema` | The maintainer schema the compile brief embeds |
| `GET` | `/api/reminders?when=upcoming\|overdue\|all&limit=...` | Memories with a reminder set, soonest first (`remind_me_list_reminders`) |
| `POST` | `/api/reminders` | Set or clear a reminder — `{"memory_id": ..., "remind_at": ISO-8601 or null}` (`remind_me_set_reminder`) |
| `GET` | `/api/digest?since_days=...` | Recent memories, vitality, reminders and sync state (`remind_me_digest`) |
| `GET` | `/api/status` | Build, database, schema currency, backups and subsystem state (`remind_me_server_status`) |
| `GET` | `/api/saved-searches` | List saved searches (`remind_me_list_saved_searches`) |
| `POST` | `/api/saved-searches` | Create or update one by name (`remind_me_save_search`) |
| `GET` | `/api/saved-searches/{name}/run` | Re-run a saved search's stored query and filters (`remind_me_run_saved_search`) |
| `DELETE` | `/api/saved-searches/{name}` | Delete one, with its watch-tracking rows (`remind_me_delete_saved_search`) |
| `GET` | `/metrics`, `/manifest.json`, `/` | Metrics, PWA manifest, and the dashboard itself |

Each of the reminder, digest, status, saved-search and wiki routes calls the
same `remind_me_core` function its MCP tool does, so a reminder set from the
dashboard and one set by Claude are the same row, validated the same way. The
dashboard drives all of them: a clock button on every memory card, a
**Reminders** view with the upcoming/overdue/all windows, a **Searches** view
that runs and deletes saved searches, and a digest/server-status panel at the
bottom of **Stats**.

### Editing the wiki

The wiki is LLM-curated — Claude synthesises pages from your raw memories — but
you own the vault, so the mechanical operations are yours: the **Wiki** view
can write a page by hand, edit or delete an existing one, and drive a compile
pass. `index.md`, `log.md` and `schema.md` are generated and refused as page
titles; every other title is slugified to `[a-z0-9-]+` before it becomes a
filename, so a page can only ever land inside the wiki directory.

Compiling stays two-phase, because the synthesis in the middle is a judgment
call this server cannot make. `POST /api/wiki/compile` returns a **brief** — the
raw memories since the last watermark, the current page index, and the
maintainer schema — and advances nothing, so it is safe to call repeatedly. The
dashboard shows that brief and offers it for copying into a Claude session
(which then writes the pages with `remind_me_wiki_write`); once the pages
exist, `{"mark_integrated": true}` moves the watermark past that batch. Marking
integrated is not reversible from the UI: those memories will not appear in a
future brief whether they were written up or not.

---

## License

MIT License. See [LICENSE](LICENSE) for details.
