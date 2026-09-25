# remind-me sync hub

The central sync point for the `remind_me` distributed sync engine: nodes push
to it and pull from it, and it never pulls from them. A port of the reference's
`hub/main.py`, serving the same ten routes over the same wire protocol.

Hub and peer are the same protocol against different topologies — a node's own
peer server (`remind_me_core::sync::server`) answers seven of these routes over
SQLite. A client cannot tell a hub from a peer, which is the point.

## Quick start

Rootless Podman, one container, no database server (the embedded engine):

```sh
crates/remind_me_hub/setup.sh install
```

With Postgres in a second container, or on SQLite instead:

```sh
crates/remind_me_hub/setup.sh --postgres install
crates/remind_me_hub/setup.sh --sqlite install
```

Each prints the generated `SYNC_SECRET` that clients need. Re-running
`install` on a machine that already has a hub keeps that hub's store: an
existing `~/remind-me-hub/hub.env` decides, whatever the default or the flags
say. Then, on each
client machine:

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
REMIND_ME_HUB_DB_PATH=./hub.db \
  cargo run -p remind_me_hub --bin rusty-remind-me-hub
```

## Configuration

| Variable | Default | Meaning |
| --- | --- | --- |
| `SYNC_SECRET` | — | Shared bearer token. **Required**; the hub refuses to start without it. |
| `DATABASE_URL` | — | Postgres connection string. Selects the Postgres backend. |
| `REMIND_ME_HUB_DB_PATH` | — | SQLite file. Selects the SQLite backend. |
| `REMIND_ME_HUB_DATA_DIR` | — | Data directory. Selects the embedded-engine backend, the default for a new hub. |
| `REMIND_ME_HUB_COMPACT_INTERVAL_SECS` | `3600` | How often the embedded-engine backend folds its insert logs. |
| `REMIND_ME_HUB_BIND` | `127.0.0.1` | Listen address. The image sets `0.0.0.0`. |
| `REMIND_ME_HUB_PORT` | `8765` | Listen port. |
| `REMIND_ME_HUB_METRICS_ENABLED` | off | Serve `GET /metrics`. Off returns 404. |
| `REMIND_ME_HUB_TOMBSTONE_RETENTION_DAYS` | `90` | Age past which `/admin/compact_tombstones` hard-deletes. |
| `REMIND_ME_HUB_STATEMENT_TIMEOUT_MS` | `15000` | Postgres statement timeout. |

Exactly one of `DATABASE_URL`, `REMIND_ME_HUB_DB_PATH` and
`REMIND_ME_HUB_DATA_DIR` must be set. More than one is an error, and so is none
— a hub that quietly created an empty SQLite file because `DATABASE_URL` was
misspelled would look healthy while serving nothing.

### The embedded-engine backend (the default)

`REMIND_ME_HUB_DATA_DIR` runs the hub on `rusty_multimodal_db`'s storage engine
in-process (`docs/adr/0021`): one data directory, no database server. It is
built in by default (the `multimodal-store` feature, through
`postgres-import`), and `setup.sh` and `deploy/docker-compose.engine.yml` use
it for a new hub. The Postgres and SQLite stores are still built in, and
existing deployments keep using them; they are removed in a later release.
Compared with SQLite it:

- keeps the whole dataset in memory, and writes each change to a per-table
  insert log that is `fsync`'d before the push returns;
- refuses ids over 64 bytes or containing a NUL byte. Such a record counts as
  `failed` and stays in the sender's outbox;
- makes a pull wait for at most one push being written. Pulls are faster than
  SQLite's under push load (3 ms against 27 ms median in
  `examples/pull_latency.rs`); contended pushes are slower, since each write
  syncs twice;
- folds the insert logs every `REMIND_ME_HUB_COMPACT_INTERVAL_SECS`, and on
  every `/admin/compact_tombstones`;
- locks its data directory, so a second hub pointed at it refuses to start.
  Back it up by copying the directory while the hub is stopped.

### Moving a hub onto the engine

`rusty-remind-me-hub-copy` copies a SQLite or Postgres hub into a new data
directory. Stop the hub first; the source is only ever read.

The tool ships in the hub image and in the release archives. From source:

```sh
cargo build -p remind_me_hub --bin rusty-remind-me-hub-copy

rusty-remind-me-hub-copy --from-sqlite ./hub.db --to ./hub-data --check
rusty-remind-me-hub-copy --from-sqlite ./hub.db --to ./hub-data
DATABASE_URL=postgresql://… rusty-remind-me-hub-copy --from-postgres --to ./hub-data
```

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

A `setup.sh` hub on SQLite moves over like this. The volume is the one its
unit already mounts, so the copy lands beside the old file, which stays as it
was:

```sh
systemctl --user stop remind-me-hub
podman run --rm -v ~/remind-me-hub/data:/data:Z,U localhost/remind-me-hub \
    rusty-remind-me-hub-copy --from-sqlite /data/hub.db --to /data/hub
sed -i 's|^REMIND_ME_HUB_DB_PATH=.*|REMIND_ME_HUB_DATA_DIR=/data/hub|' ~/remind-me-hub/hub.env
systemctl --user start remind-me-hub
```

A Postgres hub does the same with `--from-postgres`, run on the `remind-me`
network with `DATABASE_URL` from its `hub.env`
(`podman run --rm --network remind-me --env-file ~/remind-me-hub/hub.env ...`).
Then install `deploy/remind-me-hub-standalone.container` in place of the two
Postgres units, and replace `DATABASE_URL` in `hub.env` with
`REMIND_ME_HUB_DATA_DIR`.

## Routes

| Route | Auth | Purpose |
| --- | --- | --- |
| `GET /health` | none | Liveness. 200 when the database is reachable, 503 when not. |
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
and it must keep answering when the database is down. It carries no counts. Its
`db` field never echoes the underlying error, which typically embeds host,
port, database name and credentials — that goes to the log instead.

Every response carries `X-Hub-Version`, errors included, so "which build
answered this?" never needs a second request.

## Two backends

**Postgres** is the drop-in. The DDL, the `COLLATE "C"`, the
`memories_hub_seq` sequence and the legacy TIMESTAMPTZ→TEXT migration are
deliberately the reference's rather than a tidier equivalent, because the
contract is not "works" but *reads the database the Python hub was using an
hour ago*.

**SQLite** is for a self-hosted hub that wants one file and no server. It is
wire-identical and **not** schema-identical; there is no in-place switch
between the two. `docs/adr/0015` records why both exist.

One honest degradation: `GET /count?approx=1` asks for planner estimates, which
SQLite has no usable equivalent for. Rather than label a full scan
"approximate", the SQLite backend falls back to exact counts and the response
reports `approximate: false`.

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
crates/remind_me_hub/setup.sh status      # units, health, per-node counts
crates/remind_me_hub/setup.sh update      # pull, rebuild, restart, verify
crates/remind_me_hub/setup.sh restore d.sql   # restore a Postgres dump
```

`update` checks that the *new build is actually serving* rather than only that
the service restarted — a rebuilt image the unit never picked up leaves a
perfectly healthy old hub answering, which reads as success.

`restore` accepts legacy hub dumps. The hub migrates the restored schema on
startup: TIMESTAMPTZ columns become canonical TEXT, missing columns are added
with client-matching defaults, and `hub_seq` is backfilled in `(updated_at,
id)` order so the migration does not itself reorder history.

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
  full-disk encryption, your provider's volume encryption, or Postgres's own
  extensions.

## Testing

```sh
cargo test -p remind_me_hub                       # the engine and SQLite; Postgres tests skip
REMIND_ME_HUB_TEST_DATABASE_URL=postgresql://… \
  cargo test -p remind_me_hub -- --test-threads=1 # with a real Postgres
```

The route suite (`tests/suite/routes.rs`) runs once per backend that needs no
server. `tests/suite/differential.rs` pushes one script to several backends and
requires every read to match: SQLite against the embedded engine in
`hub_multimodal_test.rs`, and all of them in `hub_postgres_test.rs`.

The Postgres tests skip when no database is configured — and it is worth being
precise about what that means: a skipped test reports as **passed**, and cargo
hides the `SKIP` line unless you pass `--nocapture`. Locally that is fine. For
CI it is not, so `REMIND_ME_HUB_REQUIRE_POSTGRES=1` turns the skip into a hard
failure and CI sets it; the environment cannot lose its database and stay
green. They cover what only a real server can: the legacy migration, `nextval()`-driven `hub_seq`, planner
estimates, and the differential test with Postgres among the backends. It
compares `hub_seq` by order there, not by value: `nextval()` also spends a
number on a push that loses last-write-wins, so Postgres's sequence has gaps
the other backends' does not.

CI runs all of these, plus `--no-default-features` builds with and without
`multimodal-store`, so a `postgres::` reference leaking outside its feature
gate cannot go unnoticed.
