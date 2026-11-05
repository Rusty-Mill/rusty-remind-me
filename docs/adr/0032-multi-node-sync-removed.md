# ADR-0032: Multi-node sync and the hub are removed

Status: Accepted (2026-11-05); phase 1 implemented
Date: 2026-11-05

Supersedes ADR-0004 (sync protocol), ADR-0005 (graph sync), ADR-0006 (peer
discovery), ADR-0013 (sidecar teardown), ADR-0015 and ADR-0021 (hub storage).

## Context

remind-me runs as one remote MCP server with no local instances (Mill Board
card #746). The hub, the push/pull client, peer discovery, the peer server,
the background sync worker and the tunnel/UI sidecars it supervised served
only a fleet of nodes, and there is none. They were the largest subsystem
left in the workspace: a sixth crate, eleven modules in `remind_me_core`, two
MCP tools and their aliases, `configure` flags and two CI jobs.

Sync also left state in each store: the `sync_enabled` flag, the outbox the
repositories queue on every write while it is on, send markers and pull
cursors, and tombstones in place of hard deletes.

## Decision

1. **Phase 1 removes the transport.** Gone: `remind_me_hub` (both binaries),
   `sync::{push, pull, peers, server, worker, status, reconcile, record,
   graph}`, `db::sync_feed`, `sidecars`, `remind_me_sync_status`,
   `remind_me_sync_repair` and the `sync_reconcile*` aliases, the `sync`
   entries in `remind_me_server_status`, the digest and `/api/versions`, the
   outbox gauge, and the `configure` sync flags, which now say why they went.
2. **A store's data does not change.** Nothing turns sync on any more: a
   fresh store, or one whose flag is off, is set off at open. A store whose
   flag is on keeps it, so its writes still queue outbox rows (pruned by the
   retention window) and its deletes still tombstone, until
   `REMIND_ME_SYNC_DISABLE=1` turns it off and empties the outbox. That is
   the same explicit switch as before, so no open of a real store rewrites
   anything on its own.
3. **Provenance stays.** `REMIND_ME_NODE_ID` and `REMIND_ME_CLIENT` are
   stamped on every memory, as they always were with or without sync; they
   live in `sync` until phase 2 empties that module.
4. **Phase 2 retires the storage.** The outbox, send markers, pull cursors,
   `Origin::Sync` write paths and the tombstone switch go together, on their
   own card, because they reach the engine's tables and the delete path.

## Consequences

- The store daemon, the dashboard and the remote connector no longer start
  or report a sync worker or peer server.
- A store that synced keeps queueing outbox rows nothing drains; the
  retention window bounds them. Turning that off is one environment
  variable, chosen by whoever runs the store.
- Host configuration (the hub unit, its tunnel, `node.env` holding the
  dashboard's API key) is separate from this change and moves first.
