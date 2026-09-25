# Hub deploy templates

Several ways to run the hub, plus the env files they share. All of them build
the same image from `../Containerfile` and speak the same `SYNC_SECRET`
contract, with one variable choosing the store — these are alternative
deployments, not different hubs.

| File | Deployment |
| --- | --- |
| `remind-me-hub-standalone.container` | Podman Quadlet, rootless, one container: the embedded engine (default) or SQLite |
| `remind-me.network`, `remind-me-postgres.container`, `remind-me-hub.container` | Podman Quadlet, rootless, with Postgres |
| `docker-compose.engine.yml` | Docker Compose, the embedded engine — one container |
| `docker-compose.yml` | Docker Compose, with Postgres |
| `docker-compose.sqlite.yml` | Docker Compose, SQLite — one container |
| `fly.toml` | Fly.io, with managed Fly Postgres |
| `railway.json` | Railway, with a managed Postgres plugin |

`../setup.sh install` does the Quadlet path end to end (secrets, units, image,
services) and is the shortest route to a working hub. It installs the embedded
engine unless you pass `--postgres` or `--sqlite`. On a machine that already
has a `hub.env`, it keeps that hub's store whatever the flags say.

## The build context is the monorepo root

Every template here builds with the monorepo root (the directory holding the
workspace `Cargo.toml` and `Cargo.lock`) as context and
`crates/apps/rusty_remind_me/crates/remind_me_hub/Containerfile` as the file. That differs from the Python hub, whose context
was `hub/` alone because it copied one `main.py`; this hub is a crate in a
Cargo workspace. Building with any other directory as context fails on a
missing manifest, which does not explain itself.

## Which store

**The embedded engine** is the default for a new hub (`docs/adr/0021`). One
container, a data directory, no database server. It holds every record in
memory and logs each write to disk before answering, and its pulls stay fast
while pushes run.

**Postgres** is the drop-in for an existing Python hub: it reads that hub's own
schema, legacy dumps included, so a deployment can be taken over in place and
restored with `setup.sh restore`.

**SQLite** is one container and one file.

The three are wire-identical, so no client can tell the difference, but they
are **not** schema-identical: there is no switching a store in place. To move a
hub onto the engine, copy it with `rusty-remind-me-hub-copy` (in the image and
the release archives; see the crate README), which keeps every `hub_seq` so
nodes carry on from their cursors.

Setting more than one of `DATABASE_URL`, `REMIND_ME_HUB_DB_PATH` and
`REMIND_ME_HUB_DATA_DIR` is a startup error rather than a silent precedence
rule: it should never be ambiguous which store is serving.

## Exposure

Every template binds to loopback or a private address, never `0.0.0.0` on a
public interface. `SYNC_SECRET` is the only thing between the internet and the
whole memory database, so reach the hub over an SSH tunnel or Tailscale.
Widening a `PublishPort`/`ports:` line is a decision to make deliberately, with
real TLS and rate limiting in front of it.

`/health` is the one unauthenticated route — deliberately, so deploy
healthchecks keep working when the database is down, and it reports no counts.

## Env files

| File | For |
| --- | --- |
| `hub-engine.env.example` | Embedded-engine deployments (the default) |
| `hub.env.example` | Postgres deployments |
| `hub-sqlite.env.example` | SQLite deployments |
| `postgres.env.example` | The Postgres container itself |

Copy, fill in real secrets, `chmod 600`. For Postgres the password must match
in both files, since `hub.env`'s `DATABASE_URL` embeds it. `setup.sh install`
generates both with fresh secrets and never overwrites existing ones.
