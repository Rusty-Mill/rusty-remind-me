# Implementer Q&A — rusty-remind-me
Seeded only with questions the sources settle, each citing its source. The expert adds the rest.

## Store
- **Q:** Which store does the node run on? **A:** the in-process `rusty_multimodal_db` engine; the SQLite runtime store is gone and SQLite is read once, on first open of an old `memory.db`. [default] (ADR-0025, ARCHITECTURE.md § 2)
- **Q:** Can I swap the store now? **A:** No. The port comes first, then the R3 benchmark picks the store. [stated] (D2)
- **Q:** I need a new per-memory field. **A:** Memory rows are bincode-positional, so a column is a new record layout and a one-way upgrade: ask first (D3). Prefer the `metadata` JSON. [default] (ADR-0023; D4)
- **Q:** Where does a new table's storage code go? **A:** a port trait in `db::<group>` with an engine adapter and a conformance suite. [default] (ADR-0035)

## Surface
- **Q:** Can I rename or reshape an MCP tool? **A:** No without asking: clients rely on names and signatures. [default] (ARCHITECTURE.md tenet 3; D3 public API)
- **Q:** Which tools are in core? **A:** eleven: search, get, add, update, delete, auto_capture, resolve, entity, digest, server_status, brief. [default] (ADR-0030)
- **Q:** Is HTTP writing open without a key? **A:** No: every mutating method is refused with 401 while `REMIND_ME_API_KEY` is unset. [default] (crates/remind_me_api/src/lib.rs module doc)

## Runtime
- **Q:** May I add async to core? **A:** No; only `remind_me_remote` is async. One async call in core builds a current-thread runtime inside that function. [default] (ARCHITECTURE.md tenet 4; cloud_backup.rs)
- **Q:** Is there multi-node sync? **A:** No. Phase 1 removed the transport; outbox, cursors and tombstones remain until phase 2. [default] (ADR-0032)
