# ADR-0015: Port the hub, with storage behind a trait

Status: Accepted
Date: 2026-08-05

## Context

`gap-analysis.md` listed **E1, the hub**, as the one gap deliberately never
filed as an issue: `hub/main.py` is 1,341 LOC of Python serving ten routes,
deployed as a container against its own Postgres, and porting it is not an
addition to the Rust workspace so much as a second deployable in a second
runtime. The analysis said the decision belonged to a human. It has now been
taken: port it, with both storage backends.

Two facts framed the work, and only the first was obvious going in.

1. **Most of the wire protocol already existed.** `remind_me_core::sync::server`
   serves the peer side of exactly this protocol — `/health`, `/count`, and all
   five `/sync/*` routes — over SQLite, with hand-rolled HTTP. Hub and peer are
   the same protocol against different topologies. Only `/stats`, `/metrics`,
   `/admin/compact_tombstones`, and the `origin_node`/`hub_seq` bookkeeping are
   genuinely hub-only.

2. **The storage backend is the actual decision.** The reference is
   Postgres-only. A Rust hub on SQLite would be far smaller and reuse more, but
   it could not take over an existing deployment without a data migration —
   which is most of what "successor" means for a component whose entire job is
   holding the shared copy.

## Decision

**Port all ten routes, with storage behind a `HubStore` trait, and ship both
backends.** Postgres is the default feature; SQLite needs no feature at all.

- **Postgres** is the drop-in. The DDL, the `COLLATE "C"`, the
  `memories_hub_seq` sequence and the legacy TIMESTAMPTZ→TEXT migration are all
  deliberately the reference's rather than a tidier equivalent, because the
  contract is not "works" but "reads the database the Python hub was using an
  hour ago".
- **SQLite** is for a self-hosted hub that wants one file and no server. It is
  wire-identical and *not* schema-identical, and does not pretend otherwise.

The trait exposes complete operations — no connections, no transactions, no
SQL. The two backends differ in exactly the places a leakier interface would
have to paper over: sequences (`nextval` vs. `MAX+1` under the write lock),
upsert syntax, JSONB vs. TEXT-holding-JSON, and planner statistics that only
one of them has.

`apply_record` is the sharpest case and shows why the boundary sits here. The
reference wraps each record in its own savepoint so one malformed record cannot
poison a batch — and that isolation is *part of the operation*, not something a
caller can be trusted to remember. So the trait takes one already-validated
record and owns the transaction around it. Validation and canonicalisation
happen once, above the trait, in `record.rs`; a backend never sees a
malformed record, and "malformed" can never be confused with "storage failed"
in the push tally.

## Consequences

- **A new deployable**, `rusty-remind-me-hub`, and the workspace's fifth
  crate. It shares the wire protocol with the node's peer server and nothing
  else: no MCP, no local memories to serve, no scheduler.
- **`postgres` (the sync client) joins the dependency graph**, on by default.
  Sync rather than `tokio-postgres` because every hand-rolled server in this
  workspace is thread-per-connection, and the reference's own hub runs sync
  psycopg in a thread pool. `--no-default-features` gives a hub with no
  Postgres driver at all, and CI builds that configuration so a `postgres::`
  reference cannot leak outside the feature gate unnoticed.
- **Two backends is two things to keep honest.** The mitigation is a
  differential test that runs the same script through both and asserts the
  pulled records, `/stats` and `/count` match — the only thing that makes the
  trait more than a hopeful interface. It is why the Postgres tests exist at
  all rather than the backend being assumed correct because it compiles.
- **`/count?approx=1` degrades honestly on SQLite.** `pg_class.reltuples` has
  no counterpart; `sqlite_stat1` only exists after `ANALYZE` and is stale by
  construction. The backend returns "no estimate available", the route falls
  back to exact counts, and the response reports `approximate: false`.
  Labelling a full scan "approximate" is the one thing that flag must never
  mean.

## The reference bug this found

The reference's legacy migration converts TIMESTAMPTZ to text with
`regexp_replace(..., '\.?0+$', '')`, and its own comment says the goal is to
"match Python's `datetime.isoformat()` exactly". It does not. It strips *all*
trailing zeros, so `.500000` becomes `.5` — a string `isoformat()` would never
produce, since it emits six digits or none.

That is not cosmetic. Under `COLLATE "C"`, `...:00.5+00:00` sorts **before**
`...:00.500000+00:00` (`+` is 0x2B, `0` is 0x30). A migrated row therefore
compares as *older* than the identical instant on the node that wrote it,
which corrupts both the pull cursor's ordering and the LWW comparison the hub
resolves conflicts with. It affects only legacy rows whose microseconds are
non-zero and end in a zero — roughly one in ten of them — which is exactly the
kind of bug that survives a long time.

This port strips only a wholly-zero fraction, which is what the reference
meant. It is a deliberate divergence and the only intentional behavioural
difference from the reference in the whole hub.

**It was found by running the migration against a real Postgres**, not by
reading the regex. That is the argument for the Postgres tests existing.

## Alternatives considered

- **SQLite only.** Much smaller: the hub becomes the peer server plus three
  routes. Rejected as the sole backend because it cannot adopt an existing
  Postgres deployment, and because a central hub taking concurrent pushes from
  many nodes is precisely SQLite's weak spot. Kept as *a* backend, where those
  objections do not apply.
- **Postgres only, like the reference.** Rejected because it makes a
  single-operator self-hosted hub carry a database server for a workload one
  file would serve, and because the trait needed to exist anyway for the
  differential test to be possible.
- **An async stack (`tokio-postgres` + a framework).** Rejected: no other
  server in this workspace is async or uses a framework, and the reference gets
  FastAPI and then spends real effort disabling its `docs_url`, `redoc_url` and
  `openapi_url` — which default to ON and unauthenticated and would publish
  every route, including the one that hard-deletes rows. Here a route that was
  not written does not exist.
- **Reusing `sync::server`'s handlers directly.** Tempting, since they answer
  seven of the same routes. Rejected: they are written against a `rusqlite`
  `Connection` and a node's schema, so the reuse would mean either a
  SQLite-only hub or refactoring the node's peer server to satisfy this trait —
  a change to working code that this port has no business forcing.
