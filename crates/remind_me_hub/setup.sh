#!/usr/bin/env bash
# Remind Me sync hub (Rust) — one-command server setup for rootless Podman.
#
# Usage:
#   ./setup.sh install               Full install: secrets, quadlets, image,
#                                    services. Idempotent — never clobbers
#                                    existing secrets or data.
#   ./setup.sh restore <dump.sql>    Restore a Postgres dump (legacy hub dumps
#                                    supported). Add --force to drop a database
#                                    that already holds memories.
#   ./setup.sh status                Service state, hub health, per-node counts.
#   ./setup.sh update                git pull, rebuild the hub image, restart.
#
# Flags:
#   --sqlite     install the SQLite backend: one container, no Postgres. Not a
#                migration path — SQLite and Postgres are different deployments
#                (see docs/adr/0015), so choose before you have data.
#   --force      allow restore to drop a non-empty database
#   --dry-run    print mutating commands instead of executing them (install)
#
# Layout it manages:
#   ~/remind-me-hub/postgres.env       Postgres credentials   (chmod 600)
#   ~/remind-me-hub/hub.env            DATABASE_URL + SYNC_SECRET (chmod 600)
#   ~/remind-me-hub/postgres-data/     Postgres data directory (bind mount)
#   ~/remind-me-hub/data/              SQLite database        (--sqlite only)
#   ~/.config/containers/systemd/      Quadlet units (postgres, hub, network)

set -euo pipefail

HUB_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# The workspace root, not the crate: the image build context must include the
# root Cargo.toml and Cargo.lock. This is the main structural difference from
# the reference's setup.sh, whose context was the hub directory alone.
REPO_DIR="$(cd "$HUB_DIR/../.." && pwd)"
DATA_DIR="${REMIND_ME_HUB_DATA:-$HOME/remind-me-hub}"
QUADLET_DIR="$HOME/.config/containers/systemd"

FORCE=0
DRY_RUN=0
SQLITE=0

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

psql_in() {  # psql_in <db> [psql args...]
    local db="$1"; shift
    podman exec -i remind-me-postgres psql -U remindme -d "$db" "$@"
}

wait_for_postgres() {
    # `_` rather than a named counter: this is a repeat loop, not an iteration
    # over anything, and a named-but-unused variable trips shellcheck SC2034.
    local _
    for _ in $(seq 1 60); do
        if podman exec remind-me-postgres pg_isready -U remindme >/dev/null 2>&1; then
            return 0
        fi
        sleep 1
    done
    die "Postgres did not become ready within 60s"
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
    run mkdir -p "$QUADLET_DIR"

    if (( SQLITE )); then
        run mkdir -p "$DATA_DIR/data"
        if [ -f "$DATA_DIR/hub.env" ]; then
            log "Keeping existing $DATA_DIR/hub.env"
        else
            log "Generating $DATA_DIR/hub.env (SQLite backend)"
            if ! (( DRY_RUN )); then
                cat > "$DATA_DIR/hub.env" <<EOF
REMIND_ME_HUB_DB_PATH=/data/hub.db
SYNC_SECRET=$(rand_hex 32)
EOF
                chmod 600 "$DATA_DIR/hub.env"
            fi
        fi
        return
    fi

    run mkdir -p "$DATA_DIR/postgres-data"

    local pgpw
    if [ -f "$DATA_DIR/postgres.env" ]; then
        pgpw=$(env_value "$DATA_DIR/postgres.env" POSTGRES_PASSWORD)
        log "Keeping existing $DATA_DIR/postgres.env"
    else
        pgpw=$(rand_hex 24)
        log "Generating $DATA_DIR/postgres.env"
        if ! (( DRY_RUN )); then
            cat > "$DATA_DIR/postgres.env" <<EOF
POSTGRES_USER=remindme
POSTGRES_PASSWORD=$pgpw
POSTGRES_DB=remindme
EOF
            chmod 600 "$DATA_DIR/postgres.env"
        fi
    fi

    if [ -f "$DATA_DIR/hub.env" ]; then
        log "Keeping existing $DATA_DIR/hub.env"
    else
        log "Generating $DATA_DIR/hub.env"
        if ! (( DRY_RUN )); then
            cat > "$DATA_DIR/hub.env" <<EOF
DATABASE_URL=postgresql://remindme:$pgpw@remind-me-postgres:5432/remindme
SYNC_SECRET=$(rand_hex 32)
EOF
            chmod 600 "$DATA_DIR/hub.env"
        fi
    fi
}

install_quadlets() {
    log "Installing Quadlet units to $QUADLET_DIR"
    if (( SQLITE )); then
        # Installed under the same unit name as the Postgres variant, so
        # `systemctl --user start remind-me-hub` is the same command either way.
        run cp "$HUB_DIR/deploy/remind-me-hub-sqlite.container" \
               "$QUADLET_DIR/remind-me-hub.container"
    else
        run cp "$HUB_DIR/deploy/remind-me.network" \
               "$HUB_DIR/deploy/remind-me-postgres.container" \
               "$HUB_DIR/deploy/remind-me-hub.container" \
               "$QUADLET_DIR/"
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
    run podman build \
        --build-arg "HUB_VERSION=$version" \
        -f "$HUB_DIR/Containerfile" \
        -t "remind-me-hub:$version" \
        -t remind-me-hub:latest \
        "$REPO_DIR"
}

start_services() {
    if (( SQLITE )); then
        log "Starting the hub"
        run systemctl --user start remind-me-hub.service
    else
        log "Starting Postgres"
        run systemctl --user start remind-me-postgres.service
        (( DRY_RUN )) || wait_for_postgres
        log "Starting the hub"
        run systemctl --user start remind-me-hub.service
    fi
    (( DRY_RUN )) || wait_for_hub || warn "the hub did not answer $HEALTH_URL within 60s; check: journalctl --user -u remind-me-hub -n 50"
}

cmd_install() {
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

Server setup complete$( (( SQLITE )) && printf ' (SQLite backend)' ).

Sync secret (clients need this as REMIND_ME_SYNC_SECRET):
  $secret

Next steps:
  - configure a client:  run crates/remind_me_hub/client-setup.sh on each client
  - check anytime:       $0 status
EOF
    (( SQLITE )) || printf '  - restore a backup:    %s restore /path/to/postgres-backup.sql\n' "$0"
}

cmd_restore() {
    local dump="${1:-}"
    [[ -n "$dump" ]] || die "usage: $0 restore <dump.sql> [--force]"
    [[ -f "$dump" ]] || die "no such file: $dump"
    (( SQLITE )) && die "restore is Postgres-only; the SQLite backend has no dump format in common with it"

    podman container exists remind-me-postgres \
        || die "remind-me-postgres is not running; run '$0 install' first"
    wait_for_postgres

    local existing
    existing=$(psql_in remindme -At -c \
        "SELECT COUNT(*) FROM memories" 2>/dev/null || echo 0)
    if [[ "$existing" != "0" ]] && (( ! FORCE )); then
        die "database already holds $existing memories; re-run with --force to drop it"
    fi

    log "Stopping the hub so nothing writes during the restore"
    systemctl --user stop remind-me-hub.service || true

    log "Recreating the database"
    psql_in postgres -c "DROP DATABASE IF EXISTS remindme" >/dev/null
    psql_in postgres -c "CREATE DATABASE remindme OWNER remindme" >/dev/null

    log "Restoring $dump"
    podman exec -i remind-me-postgres psql -U remindme -d remindme < "$dump" >/dev/null

    # The hub migrates a legacy dump in place on startup: TIMESTAMPTZ columns
    # become canonical TEXT, missing columns are added, hub_seq is backfilled.
    log "Starting the hub (it will migrate the restored schema on startup)"
    systemctl --user start remind-me-hub.service
    wait_for_hub || die "the hub did not come back up; check: journalctl --user -u remind-me-hub -n 50"
    log "Restored. $(curl -fsS "$HEALTH_URL")"
}

cmd_status() {
    local units=(remind-me-hub.service)
    (( SQLITE )) || units=(remind-me-postgres.service remind-me-hub.service)
    local unit
    for unit in "${units[@]}"; do
        printf '%-32s %s\n' "$unit" "$(systemctl --user is-active "$unit" 2>/dev/null || echo inactive)"
    done

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
            --sqlite)  SQLITE=1 ;;
            -h|--help) sed -n '2,26p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
            install|restore|status|update)
                cmd="$1" ;;
            *)  args+=("$1") ;;
        esac
        shift
    done

    _set_health_url

    case "${cmd:-install}" in
        install) cmd_install ;;
        restore) cmd_restore "${args[@]:-}" ;;
        status)  cmd_status ;;
        update)  cmd_update ;;
    esac
}

main "$@"
