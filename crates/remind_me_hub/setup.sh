#!/usr/bin/env bash
# Remind Me sync hub (Rust) — one-command server setup for rootless Podman.
#
# Usage:
#   ./setup.sh install               Full install: secret, quadlet, image,
#                                    service. Idempotent — never clobbers an
#                                    existing secret or data.
#   ./setup.sh migrate               Move a hub still on Postgres or SQLite
#                                    onto the embedded engine, keeping every
#                                    hub_seq. Add --drop-invalid to copy past
#                                    rows the engine cannot store.
#   ./setup.sh restore <dump.sql>    Load a Postgres dump (legacy hub dumps
#                                    supported) into this hub, through a
#                                    throwaway Postgres container. Add --force
#                                    to replace a hub that holds memories.
#   ./setup.sh status                Service state, hub health, per-node counts.
#   ./setup.sh update                git pull, rebuild the hub image, restart.
#
# The hub stores its data in the embedded engine: one container, a data
# directory, no database server (docs/adr/0021). The Postgres and SQLite
# stores are gone; `migrate` copies a hub off either.
#
# Flags:
#   --force         allow restore to replace a hub that holds memories
#   --drop-invalid  let migrate/restore copy past rows the engine cannot store
#   --dry-run       print mutating commands instead of executing them
#                   (install, migrate)
#
# Layout it manages:
#   ~/remind-me-hub/hub.env            store setting + SYNC_SECRET (chmod 600)
#   ~/remind-me-hub/data/hub/          the engine's data directory
#   ~/.config/containers/systemd/      the Quadlet unit

set -euo pipefail

HUB_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# The monorepo root, five levels up, not the crate: the image build context
# must hold the workspace Cargo.toml and Cargo.lock (see the Containerfile).
REPO_DIR="$(cd "$HUB_DIR/../../../../.." && pwd)"
DATA_DIR="${REMIND_ME_HUB_DATA:-$HOME/remind-me-hub}"
QUADLET_DIR="$HOME/.config/containers/systemd"

FORCE=0
DRY_RUN=0
DROP_INVALID=0
# The store an existing hub.env configures: engine, or a retired postgres or
# sqlite. Empty for a machine with no hub yet. See resolve_backend.
BACKEND=""
# The engine's data directory, inside the container and under data/ on the
# host.
ENGINE_DIR=/data/hub

log()  { printf '\033[1;32m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33mwarning:\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

run() {
    if (( DRY_RUN )); then
        printf '    [dry-run] %s\n' "$*"
    else
        "$@"
    fi
}

rand_hex() { openssl rand -hex "$1"; }

env_value() {  # env_value <file> <KEY>
    sed -n "s/^$2=//p" "$1" | head -n 1
}

# Probe the hub through the address its Quadlet actually publishes, not a
# hardcoded loopback: the templates bind to the host's Tailscale IP, so
# 127.0.0.1 never answers there even when the hub is perfectly healthy.
_hub_publish_host() {
    local unit="$QUADLET_DIR/remind-me-hub.container"
    local host=""
    if [ -f "$unit" ]; then
        host=$(sed -n 's/^PublishPort=\([^:]*\):.*/\1/p' "$unit" | head -n 1)
    fi
    printf '%s' "${host:-127.0.0.1}"
}

HEALTH_URL=""
_set_health_url() { HEALTH_URL="http://$(_hub_publish_host):8765/health"; }

# Which store an existing hub.env configures, or nothing if there is none.
backend_of_env() {
    local env="$DATA_DIR/hub.env"
    [ -f "$env" ] || return 0
    if [[ -n "$(env_value "$env" DATABASE_URL)" ]]; then
        printf 'postgres'
    elif [[ -n "$(env_value "$env" REMIND_ME_HUB_DB_PATH)" ]]; then
        printf 'sqlite'
    elif [[ -n "$(env_value "$env" REMIND_ME_HUB_DATA_DIR)" ]]; then
        printf 'engine'
    fi
}

resolve_backend() {
    BACKEND=$(backend_of_env)
    if [[ -f "$DATA_DIR/hub.env" && -z "$BACKEND" ]]; then
        die "$DATA_DIR/hub.env sets none of REMIND_ME_HUB_DATA_DIR, DATABASE_URL, REMIND_ME_HUB_DB_PATH"
    fi
}

# Stop anything but `migrate` from acting on a hub still on a retired store:
# the new image would refuse to start on its hub.env, and a rebuilt unit
# would strand its data.
require_engine() {
    if [[ "$BACKEND" == postgres || "$BACKEND" == sqlite ]]; then
        die "this hub still stores its data in ${BACKEND}, which the hub no longer supports. Run '$0 migrate' to copy it onto the embedded engine first."
    fi
}

wait_for_postgres() {  # wait_for_postgres <container> <user>
    local _
    for _ in $(seq 1 60); do
        if podman exec "$1" pg_isready -U "$2" >/dev/null 2>&1; then
            return 0
        fi
        sleep 1
    done
    die "Postgres in $1 did not become ready within 60s"
}

version_from_health() {
    curl -fsS "$HEALTH_URL" 2>/dev/null \
        | sed -n 's/.*"version"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p'
}

wait_for_hub() {
    local _
    for _ in $(seq 1 60); do
        if curl -fsS "$HEALTH_URL" >/dev/null 2>&1; then
            return 0
        fi
        sleep 1
    done
    return 1
}

check_prereqs() {
    local missing=()
    command -v podman  >/dev/null 2>&1 || missing+=(podman)
    command -v curl    >/dev/null 2>&1 || missing+=(curl)
    command -v openssl >/dev/null 2>&1 || missing+=(openssl)
    (( ${#missing[@]} == 0 )) || die "missing required commands: ${missing[*]}"

    # Quadlet arrived in Podman 4.4. An older podman fails later, during
    # `systemctl --user daemon-reload`, with nothing pointing at the version.
    local major minor
    major=$(podman version --format '{{.Client.Version}}' 2>/dev/null | cut -d. -f1)
    minor=$(podman version --format '{{.Client.Version}}' 2>/dev/null | cut -d. -f2)
    if [[ -n "$major" ]] && { (( major < 4 )) || { (( major == 4 )) && (( minor < 4 )); }; }; then
        die "podman $major.$minor is too old for Quadlet; 4.4+ is required"
    fi
}

ensure_linger() {
    # Without linger, rootless user services stop when the last session ends —
    # so the hub dies when you log out, which is exactly when nobody notices.
    if command -v loginctl >/dev/null 2>&1; then
        if ! loginctl show-user "$USER" --property=Linger 2>/dev/null | grep -q 'Linger=yes'; then
            log "Enabling linger for $USER so the hub survives logout"
            run loginctl enable-linger "$USER" || warn "could not enable linger; the hub will stop when you log out"
        fi
    fi
}

ensure_env_files() {
    run mkdir -p "$QUADLET_DIR" "$DATA_DIR/data"
    if [ -f "$DATA_DIR/hub.env" ]; then
        log "Keeping existing $DATA_DIR/hub.env"
        return
    fi
    log "Generating $DATA_DIR/hub.env"
    if ! (( DRY_RUN )); then
        cat > "$DATA_DIR/hub.env" <<EOF
REMIND_ME_HUB_DATA_DIR=$ENGINE_DIR
SYNC_SECRET=$(rand_hex 32)
EOF
        chmod 600 "$DATA_DIR/hub.env"
    fi
}

install_quadlets() {
    local unit="$QUADLET_DIR/remind-me-hub.container" publish=""
    # An installed unit's address is the operator's, edited for this host; a
    # reinstall or a migrate keeps it rather than reverting to the template's.
    [ -f "$unit" ] && publish=$(sed -n 's/^PublishPort=//p' "$unit" | head -n 1)
    log "Installing the Quadlet unit to $QUADLET_DIR"
    run cp "$HUB_DIR/deploy/remind-me-hub.container" "$unit"
    if [[ -n "$publish" ]]; then
        run sed -i "s|^PublishPort=.*|PublishPort=$publish|" "$unit"
    fi
    run systemctl --user daemon-reload
}

hub_version_from_source() {
    # `pub const HUB_VERSION: &str = "1.5.0";` in the crate's lib.rs. The
    # reference reads a Python assignment from main.py; same idea, different
    # syntax, and the same reason: the image holds a binary with no manifest
    # to derive a version from.
    sed -n 's/^pub const HUB_VERSION: &str = "\([^"]*\)".*/\1/p' \
        "$HUB_DIR/src/lib.rs" | head -n 1
}

build_image() {
    local version
    version=$(hub_version_from_source)
    [[ -n "$version" ]] || die "could not read HUB_VERSION from $HUB_DIR/src/lib.rs"

    # Tagged with the version as well as latest, for two reasons: `podman
    # image ls` can then tell you what you have without starting anything,
    # and the previous build survives an update instead of being overwritten
    # -- so a rollback is a retag rather than a rebuild from an older
    # checkout, under exactly the time pressure that makes that unpleasant.
    log "Building the hub image (version $version) — a release build, allow a few minutes"
    [[ -f "$REPO_DIR/Cargo.lock" ]] \
        || die "no Cargo.lock in $REPO_DIR; run setup.sh from inside the monorepo checkout"
    run podman build \
        --build-arg "HUB_VERSION=$version" \
        -f "$HUB_DIR/Containerfile" \
        -t "remind-me-hub:$version" \
        -t remind-me-hub:latest \
        "$REPO_DIR"
}

start_services() {
    log "Starting the hub"
    run systemctl --user start remind-me-hub.service
    (( DRY_RUN )) || wait_for_hub || warn "the hub did not answer $HEALTH_URL within 60s; check: journalctl --user -u remind-me-hub -n 50"
}

# Run the copy tool in the hub image, writing to $DATA_DIR/data. Extra
# arguments go to `podman run` before the image (a network, an env file).
# Returns the tool's status; it has already printed what went wrong.
copy_into_engine() {  # copy_into_engine <target> <podman run args...> -- <copy args...>
    local target="$1"; shift
    local podman_args=()
    while (( $# )) && [[ "$1" != -- ]]; do podman_args+=("$1"); shift; done
    shift
    local extra=()
    (( DROP_INVALID )) && extra+=(--drop-invalid)
    # The `+` forms keep an empty array from tripping `set -u` on bash < 4.4.
    run podman run --rm ${podman_args[@]+"${podman_args[@]}"} \
        -v "$DATA_DIR/data:/data:Z,U" \
        remind-me-hub:latest \
        rusty-remind-me-hub-copy "$@" --to "$target" ${extra[@]+"${extra[@]}"}
}

# Move an engine directory inside the data volume. The volume is owned by
# the container's user, which the host user reaches through `podman unshare`.
move_in_data() {  # move_in_data <from> <to>, container paths under /data
    run podman unshare mv "$DATA_DIR/data/${1#/data/}" "$DATA_DIR/data/${2#/data/}"
}

cmd_install() {
    require_engine
    check_prereqs
    ensure_linger
    ensure_env_files
    install_quadlets
    build_image
    start_services

    if (( DRY_RUN )); then
        log "Dry run complete — no changes made"
        return
    fi

    local secret
    secret=$(env_value "$DATA_DIR/hub.env" SYNC_SECRET)
    log "Hub is up: $(curl -fsS "$HEALTH_URL")"
    cat <<EOF

Server setup complete.

Sync secret (clients need this as REMIND_ME_SYNC_SECRET):
  $secret

Next steps:
  - configure a client:  run crates/remind_me_hub/client-setup.sh on each client
  - check anytime:       $0 status
  - load a Postgres dump: $0 restore /path/to/postgres-backup.sql
EOF
}

cmd_migrate() {
    [[ "$BACKEND" != engine ]] || die "nothing to migrate: this hub already uses the embedded engine"
    [[ -n "$BACKEND" ]] || die "nothing to migrate: there is no hub.env in $DATA_DIR"
    check_prereqs
    local from=()
    if [[ "$BACKEND" == postgres ]]; then
        podman container exists remind-me-postgres \
            || die "remind-me-postgres is not running; start it (systemctl --user start remind-me-postgres) so its data can be read"
        # --env-file hands the copy DATABASE_URL without putting its password
        # on a command line.
        from=(--network remind-me --env-file "$DATA_DIR/hub.env" -- --from-postgres)
    else
        from=(-- --from-sqlite "$(env_value "$DATA_DIR/hub.env" REMIND_ME_HUB_DB_PATH)")
    fi
    [[ ! -e "$DATA_DIR/data/hub" ]] \
        || die "$DATA_DIR/data/hub already exists; move it aside so the copy lands in an empty directory"

    run mkdir -p "$DATA_DIR/data"
    build_image
    # Rows the engine cannot store stop the copy. Finding them while the hub
    # still serves keeps them from costing any downtime. (--drop-invalid
    # copies past them, so there is nothing to check first.)
    if ! (( DROP_INVALID )); then
        log "Checking that every row can be copied, while the hub still runs"
        copy_into_engine "$ENGINE_DIR" "${from[@]}" --check \
            || die "some rows cannot be copied (listed above); nothing was stopped or written. Re-run with --drop-invalid to copy everything else."
    fi
    log "Stopping the hub so nothing writes during the copy"
    run systemctl --user stop remind-me-hub.service || true
    log "Copying the $BACKEND store onto the engine"
    copy_into_engine "$ENGINE_DIR" "${from[@]}" \
        || die "the copy failed and wrote nothing (see above). The hub is stopped and its $BACKEND store is untouched; fix the cause and re-run '$0 migrate'."

    log "Pointing hub.env at the engine (the old one is kept as hub.env.pre-engine)"
    if ! (( DRY_RUN )); then
        cp -p "$DATA_DIR/hub.env" "$DATA_DIR/hub.env.pre-engine"
        {
            printf 'REMIND_ME_HUB_DATA_DIR=%s\n' "$ENGINE_DIR"
            grep -v -E '^(DATABASE_URL|REMIND_ME_HUB_DB_PATH|REMIND_ME_HUB_STATEMENT_TIMEOUT_MS)=' \
                "$DATA_DIR/hub.env.pre-engine" || true
        } > "$DATA_DIR/hub.env.new"
        chmod 600 "$DATA_DIR/hub.env.new"
        mv "$DATA_DIR/hub.env.new" "$DATA_DIR/hub.env"
    fi

    install_quadlets
    start_services
    (( DRY_RUN )) && { log "Dry run complete — no changes made"; return; }

    log "Migrated. $(curl -fsS "$HEALTH_URL")"
    if [[ "$BACKEND" == postgres ]]; then
        cat <<EOF

The Postgres container and its data are untouched. Once the hub looks right
('$0 status'), remove them:
  systemctl --user stop remind-me-postgres.service
  rm $QUADLET_DIR/remind-me-postgres.container $QUADLET_DIR/remind-me.network
  systemctl --user daemon-reload
  # and, when you no longer want it: $DATA_DIR/postgres-data, postgres.env
EOF
    else
        printf '\nThe SQLite file is untouched. Once the hub looks right (%s status), delete it from %s/data.\n' "$0" "$DATA_DIR"
    fi
}

# Load a Postgres dump (a Python hub's, say) into this hub. The hub no
# longer runs Postgres, so the dump goes into a throwaway Postgres container
# the copy tool reads from, which is removed afterwards.
cmd_restore() {
    local dump="${1:-}"
    [[ -n "$dump" ]] || die "usage: $0 restore <dump.sql> [--force] [--drop-invalid]"
    [[ -f "$dump" ]] || die "no such file: $dump"
    require_engine
    [[ "$BACKEND" == engine ]] || die "no hub here yet; run '$0 install' first"
    check_prereqs

    local existing
    existing=$(curl -fsS -H "Authorization: Bearer $(env_value "$DATA_DIR/hub.env" SYNC_SECRET)" \
        "http://$(_hub_publish_host):8765/count?table=memories" 2>/dev/null \
        | sed -n 's/.*"total"[[:space:]]*:[[:space:]]*\([0-9]*\).*/\1/p')
    if [[ "${existing:-0}" != "0" ]] && (( ! FORCE )); then
        die "the hub already holds $existing memories; re-run with --force to replace them (they are kept aside, not deleted)"
    fi

    local pg=remind-me-restore-pg net=remind-me-restore password
    password=$(rand_hex 24)
    # shellcheck disable=SC2064 # expand now: the names are fixed for this run.
    trap "podman rm -f $pg >/dev/null 2>&1; podman network rm $net >/dev/null 2>&1" EXIT
    log "Starting a throwaway Postgres to load $dump into"
    podman network exists "$net" || podman network create "$net" >/dev/null
    podman run -d --rm --name "$pg" --network "$net" \
        -e POSTGRES_USER=remindme -e POSTGRES_PASSWORD="$password" -e POSTGRES_DB=remindme \
        docker.io/library/postgres:16-alpine >/dev/null
    wait_for_postgres "$pg" remindme
    podman exec -i "$pg" psql -q -U remindme -d remindme < "$dump" >/dev/null

    log "Copying the dump onto the engine"
    local env_file="$DATA_DIR/restore.env"
    (umask 077; printf 'DATABASE_URL=postgresql://remindme:%s@%s:5432/remindme\n' "$password" "$pg" > "$env_file")
    copy_into_engine /data/hub.restored --network "$net" --env-file "$env_file" -- --from-postgres \
        || { rm -f "$env_file"; die "the copy failed and wrote nothing (see above); the hub was not touched. Rows the engine cannot store are listed there; re-run with --drop-invalid to copy everything else."; }
    rm -f "$env_file"

    log "Stopping the hub to swap the restored data in"
    systemctl --user stop remind-me-hub.service || true
    local aside=""
    if podman unshare test -e "$DATA_DIR/data/hub"; then
        aside="/data/hub.replaced-$(date +%Y%m%d%H%M%S)"
        move_in_data "$ENGINE_DIR" "$aside"
    fi
    move_in_data /data/hub.restored "$ENGINE_DIR"
    systemctl --user start remind-me-hub.service
    wait_for_hub || die "the hub did not come back up; check: journalctl --user -u remind-me-hub -n 50"
    log "Restored. $(curl -fsS "$HEALTH_URL")"
    if [[ -n "$aside" ]]; then
        printf 'The data it replaced is in %s/data/%s.\n' "$DATA_DIR" "${aside#/data/}"
    fi
}

cmd_status() {
    if [[ "$BACKEND" == postgres || "$BACKEND" == sqlite ]]; then
        warn "this hub still stores its data in $BACKEND; run '$0 migrate' before updating it"
    fi
    printf '%-32s %s\n' remind-me-hub.service \
        "$(systemctl --user is-active remind-me-hub.service 2>/dev/null || echo inactive)"

    local health
    if health=$(curl -fsS "$HEALTH_URL" 2>/dev/null); then
        printf '\nhealth: %s\n' "$health"
    else
        printf '\nhealth: unreachable at %s\n' "$HEALTH_URL"
        return
    fi

    local secret
    secret=$(env_value "$DATA_DIR/hub.env" SYNC_SECRET)
    printf '\nper-node counts:\n'
    curl -fsS -H "Authorization: Bearer $secret" \
        "http://$(_hub_publish_host):8765/count?by=origin_node" 2>/dev/null \
        || printf '  (unavailable)\n'
    printf '\n'
}

cmd_update() {
    require_engine
    local before after
    before=$(version_from_health || true)

    log "Pulling the latest source"
    run git -C "$REPO_DIR" pull --ff-only

    build_image
    log "Restarting the hub"
    run systemctl --user restart remind-me-hub.service
    (( DRY_RUN )) && { log "Dry run complete"; return; }

    wait_for_hub || die "the hub did not come back up; check: journalctl --user -u remind-me-hub -n 50"

    # Checking that the *new build is actually serving*, not merely that the
    # service restarted: a rebuilt image that the unit never picked up leaves a
    # perfectly healthy old hub answering, which reads as success.
    after=$(version_from_health || true)
    local expected
    expected=$(hub_version_from_source)
    if [[ "$after" == "$expected" ]]; then
        log "Updated: now serving $after${before:+ (was $before)}"
    else
        die "the hub reports '$after' but the source says '$expected' — the new image is not serving. Check: systemctl --user status remind-me-hub"
    fi
}

main() {
    local cmd="" args=()
    while (( $# )); do
        case "$1" in
            --force)   FORCE=1 ;;
            --dry-run) DRY_RUN=1 ;;
            --drop-invalid) DROP_INVALID=1 ;;
            --postgres|--sqlite)
                die "$1 is gone: the hub stores its data only in the embedded engine. To move an existing hub onto it, run '$0 migrate'." ;;
            -h|--help) sed -n '2,33p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
            install|migrate|restore|status|update)
                cmd="$1" ;;
            *)  args+=("$1") ;;
        esac
        shift
    done

    _set_health_url
    resolve_backend

    case "${cmd:-install}" in
        install) cmd_install ;;
        migrate) cmd_migrate ;;
        restore) cmd_restore "${args[@]:-}" ;;
        status)  cmd_status ;;
        update)  cmd_update ;;
    esac
}

main "$@"
