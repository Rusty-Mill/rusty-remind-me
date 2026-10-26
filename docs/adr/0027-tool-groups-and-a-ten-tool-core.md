# ADR-0027: Tool groups and a ten-tool core

Status: Accepted (2026-10-26); step 1 implemented
Date: 2026-10-26

## Context

The MCP server advertised 76 tools. A typical session uses about eight.
Claude clients defer tools and load them by keyword search over name and
description, so a large surface makes the right tool hard to find: a search
for `remind_me_search` returned `remind_me_delete_saved_search` first, and no
description contained the words "remind-me". Clients that load every tool up
front (Hermes, local models, ChatGPT, Gemini) pay for all 76 schemas each
session, and small models choose worse among near-identical names.

Profiles already existed (`REMIND_ME_TOOL_PROFILE` = `full`, `standard`,
`core`), but the default was `full`, `core` held 19 tools, and any tool
outside two hand-kept lists was treated as admin implicitly.

Per-tool call counts are not persisted (the `remind_me_tool_calls_total`
counter lives in memory), so the core set was chosen from the server
instructions and the clients' standing instructions, not from usage data.

## Decision

1. **Every public tool belongs to exactly one group**: `core`, `pipeline`,
   `wiki`, `import`, `admin`, `extras` (`tool_profiles::GROUPS`). A test
   asserts each advertised tool is in exactly one, and that every handler is
   either a public tool or an alias.
2. **Core is ten tools**: search, get, add, update, delete, auto_capture,
   resolve, entity, digest, server_status.
3. **`REMIND_ME_TOOL_PROFILE` takes a comma list of groups**, such as
   `core,wiki`. Core is always on. `full` is every group; `standard` keeps
   its old meaning as `core,pipeline,wiki`. An unknown part falls back to
   `full` with a warning, as before.
4. **Look-alikes merge into one tool that routes on an argument**
   (`remind_me_mcp::surface`):
   - `search`: no `query` lists (was `list`); `reference` looks up
     references (was `references`).
   - `get`: `capture_id` (was `get_capture`), `history=true` (was
     `history`), `raw=true` (was `source`).
   - `update`: `remind_at` alone (was `set_reminder`).
   - `digest`: `section=stats|vitality` (was `stats`, `vitality_report`).
   - `server_status`: `section=watcher|webhook|updates|connectors` (was
     `watch_status`, `webhook_status`, `check_update`, `list_connectors`).
   - `queue(kind)`: the eight `*_batch` and `*_candidates` fetchers.
   - `wiki_read`: `slug`, `query`, `all=true` or nothing (was `wiki_read`,
     `wiki_search`, `wiki_load`, `wiki_list`).
   - `import(from, path)`: file, directory, dbs, mempalace, wiki (was five
     import tools).
   - `reminders`, `saved_search(action)`, `sync_status(reconcile)`.

   A merged schema is the union of its handlers' properties; a property
   whose handlers disagree on a default advertises none. 76 tools become 44;
   `self_update` leaving MCP (step 2) makes it 43.
5. **Apply tools with different item schemas stay separate** (`decompose`,
   `annotate`, `normalize_apply`, `reclassify`). One tool with a `oneOf`
   union would suit small models worse than four strict schemas.
6. **Handlers keep their names, and old names stay callable as aliases**
   until 0.6.0. An alias is never listed, reaches its own handler with its
   old arguments, and is allowed exactly when its replacement's group is, so
   a hidden group stays hidden under its old names. First use in a process
   is logged to stderr; every call is counted under the name it was made
   with.
7. **Descriptions lead with "remind-me"**: core tools in their first
   sentence, every other tool with its group label (`remind-me wiki: ...`).
8. **`resolve` gains `carded`**: a decision moved to a board card is no
   longer open but not done, and keeps its full weight.
9. **One binary, no new crates.** `remind_me_core` is not split here
   (ADR-0018). The handler catalog stays one `json!` literal for now; it
   shrinks when the aliases go.

## Rollout

1. Groups, merges, aliases, descriptions, `carded`. The default stays
   `full`, so no call that worked before fails.
2. After about a week on a live node: the default becomes `core`, admin
   operations gain CLI subcommands, `self_update` becomes CLI-only.
3. 0.6.0: aliases are removed once their calls stop.

## Consequences

- Under `full` a client sees 44 tools instead of 76; after step 2, a client
  that sets nothing sees 10.
- 36 tool names are deprecated. Anything that calls them keeps working until
  0.6.0.
- Per-client profiles keyed by client id are left to the multi-client
  contract; this ADR gives them the groups to select from.
- The refusal for a hidden tool names its group and the setting that turns
  it on, and `server_status` reports the active profile and groups.
