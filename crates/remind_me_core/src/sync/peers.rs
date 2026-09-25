//! Peer discovery: a static peer list plus Tailscale's local API — the
//! third sync slice. Verified against the reference's `sync.py` "Peer
//! discovery" section directly before writing any of this: there is no
//! dedicated `tailscale.py` module there either, it's a plain HTTP-over-
//! Unix-socket call to `GET /localapi/v0/status`, and the reference's own
//! test suite never touches a real `tailscaled` — every test fakes the
//! transport. This module does the same, gated behind a fake `UnixListener`
//! in its own tests rather than a real daemon.

// Only the Tailscale status structs derive it, and those are `cfg(unix)`.
#[cfg(unix)]
use serde::Deserialize;
use std::collections::HashSet;

/// A remote this node can sync with, beyond the single configured hub.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Peer {
    pub node_id: String,
    pub url: String,
}

/// `REMIND_ME_STATIC_PEERS`: a JSON array of `{"node_id": ..., "url": ...}`
/// objects. Unlike the reference (which lets a malformed *value* for this
/// env var crash the process at import time — arguably a defect, not a
/// design choice worth preserving), this degrades to an empty list instead,
/// matching this crate's consistent graceful-degradation posture for every
/// other optional feature. A malformed *entry* within an otherwise-valid
/// array is skipped individually, matching the reference exactly either way.
pub const STATIC_PEERS_ENV: &str = "REMIND_ME_STATIC_PEERS";

fn parse_static_peers() -> Vec<Peer> {
    let raw = std::env::var(STATIC_PEERS_ENV).unwrap_or_default();
    if raw.trim().is_empty() {
        return Vec::new();
    }
    let Ok(serde_json::Value::Array(entries)) = serde_json::from_str(&raw) else {
        return Vec::new();
    };
    entries
        .into_iter()
        .filter_map(|entry| {
            let node_id = entry.get("node_id")?.as_str()?.to_string();
            let url = entry
                .get("url")?
                .as_str()?
                .trim_end_matches('/')
                .to_string();
            Some(Peer { node_id, url })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Tailscale local API (Unix-only; a no-op elsewhere)
// ---------------------------------------------------------------------------

pub const TAILSCALE_SOCKET_ENV: &str = "REMIND_ME_TAILSCALE_SOCKET";

/// Resolve the tailscaled local API socket path: the config override first,
/// then the platform default (`/var/run/tailscaled.socket` on macOS,
/// `/var/run/tailscale/tailscaled.sock` elsewhere) -- matching the
/// reference's own `_tailscale_socket()` exactly.
///
/// Unix-only, like everything else in this section: the local API is reached
/// over a Unix domain socket, so on Windows there is nothing to resolve a path
/// *for*. Gated rather than merely unused so the Windows build stays clean.
#[cfg(unix)]
fn tailscale_socket_path() -> String {
    if let Ok(configured) = std::env::var(TAILSCALE_SOCKET_ENV) {
        if !configured.is_empty() {
            return configured;
        }
    }
    if cfg!(target_os = "macos") {
        "/var/run/tailscaled.socket".to_string()
    } else {
        "/var/run/tailscale/tailscaled.sock".to_string()
    }
}

#[cfg(unix)]
#[derive(Debug, Deserialize)]
struct TailscalePeerInfo {
    #[serde(default)]
    #[serde(rename = "Online")]
    online: bool,
    #[serde(default)]
    #[serde(rename = "TailscaleIPs")]
    tailscale_ips: Vec<String>,
    #[serde(default)]
    #[serde(rename = "HostName")]
    host_name: String,
}

#[cfg(unix)]
#[derive(Debug, Deserialize)]
struct TailscaleStatus {
    #[serde(default)]
    #[serde(rename = "Peer")]
    peer: std::collections::HashMap<String, TailscalePeerInfo>,
}

/// This node's own configured `REMIND_ME_PEER_PORT` (or the default),
/// which every Tailscale-discovered peer is assumed to also be listening
/// on -- the reference makes the same assumption (there is no per-peer
/// port discovery; every remind_me install uses the same default unless
/// its operator overrides it identically everywhere).
#[cfg(unix)]
fn peer_port() -> u16 {
    std::env::var(super::PEER_PORT_ENV)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(super::DEFAULT_PEER_PORT)
}

#[cfg(unix)]
mod unix_socket {
    use super::TailscaleStatus;
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    const CONNECT_IO_TIMEOUT: Duration = Duration::from_secs(3);

    /// `GET /localapi/v0/status` over the Tailscale local API's Unix
    /// socket. The `Host` header value is a dummy -- routing is entirely
    /// via the socket path itself, matching the reference's own
    /// `http://local-tailscaled.sock/...` placeholder URL.
    pub(super) fn query_status(socket_path: &str) -> Option<TailscaleStatus> {
        let mut stream = UnixStream::connect(socket_path).ok()?;
        stream.set_read_timeout(Some(CONNECT_IO_TIMEOUT)).ok();
        stream.set_write_timeout(Some(CONNECT_IO_TIMEOUT)).ok();
        stream
            .write_all(
                b"GET /localapi/v0/status HTTP/1.1\r\nHost: local-tailscaled.sock\r\nConnection: close\r\n\r\n",
            )
            .ok()?;
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).ok()?;
        let text = String::from_utf8_lossy(&raw);
        let (_head, body) = text.split_once("\r\n\r\n")?;
        serde_json::from_str::<TailscaleStatus>(body).ok()
    }
}

/// Every online Tailscale peer with at least one address, addressed at
/// `http://{first_ip}:{peer_port}` -- no tag or hostname-pattern filter at
/// discovery time, matching the reference exactly. Whether a discovered
/// address is genuinely a remind_me instance is decided later, by probing
/// `/health` before syncing with it (see `probe_peer`), not here.
///
/// Any failure reaching or parsing the Tailscale local API (no socket, no
/// daemon running, not a Tailscale node at all) degrades to an empty list
/// silently -- this crate has no Tailscale dependency unless the operator
/// actually has Tailscale running, exactly like every other optional
/// feature in this crate degrades when its prerequisite is absent.
#[cfg(unix)]
fn tailscale_peers() -> Vec<Peer> {
    let Some(status) = unix_socket::query_status(&tailscale_socket_path()) else {
        return Vec::new();
    };
    let port = peer_port();
    status
        .peer
        .into_iter()
        .filter(|(_, info)| info.online && !info.tailscale_ips.is_empty())
        .map(|(name, info)| Peer {
            node_id: if info.host_name.is_empty() {
                name
            } else {
                info.host_name
            },
            url: format!("http://{}:{}", info.tailscale_ips[0], port),
        })
        .collect()
}

/// Tailscale's local API is reached over a Unix domain socket, which has
/// no direct equivalent on this platform -- degrades to "no Tailscale
/// peers," the same outcome a Unix machine gets when Tailscale itself
/// isn't installed.
#[cfg(not(unix))]
fn tailscale_peers() -> Vec<Peer> {
    Vec::new()
}

/// Every peer this node might sync with beyond the configured hub: static
/// peers first (so a static entry wins a URL collision), then Tailscale-
/// discovered peers not already covered by a static one -- confirmed
/// against the reference's own dedup-by-URL, static-peers-seed-first order
/// (`test_discover_peers_parses_tailscale_status`'s `static-dup` case).
pub fn discover_peers() -> Vec<Peer> {
    let mut seen_urls: HashSet<String> = HashSet::new();
    let mut peers = Vec::new();

    for peer in parse_static_peers() {
        if seen_urls.insert(peer.url.clone()) {
            peers.push(peer);
        }
    }
    for peer in tailscale_peers() {
        if seen_urls.insert(peer.url.clone()) {
            peers.push(peer);
        }
    }

    peers
}

/// Whether `url` answers `/health` with the configured secret -- the only
/// "is this actually a remind_me peer" check the reference makes, applied
/// uniformly to every discovered peer (static or Tailscale-sourced) right
/// before syncing with it, not at discovery time.
pub fn probe_peer(url: &str, secret: &str) -> bool {
    let health_url = format!("{}/health", url.trim_end_matches('/'));
    matches!(super::http::get(&health_url, secret), Ok((status, _)) if (200..300).contains(&status))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // `STATIC_PEERS_ENV`/`TAILSCALE_SOCKET_ENV` are process-global and these
    // unit tests run concurrently with each other by default -- unlike this
    // crate's integration tests (which already hold an `ENV_LOCK` for this
    // exact reason), these predate that convention and raced each other
    // intermittently. Same fix, applied here too.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn parse_static_peers_reads_well_formed_entries() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        crate::test_env::set_var(
            STATIC_PEERS_ENV,
            r#"[{"node_id":"laptop","url":"http://100.64.0.9:8766"}]"#,
        );
        let peers = parse_static_peers();
        crate::test_env::remove_var(STATIC_PEERS_ENV);
        assert_eq!(
            peers,
            vec![Peer {
                node_id: "laptop".to_string(),
                url: "http://100.64.0.9:8766".to_string()
            }]
        );
    }

    #[test]
    fn parse_static_peers_skips_malformed_entries() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        crate::test_env::set_var(
            STATIC_PEERS_ENV,
            r#"["not-a-dict", {"node_id":"x"}, {"url":"http://ok:1"}, 42]"#,
        );
        let peers = parse_static_peers();
        crate::test_env::remove_var(STATIC_PEERS_ENV);
        assert!(peers.is_empty());
    }

    #[test]
    fn parse_static_peers_degrades_to_empty_on_malformed_json_rather_than_panicking() {
        // Deliberate divergence from the reference, which lets this crash
        // the whole process at import time -- documented in ADR-0006.
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        crate::test_env::set_var(STATIC_PEERS_ENV, "not json at all");
        let peers = parse_static_peers();
        crate::test_env::remove_var(STATIC_PEERS_ENV);
        assert!(peers.is_empty());
    }

    #[test]
    fn parse_static_peers_is_empty_when_unset() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        crate::test_env::remove_var(STATIC_PEERS_ENV);
        assert!(parse_static_peers().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn tailscale_socket_path_honors_the_env_override() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        crate::test_env::set_var(TAILSCALE_SOCKET_ENV, "/tmp/custom.sock");
        assert_eq!(tailscale_socket_path(), "/tmp/custom.sock");
        crate::test_env::remove_var(TAILSCALE_SOCKET_ENV);
    }
}
