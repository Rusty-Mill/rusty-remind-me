# rusty-remind-me — spec index
Design chat: unknown (harvest; no single design chat) · remind-me tag `rusty-remind-me`
Harvested: 2026-11-16 @ 94699a6
Owner: Nano (`baileyrd`) — the person whose words count as `[stated]`
Tags: [stated] [default] [open]. Pointers only: open the listed docs at call time.

## Intent
Persistent memory that every model, agent and bot shares, so all of them work from
one knowledge base (D1 [stated]). One remote MCP server holds one store (ADR-0032).

## Scope
- In: memory store and search (keyword, optional semantic), vitality decay, entities
  and graph, projects, decisions and follow-ups, captures and session logs, the wiki,
  imports, the MCP surface (stdio and remote), the REST API and dashboard.
- Out: multi-node sync and the hub (ADR-0032); the Python `remind_me` and its schema
  (ADR-0023, ARCHITECTURE tenet 3); Mill Board and rusty_tick (own projects since
  2026-10-28); programme order and phase gates (Millwright, rusty_mill
  `docs/projects/millwright/`, `millwright-expert`).

## Constraints
- Storage: refactor first, choose the store at the R3 benchmark (D2 [stated]).
- Ask before schema or data migrations, public API changes, deletions and
  dependency or toolchain bumps (D3 [stated]).

## Shape
Five crates: `remind_me_core` (domain and store, blocking), `remind_me_mcp` (stdio
JSON-RPC), `remind_me_api` (hand-rolled HTTP), `remind_me_remote` (Streamable HTTP
connector, the only async crate), `remind_me_cli` (binary `rusty-remind-me`).
Store: `rusty_multimodal_db` engine in-process, behind one port per table group
(ADR-0035). ARCHITECTURE.md § 2 is authoritative.

## Index
| Subsystem | Docs | Owner | Status | Expert |
|---|---|---|---|---|
| Overall shape, tenets, crates | ARCHITECTURE.md, README.md, CONTRIBUTING.md | @baileyrd | active | this one |
| Store and storage port | docs/adr/0021-0025, 0035; docs/design/dbs-modularity-audit.md | @baileyrd | active | this one |
| Store choice (R3 benchmark) | rusty_mill docs/projects/millwright/ROADMAP.md § remind-me, DECISIONS.md `remind-me-order` | @baileyrd | active | this one (order: millwright-expert) |
| Sync and hub (removed) | docs/adr/0004-0007, 0013, 0015, 0021, 0032 | @baileyrd | phase 2 open | this one |
| Search, vitality, embeddings | docs/adr/0002, 0034; ARCHITECTURE.md § 3-4 | @baileyrd | active | this one |
| MCP surface and clients | docs/adr/0010, 0017, 0028, 0030 | @baileyrd | active | this one |
| Remote connector and OAuth | docs/adr/0010, 0011; crates/remind_me_remote/src/lib.rs module doc | @baileyrd | active | this one |
| Projects, decisions, captures | docs/adr/0026, 0027, 0029, 0031; docs/routines/session-ladder.md | @baileyrd | active | this one |
| Wiki | docs/adr/0019, 0033; docs/design/topic-taxonomy.md | @baileyrd | active | this one |
| Ops: dashboard, updates, telemetry | docs/adr/0003, 0008, 0009, 0012, 0014; docs/CUTOVER.md | @baileyrd | stable | this one |
| Optional features in core | docs/adr/0018 | @baileyrd | stable | this one |
| Rust conventions and traps | docs/handoff/DOMAIN.md; docs/handoff/sources/rust-adherence.md (not binding) | @baileyrd | active | this one |

Decision log: `docs/adr/` (ADR-0001 to ADR-0035) plus `DECISIONS.md` here.
