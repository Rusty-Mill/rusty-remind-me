# Hub deploy templates

Several ways to run the hub. All of them build the same image from
`../Containerfile` and share one `hub.env` contract: `REMIND_ME_HUB_DATA_DIR`
for the store and `SYNC_SECRET` for the bearer. They are alternative
deployments, not different hubs.

| File | Deployment |
| --- | --- |
| `remind-me-hub.container` | Podman Quadlet, rootless, one container |
| `docker-compose.yml` | Docker Compose, one container |
| `fly.toml` | Fly.io, with a Fly Volume |
| `railway.json` | Railway, with a Railway volume |

`../setup.sh install` does the Quadlet path end to end (secret, unit, image,
service) and is the shortest route to a working hub.

## The store

The hub stores its data in the embedded `rusty_multimodal_db` engine
(`docs/adr/0021`): a data directory on a volume, no database server. It holds
every record in memory and logs each write to disk before answering. Give the
container a persistent volume at `/data` and set
`REMIND_ME_HUB_DATA_DIR=/data/hub`.

The Postgres and SQLite stores are gone. A hub still configured for either
(`DATABASE_URL` or `REMIND_ME_HUB_DB_PATH` set) refuses to start and says how
to copy its data over:

- **Quadlet installs:** `../setup.sh migrate` copies the old store onto the
  engine, rewrites `hub.env` and swaps the unit. The old data is left in
  place.
- **Anything else:** run `rusty-remind-me-hub-copy` (in the image and the
  release archives; see the crate README's "Moving a hub onto the engine"),
  then replace `DATABASE_URL` or `REMIND_ME_HUB_DB_PATH` with
  `REMIND_ME_HUB_DATA_DIR`.

The copy keeps every `hub_seq`, so nodes carry on from their cursors.

A Postgres dump from a Python hub loads with `../setup.sh restore
<dump.sql>`, which reads it through a throwaway Postgres container.

## Railway

`railway.json` sets the build and the health check. In the Railway service's
settings, attach a volume mounted at `/data` and set the variables
`REMIND_ME_HUB_DATA_DIR=/data/hub` and `SYNC_SECRET`. A volume is attached to
one replica, so run a single instance.

## The build context is the monorepo root

Every template here builds with the monorepo root (the directory holding the
workspace `Cargo.toml` and `Cargo.lock`) as context and
`crates/apps/rusty_remind_me/crates/remind_me_hub/Containerfile` as the file.
That differs from the Python hub, whose context was `hub/` alone because it
copied one `main.py`; this hub is a crate in a Cargo workspace. Building with
any other directory as context fails on a missing manifest, which does not
explain itself.

## Exposure

Every template binds to loopback or a private address, never `0.0.0.0` on a
public interface. `SYNC_SECRET` is the only thing between the internet and the
whole memory database, so reach the hub over an SSH tunnel or Tailscale.
Widening a `PublishPort`/`ports:` line is a decision to make deliberately, with
real TLS and rate limiting in front of it.

`/health` is the one unauthenticated route — deliberately, so deploy
healthchecks keep working when the store is down, and it reports no counts.

## Env file

`hub.env.example`: copy to `hub.env`, fill in a real secret, `chmod 600`.
`setup.sh install` generates it with a fresh secret and never overwrites an
existing one.
