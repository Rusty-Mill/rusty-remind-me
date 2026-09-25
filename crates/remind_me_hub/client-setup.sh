#!/usr/bin/env bash
# Remind Me — configure a client machine for hub sync.
#
# Usage:
#   ./client-setup.sh --node-id <id> [options]
#
# Options:
#   --node-id ID        Unique id for this machine (e.g. home-pc-wsl). Required.
#   --secret HEX        SYNC_SECRET from the server's hub.env. Prompted if absent.
#   --hub-url URL       Hub URL as seen from this machine
#                       (default http://127.0.0.1:8765 — the tunnel's local end).
#                       https:// works as-is when something terminates TLS in
#                       front of the hub.
#   --tunnel USER@HOST[:PORT]
#                       Also set up a persistent SSH tunnel to the hub server:
#                       dedicated key, ~/.ssh/config block, systemd user service.
#   --peer-port N       Local peer-sync port (default 8766; use a different
#                       port on each machine that shares a network).
#   --db-path PATH      Node database (default ~/.remind_me/remind_me.db).
#   --apply-code        Merge the MCP server entry into ~/.claude.json
#                       (Claude Code). A timestamped backup is written first.
#
# This script owns the parts that are genuinely a shell's job: prompting for
# the secret without echoing it, the SSH tunnel, and Claude Code's
# ~/.claude.json (which holds a lot of unrelated state, so it is merged with a
# backup rather than rewritten).
#
# Everything else is delegated to `rusty-remind-me configure`, which writes
# the entry — sync environment included — for Claude Desktop, Cursor,
# Antigravity and generic MCP clients. The entry is built in exactly one
# place; this script reads back what configure wrote rather than constructing
# a second copy that could drift from it.

set -euo pipefail

NODE_ID=""
SECRET="${REMIND_ME_SYNC_SECRET:-}"
HUB_URL="http://127.0.0.1:8765"
PEER_PORT="8766"
DB_PATH="$HOME/.remind_me/remind_me.db"
TUNNEL=""
APPLY_CODE=0
SECRET_FROM_ARGV=0

log()  { printf '\033[1;32m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33mwarning:\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

while (( $# )); do
    case "$1" in
        --node-id)    NODE_ID="${2:-}"; shift 2 ;;
        --secret)     SECRET="${2:-}"; SECRET_FROM_ARGV=1; shift 2 ;;
        --hub-url)    HUB_URL="${2:-}"; shift 2 ;;
        --peer-port)  PEER_PORT="${2:-}"; shift 2 ;;
        --db-path)    DB_PATH="${2:-}"; shift 2 ;;
        --tunnel)     TUNNEL="${2:-}"; shift 2 ;;
        --apply-code) APPLY_CODE=1; shift ;;
        -h|--help)    sed -n '2,28p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *)            die "unknown option: $1" ;;
    esac
done

[[ -n "$NODE_ID" ]] || die "--node-id is required (see --help)"

if [[ -z "$SECRET" ]]; then
    # Read without echo: this is the credential that stands between the
    # internet and the whole memory database, and a shell history entry is
    # forever.
    read -r -s -p "SYNC_SECRET (from the server's hub.env): " SECRET
    printf '\n'
fi
[[ -n "$SECRET" ]] || die "a sync secret is required"
if (( SECRET_FROM_ARGV )); then
    # Kept because it is the documented interface, but the same argument that
    # made `configure` refuse a --secret flag applies here: argv is readable
    # through /proc and lands in shell history. Prompting or
    # REMIND_ME_SYNC_SECRET avoids both.
    warn "--secret puts the token in argv and shell history; prefer REMIND_ME_SYNC_SECRET or the prompt"
fi

# ---------------------------------------------------------------------------
# 1. The binary
# ---------------------------------------------------------------------------

BIN="$(command -v rusty-remind-me || true)"
if [[ -z "$BIN" ]]; then
    # A cargo-installed binary is the common case and is often not on PATH for
    # a non-login shell, which is what a client app launches the server from.
    if [[ -x "$HOME/.cargo/bin/rusty-remind-me" ]]; then
        BIN="$HOME/.cargo/bin/rusty-remind-me"
    else
        die "rusty-remind-me is not on PATH. Install it first: cargo install --path crates/remind_me_cli"
    fi
fi
log "Using $BIN"

mkdir -p "$(dirname "$DB_PATH")"

# ---------------------------------------------------------------------------
# 2. SSH tunnel (optional)
# ---------------------------------------------------------------------------

if [[ -n "$TUNNEL" ]]; then
    TUNNEL_USERHOST="${TUNNEL%%:*}"
    TUNNEL_PORT="22"
    [[ "$TUNNEL" == *:* ]] && TUNNEL_PORT="${TUNNEL##*:}"
    SSH_KEY="$HOME/.ssh/remind_me_tunnel"
    SSH_HOST_ALIAS="remind-me-hub"
    TUNNEL_SERVICE="remind-me-tunnel.service"

    if [[ ! -f "$SSH_KEY" ]]; then
        log "Generating a dedicated tunnel key at $SSH_KEY"
        ssh-keygen -t ed25519 -N "" -f "$SSH_KEY" -C "remind-me-tunnel@$NODE_ID" >/dev/null
    fi

    mkdir -p "$HOME/.ssh" && chmod 700 "$HOME/.ssh"
    if ! grep -q "^Host $SSH_HOST_ALIAS\$" "$HOME/.ssh/config" 2>/dev/null; then
        log "Adding a ~/.ssh/config block for $SSH_HOST_ALIAS"
        cat >> "$HOME/.ssh/config" <<EOF

Host $SSH_HOST_ALIAS
    HostName ${TUNNEL_USERHOST##*@}
    User ${TUNNEL_USERHOST%%@*}
    Port $TUNNEL_PORT
    IdentityFile $SSH_KEY
    IdentitiesOnly yes
    LocalForward 8765 127.0.0.1:8765
    ExitOnForwardFailure yes
    ServerAliveInterval 30
    ServerAliveCountMax 3
EOF
        chmod 600 "$HOME/.ssh/config"
    else
        log "Keeping the existing $SSH_HOST_ALIAS block in ~/.ssh/config"
    fi

    mkdir -p "$HOME/.config/systemd/user"
    cat > "$HOME/.config/systemd/user/$TUNNEL_SERVICE" <<EOF
[Unit]
Description=Remind Me SSH tunnel to sync hub
After=network-online.target
Wants=network-online.target
StartLimitIntervalSec=300
StartLimitBurst=5

[Service]
Type=simple
ExecStart=$(command -v ssh) -N $SSH_HOST_ALIAS
Restart=on-failure
RestartSec=30

[Install]
WantedBy=default.target
EOF
    systemctl --user daemon-reload
    if ssh -o BatchMode=yes -o ConnectTimeout=5 "$SSH_HOST_ALIAS" true 2>/dev/null; then
        systemctl --user enable --now "$TUNNEL_SERVICE"
        log "Tunnel service running"
    else
        systemctl --user enable "$TUNNEL_SERVICE" 2>/dev/null || true
        warn "cannot authenticate to $TUNNEL_USERHOST yet — authorize the key, then start the tunnel:"
        printf '    ssh-copy-id -i %s.pub -p %s %s\n' "$SSH_KEY" "$TUNNEL_PORT" "$TUNNEL_USERHOST"
        printf '    systemctl --user start %s\n' "$TUNNEL_SERVICE"
    fi
fi

# ---------------------------------------------------------------------------
# 3. Hub connectivity
# ---------------------------------------------------------------------------

if curl -fsS --max-time 3 "$HUB_URL/health" >/dev/null 2>&1; then
    log "Hub reachable at $HUB_URL"
    # Reported so a version mismatch is visible now rather than as a puzzling
    # 404 later — clients probe for the 404 to detect capabilities.
    curl -fsS --max-time 3 "$HUB_URL/health" 2>/dev/null \
        | sed -n 's/.*"version"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/    hub version: \1/p'
else
    warn "hub is NOT answering $HUB_URL/health yet — sync will retry every cycle once it is (tunnel up? server running?)"
fi

# ---------------------------------------------------------------------------
# 4. MCP config
# ---------------------------------------------------------------------------

log "Writing MCP entries via rusty-remind-me configure"
CONFIGURE_ARGS=(--node-id "$NODE_ID" --hub-url "$HUB_URL" --peer-port "$PEER_PORT" --db-path "$DB_PATH")
# The secret goes through the environment, never argv: `configure` refuses a
# --secret flag precisely because argv is world-readable through /proc.
REMIND_ME_SYNC_SECRET="$SECRET" "$BIN" configure "${CONFIGURE_ARGS[@]}"

# Claude Code is not one of configure's targets: ~/.claude.json carries a great
# deal of unrelated state, so it is merged in place with a backup rather than
# written. The entry itself is READ BACK from what configure just wrote, so
# there is one definition of the sync environment rather than two that can
# drift -- only REMIND_ME_CLIENT differs, since this is a different client.
GENERIC_CONFIG="$HOME/.mcp/config.json"
[[ -f "$GENERIC_CONFIG" ]] || die "configure did not write $GENERIC_CONFIG; cannot derive the Claude Code entry"

if (( ! APPLY_CODE )); then
    echo
    log "Claude Code — entry for ~/.claude.json under \"mcpServers\" -> \"rusty-remind-me\":"
    python3 - "$GENERIC_CONFIG" <<'PYEOF'
import json, sys
entry = json.load(open(sys.argv[1]))["mcpServers"]["rusty-remind-me"]
entry.setdefault("env", {})["REMIND_ME_CLIENT"] = "claude-code"
# Shown with the secret redacted: this goes to a terminal, and the point here
# is the shape of the entry, not the credential the operator already has.
shown = json.loads(json.dumps(entry))
if shown["env"].get("REMIND_ME_SYNC_SECRET"):
    shown["env"]["REMIND_ME_SYNC_SECRET"] = "<REMIND_ME_SYNC_SECRET>"
print(json.dumps(shown, indent=2))
PYEOF
    printf '\n    Re-run with --apply-code to merge it in automatically.\n'
else

log "Merging the same entry into ~/.claude.json (Claude Code)"
python3 - "$GENERIC_CONFIG" <<'PYEOF'
import json, os, shutil, sys, time

source = sys.argv[1]
with open(source) as f:
    entry = json.load(f)["mcpServers"]["rusty-remind-me"]
# The one field that is genuinely per-client.
entry.setdefault("env", {})["REMIND_ME_CLIENT"] = "claude-code"

path = os.path.expanduser("~/.claude.json")
cfg = {}
if os.path.exists(path):
    # Backup before touching a file we did not write and do not own.
    shutil.copy2(path, f"{path}.bak.{int(time.time())}")
    with open(path) as f:
        cfg = json.load(f)
cfg.setdefault("mcpServers", {})["rusty-remind-me"] = entry
with open(path, "w") as f:
    json.dump(cfg, f, indent=2)
    f.write("\n")
print(f"    wrote {path}")
PYEOF
fi

cat <<EOF

Client setup complete for node "$NODE_ID".

  database:   $DB_PATH
  hub:        $HUB_URL
  peer port:  $PEER_PORT

Sync turns on only when NODE_ID, HUB_URL and SYNC_SECRET are all set — the
entry above carries all three. Verify once the client has restarted, by
calling the remind_me_sync_status tool (or remind_me_server_status for the
fuller picture). This CLI has no sync-status subcommand: unlike the reference
it exposes memory operations only, so sync state is read through the tool
surface rather than the command line.
EOF
