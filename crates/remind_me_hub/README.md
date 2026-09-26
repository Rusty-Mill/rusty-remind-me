# remind-me sync hub

The central sync point for the `remind_me` distributed sync engine: nodes push
to it and pull from it, and it never pulls from them. A port of the reference's
`hub/main.py`, serving the same ten routes over the same wire protocol.

Hub and peer are the same protocol against different topologies — a node's own
peer server (`remind_me_core::sync::server`) answers seven of these routes from
its SQLite database. A client cannot tell a hub from a peer, which is the point.

## Quick start

Rootless Podman, one container, no database server:

```sh
crates/remind_me_hub/setup.sh install
```

It prints the generated `SYNC_SECRET` that clients need. Re-running `install`
keeps an existing secret and data. Then, on each client machine:

```sh
crates/remind_me_hub/client-setup.sh --node-id my-laptop --tunnel me@hub-host
```

That prompts for the secret, sets up an SSH tunnel, and calls
`rusty-remind-me configure` to write the MCP entry — sync environment included
— for every client. For a node that needs no tunnel, `configure` alone is
enough:

```sh
REMIND_ME_SYNC_SECRET=... rusty-remind-me configure \
    --node-id my-laptop --hub-url http://127.0.0.1:8765
```

The secret is read from the environment and has no flag: argv is world-readable
through `/proc` and lands in shell history. `configure` also refuses a partial
triple — node id, hub URL and secret, or none of them — because sync silently
does nothing unless all three are set.

Other deployments — Docker Compose, Fly, Railway — are in [`deploy/`](deploy/).

## Running it directly

```sh
SYNC_SECRET=$(openssl rand -hex 32) \
REMIND_ME_HUB_DATA_DIR=./hub-data \
  cargo run -p remind_me_hub --bin rusty-remind-me-hub
```

## Configuration

| Variable | Default | Meaning |
| --- | --- | --- |
| `SYNC_SECRET` | — | Shared bearer token. **Required**; the hub refuses to start without it. |
| `REMIND_ME_HUB_DATA_DIR` | — | The store's data directory. **Required**. |
| `REMIND_ME_HUB_COMPACT_INTERVAL_SECS` | `3600` | How often the store folds its insert logs. |
| `REMIND_ME_HUB_BIND` | `127.0.0.1` | Listen address. The image sets `0.0.0.0`. |
| `REMIND_ME_HUB_PORT` | `8765` | Listen port. |
| `REMIND_ME_HUB_METRICS_ENABLED` | off | Serve `GET /metrics`. Off returns 404. |
| `REMIND_ME_HUB_TOMBSTONE_RETENTION_DAYS` | `90` | Age past which `/admin/compact_tombstones` hard-deletes. |

An unset `REMIND_ME_HUB_DATA_DIR` is an error rather than a default: a hub that
quietly created an empty store would look healthy while serving nothing.

`DATABASE_URL` and `REMIND_ME_HUB_DB_PATH` selected the Postgres and SQLite
stores, which are gone. The hub refuses to start with either set, even beside
`REMIND_ME_HUB_DATA_DIR`, and says how to copy the old store over: an empty
engine in front of data that was never copied would hide it.

### The store

The hub runs on `rusty_multimodal_db`'s storage engine in-process
(`docs/adr/0021`): one data directory, no database server. It:

- keeps the whole dataset in memory, and writes each change to a per-table
  insert log that is `fsync`'d before the push returns;
- refuses ids over 64 bytes or containing a NUL byte. Such a record counts as
  `failed` and stays in the sender's outbox;
- applies a push in chunks of 64 records, each under one sync, and a pull
  waits for at most one chunk. In `examples/pull_latency.rs` it took pushes
  of 100 records about six times as fast as the SQLite store it replaced,
  with a pull median of 4 ms against SQLite's 47 ms;
- pauses writes briefly each time a table's row count passes a power of
  two, while an in-memory map regrows: under 10 ms at 115 000 memories,
  37 ms at 229 000;
- after a failed `fsync`, refuses every write and fails `/health` until
  restarted;
- folds the insert logs every `REMIND_ME_HUB_COMPACT_INTERVAL_SECS`, and on
  every `/admin/compact_tombstones`;
- locks its data directory, so a second hub pointed at it refuses to start.
  Back it up by copying the directory while the hub is stopped.

`GET /count?approx=1` asks for planner estimates, which the engine has no
equivalent for. Rather than label a full scan "approximate", the hub answers
exact counts and reports `approximate: false`.

### Moving a hub onto the engine

A hub that ran on the Postgres or SQLite store moves over with
`rusty-remind-me-hub-copy`, which copies either into a new data directory.

**A `setup.sh` hub:** run `setup.sh migrate`. It builds the new image, stops
the hub, copies the old store onto the engine, rewrites `hub.env` (keeping
the old one as `hub.env.pre-engine`), installs the one-container unit, and
starts the hub. The Postgres container and its data, or the SQLite file,
are left in place for you to remove once the hub looks right. If the copy
lists rows the engine cannot store, `setup.sh migrate --drop-invalid` copies
everything else.

**Anything else:** stop the hub and run the tool; the source is only ever
read. It ships in the hub image and in the release archives. From source:

```sh
cargo build -p remind_me_hub --bin rusty-remind-me-hub-copy

rusty-remind-me-hub-copy --from-sqlite ./hub.db --to ./hub-data --check
rusty-remind-me-hub-copy --from-sqlite ./hub.db --to ./hub-data
DATABASE_URL=postgresql://… rusty-remind-me-hub-copy --from-postgres --to ./hub-data
```

Then replace `DATABASE_URL` or `REMIND_ME_HUB_DB_PATH` in the hub's
environment with `REMIND_ME_HUB_DATA_DIR`, pointing at the copy.

- **Every `hub_seq` and `origin_node` is kept**, so nodes carry on from their
  cursors and `exclude_node` still means what it did. The next `hub_seq` is
  issued above the highest the source ever handed out, not only above its
  remaining rows: for Postgres that is the sequence's last value, for SQLite
  the mark it keeps in `hub_meta`.
- **Rows the engine cannot store are listed, and nothing is written**: ids
  over 64 bytes or holding NUL, and rows the reader could not parse. `--check`
  lists them without writing; `--drop-invalid` copies everything else.
- **A legacy Postgres database** (the Python hub's `TIMESTAMPTZ` schema) is
  read as it stands. It is not migrated first.
- **The target must be empty or absent.** After writing, the tool reads every
  row back and compares it with the source.
- **`--from-postgres` reads `DATABASE_URL`**, so the password never appears
  on the command line.

The readers stay in later releases (the Postgres one behind the default
`postgres-import` feature), so a hub that migrates late is not stranded.

## Routes

| Route | Auth | Purpose |
| --- | --- | --- |
| `GET /health` | none | Liveness. 200 when the store can take writes, 503 when not. |
| `GET /stats` | bearer | Full aggregate — once per reconcile. |
| `GET /count` | bearer | Scalar counts, cheap enough to poll. `?table=`, `?since=`, `?by=origin_node\|category`, `?approx=1`. |
| `GET /metrics` | bearer | Prometheus text. 404 when disabled. |
| `POST /admin/compact_tombstones` | bearer | Hard-delete expired tombstones. |
| `POST /sync/push` | bearer | Upsert a batch. LWW on `updated_at`. |
| `GET /sync/pull` | bearer | Memory records since a cursor. |
| `GET /sync/pull_entities` | bearer | Entity records. |
| `GET /sync/pull_links` | bearer | Memory↔entity links. |
| `GET /sync/pull_entity_relations` | bearer | Typed entity edges. |

`/health` is unauthenticated on purpose: it is what a deploy healthcheck polls,
and it must keep answering when the store is down. It carries no counts. Its
`db` field never echoes the underlying error — that goes to the log instead.

Every response carries `X-Hub-Version`, errors included, so "which build
answered this?" never needs a second request.

## `origin_node`, and why pull filters on it

`origin_node` records *which node pushed* a record. It never leaves the hub —
no wire format includes it.

`GET /sync/pull?exclude_node=X` filters on `origin_node`, not on the record's
own `node_id`. That difference is load-bearing: a client never rewrites
`node_id` on update, so filtering on it would make a record's creator deaf to
every later edit anyone else pushed. Peers compensate by pushing to each other;
a hub is pull-only, so it must track pushers itself.

`?full=1` drops the filter entirely, so a node that lost its database can
re-seed everything it originally authored — normally unreachable, precisely
because `exclude_node` always excludes a node's own pushes.

## Cursors

Three modes, in order of preference:

- **`since_seq`** — keyset on the hub-assigned, monotonic `hub_seq`, bumped on
  every write regardless of the record's own client-authored `updated_at`.
- **`since` + `since_id`** — legacy `(updated_at, id)` keyset.
- **`since` alone** — legacy strict `updated_at >`.

`since_seq` exists because the timestamp cursors have a real failure: a node
back online after a fortnight pushes records still stamped with old
timestamps, which sort *behind* an already-advanced cursor and are then
permanently invisible to everyone else. `updated_at` still drives LWW; this
only changes what the pull cursor orders on.

## Version

`HUB_VERSION` in `src/lib.rs` is a hand-maintained literal — the image holds a
binary with no manifest to derive one from. Bump it (semver) whenever
observable behaviour changes: MAJOR for a wire break, MINOR for a new endpoint
or response field, PATCH for a fix nothing can key off. `setup.sh` reads it
from the source and passes it to the build as `HUB_VERSION`, so the image label
and the running hub cannot disagree for an image built the documented way.

Clients that need to know whether a capability exists should probe for the 404
rather than compare versions. This is a diagnostic, not a feature-negotiation
channel.

## Operating

```sh
crates/remind_me_hub/setup.sh status          # unit, health, per-node counts
crates/remind_me_hub/setup.sh update          # pull, rebuild, restart, verify
crates/remind_me_hub/setup.sh restore d.sql   # load a Postgres dump
crates/remind_me_hub/setup.sh migrate         # move off Postgres or SQLite
```

`update` checks that the *new build is actually serving* rather than only that
the service restarted — a rebuilt image the unit never picked up leaves a
perfectly healthy old hub answering, which reads as success. On a hub still
on Postgres or SQLite it refuses, and points at `migrate`.

`restore` loads a Postgres dump, a Python hub's legacy one included, into a
throwaway Postgres container, copies it onto the engine with the copy tool,
and swaps it in. A hub that already holds memories needs `--force`, and the
data it replaces is moved aside rather than deleted.

Tombstone compaction is operator-triggered (a cron hitting
`/admin/compact_tombstones`) rather than a background loop, since the hub has
no periodic-task infrastructure to hang one off. It is purely time-based, with
no per-node cursor tracking — a node offline longer than the retention window
can miss a delete, the same accepted gap the client-side compaction lives with.

## Security posture

- **`SYNC_SECRET` is the only thing between the internet and the whole memory
  database.** Reach the hub over an SSH tunnel or Tailscale; every deploy
  template binds to loopback or a private address.
- Bearer comparison is constant-time over bytes, and an unset secret rejects
  every request rather than accepting an empty bearer.
- There is no OpenAPI or docs route to disable. The reference spends real
  effort turning FastAPI's three off, because they default to on and
  unauthenticated and would publish every route — including the one that
  hard-deletes rows. Here a route that was not written does not exist.
- Data is stored plaintext. Encryption at rest is the storage layer's job:
  full-disk encryption or your provider's volume encryption.

## Testing

```sh
cargo test -p remind_me_hub                       # Postgres copy tests skip
REMIND_ME_HUB_TEST_DATABASE_URL=postgresql://… \
  cargo test -p remind_me_hub -- --test-threads=1 # with a real Postgres
```

The route suite (`tests/suite/routes.rs`) covers the protocol request by
request. `tests/suite/recorded.rs` pushes one script and requires every read
to answer as the retired SQLite store did; those answers were recorded before
it went (`tests/fixtures/README.md`). The copy tests rebuild SQLite and
Postgres hubs from dumps those stores wrote, and hold each copy to the
store's recorded answers.

The Postgres copy tests skip when no database is configured — and it is worth
being precise about what that means: a skipped test reports as **passed**, and
cargo hides the `SKIP` line unless you pass `--nocapture`. Locally that is
fine. For CI it is not, so `REMIND_ME_HUB_REQUIRE_POSTGRES=1` turns the skip
into a hard failure and CI sets it; the environment cannot lose its database
and stay green.

CI runs all of these, plus a `--no-default-features` build, so a
`postgres::` reference leaking outside its feature gate cannot go unnoticed.
