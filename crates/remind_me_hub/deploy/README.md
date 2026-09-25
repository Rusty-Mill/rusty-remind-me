# Hub deploy templates

Four ways to run the hub, plus the env files they share. All of them build the
same image from `../Containerfile` and speak the same `SYNC_SECRET` /
`DATABASE_URL` contract — these are alternative deployments, not different
hubs.

| File | Deployment |
| --- | --- |
| `remind-me.network`, `remind-me-postgres.container`, `remind-me-hub.container` | Podman Quadlet, rootless, with Postgres |
| `remind-me-hub-sqlite.container` | Podman Quadlet, rootless, SQLite — one container |
| `docker-compose.yml` | Docker Compose, with Postgres |
| `docker-compose.sqlite.yml` | Docker Compose, SQLite — one container |
| `fly.toml` | Fly.io, with managed Fly Postgres |
| `railway.json` | Railway, with a managed Postgres plugin |

`../setup.sh install` does the Quadlet path end to end (secrets, units, image,
services) and is the shortest route to a working hub. Add `--sqlite` for the
SQLite variant.

## The build context is the workspace root

Every template here builds with the workspace root as context and
`crates/remind_me_hub/Containerfile` as the file. That differs from the Python
hub, whose context was `hub/` alone because it copied one `main.py`; this hub
is a crate in a Cargo workspace and needs the root `Cargo.toml` and
`Cargo.lock`. Building with this directory as context fails on a missing
manifest, which does not explain itself.

## Postgres or SQLite

**Postgres** is the default and the drop-in: it reads the Python hub's own
schema, legacy dumps included, so an existing deployment can be taken over in
place. Choose it when several nodes push concurrently.

**SQLite** has no counterpart in the Python hub. One container, one file, no
database server. Choose it for a single operator's handful of devices. It is
wire-identical, so no client can tell the difference — but it is **not**
schema-identical, so there is no in-place switch between the two backends.
Decide before you have data. `docs/adr/0015` records the reasoning.

Setting more than one of `DATABASE_URL`, `REMIND_ME_HUB_DB_PATH` and
`REMIND_ME_HUB_DATA_DIR` is a startup error rather than a silent precedence
rule: it should never be ambiguous which store is serving. The third, the
embedded-engine backend, is a preview none of these templates use yet (see the
crate README).

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
| `hub.env.example` | Postgres deployments |
| `hub-sqlite.env.example` | SQLite deployments |
| `postgres.env.example` | The Postgres container itself |

Copy, fill in real secrets, `chmod 600`. For Postgres the password must match
in both files, since `hub.env`'s `DATABASE_URL` embeds it. `setup.sh install`
generates both with fresh secrets and never overwrites existing ones.
