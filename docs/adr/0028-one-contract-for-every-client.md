# ADR-0028: One contract for every client

Status: Proposed (2026-10-26); awaiting owner approval, nothing implemented
Date: 2026-10-26

## Context

remind-me is used by more than Claude. Its clients today, and how each
reaches the store:

| Client | Transport | `client` stamped today | Instructions it gets | Capture at end |
| --- | --- | --- | --- | --- |
| Claude Code | stdio MCP via the plugin, through the daemon | handshake `claude-code/<ver>` | `initialize` text + plugin hooks | plugin hooks (ADR-0026) |
| claude.ai chat and voice | Streamable HTTP (`rusty-remind-me remote`) | **`unknown`**: `remind_me_remote` drops `CallContext`'s client info and never calls `set_handshake_client` | **none**: the remote server is built without `.instructions(...)` | only if the model calls `remind_me_auto_capture`; the user's account preferences carry the rule by hand |
| Hermes harness (JARVIS, dashboard, gateway) | stdio MCP from `~/.hermes/config.yaml` | the Python MCP SDK's handshake name, the same for every Hermes bot | `initialize` text | none |
| Local models on BaileyAi (llama-swap, e.g. Qwen) | through whichever harness hosts them (Hermes today, assumed); llama-swap itself is a model proxy with no tools | the harness's name | the harness's | none |
| Multi-model env (ChatGPT, Gemini) | remote MCP (ChatGPT connectors, Gemini CLI) or REST | `unknown` on remote, the env var on REST | none on remote | none |
| Dashboard, CLI, importers, webhook | REST, CLI | `REMIND_ME_CLIENT` or `unknown` | n/a | n/a |

Rows three to five are partly assumed; the conformance check below is how
each gets confirmed.

What exists already and is kept: `client`, `written_by`, `capture_method` and
the context columns on every row (ADR-0026); per-connection session variables
on the daemon (`REMIND_ME_CLIENT`, `REMIND_ME_TOOL_PROFILE`,
`REMIND_ME_DEFAULT_RESPONSE_FORMAT`); tool profiles `full`, `standard`,
`core` (19 tools), where a hidden tool is also refused on call.

What is wrong:

1. **Identity is configured per process, not per caller.** The remote server
   is one process for every connector, so it has one profile and stamps one
   client (or none). On stdio the handshake beats `REMIND_ME_CLIENT`, so every
   Hermes bot records the same SDK name and an operator cannot tell JARVIS
   from the gateway.
2. **The surface assumes a big model with deferred tool loading.** 74 tools,
   about 17 KB of descriptions and 50 KB of `tools/list`. Small local models
   load all of it up front, and the `core` profile is still 19 tools.
   Descriptions vary from 19 characters (`remind_me_get`) to several hundred.
   Results default to Markdown.
3. **The usage rules live in Claude-only places**: the plugin's hooks and
   commands, and a hand-copied block in the user's claude.ai preferences.
   Remote clients get no instructions at all.
4. **Capture is Claude Code-only.** Every other client depends on the model
   remembering to call a tool.
5. **Nothing checks a client is wired right.** `docs/CUTOVER.md` records two
   consumers (omp, Hermes) missed on a cutover because nothing enumerates them.

## Decision

The contract is the MCP tool table in `remind_me_mcp`, served the same way on
every transport. One dispatch (`McpServer::handle_request`) stays the only
implementation; transports only adapt it and supply the caller.

### 1. Every caller has a registered client id

A client registry, `clients.json` beside the store (settings, not schema):

```json
{
  "claude-code":  { "profile": "core",    "format": "markdown" },
  "claude-ai":    { "profile": "core",    "format": "markdown", "oauth_clients": ["<id>"] },
  "hermes-jarvis":{ "profile": "minimal", "format": "json" },
  "qwen-baileyai":{ "profile": "minimal", "format": "json" }
}
```

- **Id** is short, stable and lowercase (`[a-z0-9-]{1,40}`), validated at
  load. It names *who is calling* (a bot, an app install), not the SDK.
- **Resolution, first match wins:** the credential (an OAuth client or a REST
  API key mapped in the registry), then `REMIND_ME_CLIENT` from the caller's
  own config, then the handshake name if the registry lists it as an alias,
  then `unknown`. This flips today's handshake-first rule: the handshake names
  the app, and several bots share one app. The handshake string is still
  recorded, in `metadata.client_app`, so nothing observed is lost.
- **Per caller, not per process.** The remote adapter passes `CallContext`
  through, so each remote call resolves its own id, profile and format. The
  same for REST.
- **`unknown` is allowed, then refused.** Phase 1 accepts it and counts it in
  `remind_me_server_status`. Once every known client is registered,
  `REMIND_ME_REQUIRE_CLIENT=1` refuses writes from `unknown` with an error
  naming the fix. Reads stay open.
- **Filter by it.** `remind_me_search` and `remind_me_list` gain `client`,
  beside the existing `written_by`.

### 2. Profiles are per client, smallest first

| Profile | For | Tools |
| --- | --- | --- |
| `minimal` | small local models | `search`, `get`, `add`, `auto_capture`, `brief` (#62) |
| `core` | Claude and other large models | `minimal` + `update`, `resolve`, `entity`, `server_status`; the final list comes from #54 |
| `standard`, `full` | as today | unchanged |

Admin, sync, import and pipeline tools are opt-in groups added to a profile
in the registry. Which tools exist and how they merge is #54's decision; this
ADR fixes only that the profile is chosen per client.

### 3. Rules every tool in `minimal` and `core` meets

Checked in CI by a test over the tool table, so a regression fails the build:

- Name `remind_me_<verb>` (#52). Description at most 160 characters, leading
  with a verb phrase that says "remind-me" and the noun.
- Input schema strict: `additionalProperties: false`, closed sets as `enum`,
  at most one required field in `minimal` (`query`, `text` or `id`), no nested
  objects in `minimal`.
- Results are JSON: `{"items":[{"id","type","text","at","score"}],"next"}`
  with `text` cut to a budget and full text through `get` (#51). Markdown
  stays available as a rendering, chosen per client by `format`; JSON is the
  contract, Markdown is a view of it.
- Errors are `{"error":{"code","message","fix"}}`, with `fix` saying what to
  send instead.
- Nothing names a client: no "Claude", no "Claude Code hook", no deferred
  tool loading in descriptions or results.

### 4. One usage guide, served by the server

`crates/apps/rusty_remind_me/GUIDE.md` is the single source, compiled into the
binary. Its rules are the ones already proven in practice: search before
answering about the user, save each durable fact as you go (one fact, typed,
a decision with its rationale), close decisions with `resolve`, capture the
conversation at the end, never store secrets. It is served as:

- the MCP `initialize` instructions, on stdio **and on remote**;
- an MCP resource, `remind-me://guide`, and `GET /api/guide`;
- `rusty-remind-me guide [--client ID]`, which prints the text for pasting
  into a system prompt that has no MCP instructions (Hermes personas, local
  models, the multi-model env), with only the tools that client's profile has.

Client-side instruction files point at it instead of restating it: the user's
global `CLAUDE.md`, claude.ai preferences, Hermes persona files. This repo's
`CLAUDE.md` is about commit signing and is not touched.

### 5. Capture has one input and a path per client

One neutral input, the one `remind_me_auto_capture` already takes:
`{session_id, dialog: [{role, text}], summary?}`, stamped with the resolved
client.

| Client | Path |
| --- | --- |
| Claude Code | plugin hooks, unchanged |
| claude.ai chat, voice | the model calls `remind_me_auto_capture`, told to by the guide in `initialize` (today it is told nothing) |
| Hermes and the local models it hosts | Hermes's session-end runs `rusty-remind-me capture-transcript --format messages`, which reads the neutral input on stdin |
| Multi-model env | its orchestrator calls `remind_me_auto_capture` (MCP) or `POST /api/capture` (REST) at the end of a run |

`--format messages` and `POST /api/capture` are built only when Hermes or the
multi-model env is wired, so each has a real caller.

### 6. A conformance check every client can run

`rusty-remind-me conformance --client ID (--stdio CMD | --url URL)` connects as
that client over its own transport and reports pass or fail per line:

1. `initialize` returns the guide.
2. `tools/list` equals the client's profile, and every tool meets section 3.
3. `add` then `search` then `get` round-trips one memory tagged
   `conformance`, which is then deleted.
4. That memory was stamped with the expected client id, not `unknown`.
5. A capture dry run is accepted.

Exit 0 only if all pass. Run after installing or changing any client, and
listed in `docs/CUTOVER.md` as the step that would have caught omp and Hermes.

## Consequences

- One person reading the store can tell every bot apart, and a profile or
  format is a registry line, not a process.
- Small models see five tools and short JSON. Large models lose nothing:
  `full` stays.
- Flipping identity precedence changes the `client` value on new rows for any
  stdio client that also sets `REMIND_ME_CLIENT`. Old rows are untouched.
- `REMIND_ME_REQUIRE_CLIENT=1` will break any client not registered; it stays
  off until `conformance` passes for each one.
- No schema change and no migration: `client` exists, `client_app` lives in
  `metadata`, the registry is a settings file.

## Order of work

Each step is its own PR and shippable alone.

1. Remote passes caller identity and serves the instructions (the two bugs in
   the table). Small, no contract change.
2. Client registry, resolution order, `client` filter, `unknown` counter.
3. Per-client profile and format; the `minimal` profile; the CI test of
   section 3 (with #52 and #54).
4. `GUIDE.md` and its four surfaces; point the user's instruction files at it.
5. `conformance`.
6. Capture for Hermes and the multi-model env, as each is wired.
7. `REMIND_ME_REQUIRE_CLIENT=1`, once every client passes `conformance`.

## Relation to other cards

- #52 (discoverable names): its naming rules become section 3's.
- #54 (slim the surface): decides the tool list and merges; this ADR makes
  the profile per client and adds `minimal`.
- #62 (`remind_me_brief`): is in `minimal`, returns the section 3 JSON with a
  per-client token budget from the registry.
- #56 (session logs through the refinement ladder): consumes captures from
  every client, not only Claude Code.

## Not done

- Per-client read scoping (a client seeing only its own memories). The store
  is one person's; nothing needs it yet.
- Routing small models to a different ranking or a smaller embedding.
- Authenticating stdio callers. A local process with the binary already has
  the store.
