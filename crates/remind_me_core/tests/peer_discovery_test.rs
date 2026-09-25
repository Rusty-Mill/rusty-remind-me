//! Coverage for peer discovery: the static peer list, a faked Tailscale
//! local API (over a real `UnixListener`, standing in for `tailscaled` --
//! the reference's own test suite never touches a real daemon either), and
//! `probe_peer`'s health check.
//!
//! `REMIND_ME_STATIC_PEERS`/`REMIND_ME_TAILSCALE_SOCKET`/
//! `REMIND_ME_PEER_PORT` are process-global; every test here holds
//! `ENV_LOCK` for its duration, the same convention `mempalace_import_test.rs`
//! established.

#![cfg(unix)]

#[path = "../src/test_env.rs"]
mod test_env;

use remind_me_core::sync::{
    discover_peers, probe_peer, Peer, PeerServerConfig, STATIC_PEERS_ENV, TAILSCALE_SOCKET_ENV,
};
use remind_me_core::Database;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::net::UnixListener;
use std::sync::Mutex;

static ENV_LOCK: Mutex<()> = Mutex::new(());

fn clear_env() {
    crate::test_env::remove_var(STATIC_PEERS_ENV);
    crate::test_env::remove_var(TAILSCALE_SOCKET_ENV);
    crate::test_env::remove_var("REMIND_ME_PEER_PORT");
}

/// Serves one `GET /localapi/v0/status` request over a Unix socket at a
/// fresh temp path, replying with `body`, and returns that path.
fn fake_tailscale_socket(tag: &str, body: &str) -> String {
    let path = std::env::temp_dir().join(
        format!(
            "rmm_ts_{tag}_{}_{:?}",
            std::process::id(),
            std::thread::current().id()
        )
        .replace(['(', ')', ' '], ""),
    );
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).unwrap();
    let body = body.to_string();
    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    path.to_string_lossy().to_string()
}

#[test]
fn discover_peers_is_empty_when_nothing_is_configured() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_env();
    // No static peers, and a Tailscale socket path that doesn't exist --
    // the same degraded state as a machine with no Tailscale installed.
    crate::test_env::set_var(TAILSCALE_SOCKET_ENV, "/nonexistent/path/to/a.sock");

    let peers = discover_peers();

    clear_env();
    assert!(peers.is_empty());
}

#[test]
fn discover_peers_reads_a_well_formed_static_peer() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_env();
    crate::test_env::set_var(
        STATIC_PEERS_ENV,
        r#"[{"node_id":"laptop","url":"http://100.64.0.9:8766"}]"#,
    );
    crate::test_env::set_var(TAILSCALE_SOCKET_ENV, "/nonexistent/path/to/a.sock");

    let peers = discover_peers();

    clear_env();
    assert_eq!(
        peers,
        vec![Peer {
            node_id: "laptop".to_string(),
            url: "http://100.64.0.9:8766".to_string()
        }]
    );
}

#[test]
fn discover_peers_parses_tailscale_status_filtering_offline_and_ipless_peers() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_env();
    // Mirrors the reference's own test fixture exactly: p1 online+addressed
    // (kept), p2 offline (dropped), p3 online but no address (dropped).
    let status = serde_json::json!({
        "Peer": {
            "p1": { "Online": true, "TailscaleIPs": ["100.64.0.1"], "HostName": "alpha" },
            "p2": { "Online": false, "TailscaleIPs": ["100.64.0.2"], "HostName": "off" },
            "p3": { "Online": true, "TailscaleIPs": [], "HostName": "no-ip" },
        }
    })
    .to_string();
    let socket = fake_tailscale_socket("filter", &status);
    crate::test_env::set_var(TAILSCALE_SOCKET_ENV, &socket);
    crate::test_env::set_var("REMIND_ME_PEER_PORT", "8766");

    let peers = discover_peers();

    clear_env();
    let _ = std::fs::remove_file(&socket);
    assert_eq!(
        peers,
        vec![Peer {
            node_id: "alpha".to_string(),
            url: "http://100.64.0.1:8766".to_string()
        }]
    );
}

#[test]
fn discover_peers_a_static_entry_wins_a_url_collision_with_a_tailscale_peer() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_env();
    let status = serde_json::json!({
        "Peer": {
            "p4": { "Online": true, "TailscaleIPs": ["100.64.0.4"], "HostName": "tailscale-name" },
        }
    })
    .to_string();
    let socket = fake_tailscale_socket("collision", &status);
    crate::test_env::set_var(TAILSCALE_SOCKET_ENV, &socket);
    crate::test_env::set_var("REMIND_ME_PEER_PORT", "8766");
    crate::test_env::set_var(
        STATIC_PEERS_ENV,
        r#"[{"node_id":"static-dup","url":"http://100.64.0.4:8766"}]"#,
    );

    let peers = discover_peers();

    clear_env();
    let _ = std::fs::remove_file(&socket);
    assert_eq!(
        peers,
        vec![Peer { node_id: "static-dup".to_string(), url: "http://100.64.0.4:8766".to_string() }],
        "the static entry wins the collision -- the Tailscale-sourced duplicate is dropped, not added twice"
    );
}

#[test]
fn discover_peers_falls_back_to_the_dict_key_when_hostname_is_absent() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_env();
    let status = serde_json::json!({
        "Peer": { "p5": { "Online": true, "TailscaleIPs": ["100.64.0.5"] } }
    })
    .to_string();
    let socket = fake_tailscale_socket("nohostname", &status);
    crate::test_env::set_var(TAILSCALE_SOCKET_ENV, &socket);
    crate::test_env::set_var("REMIND_ME_PEER_PORT", "8766");

    let peers = discover_peers();

    clear_env();
    let _ = std::fs::remove_file(&socket);
    assert_eq!(
        peers,
        vec![Peer {
            node_id: "p5".to_string(),
            url: "http://100.64.0.5:8766".to_string()
        }]
    );
}

// ---------------------------------------------------------------------------
// probe_peer
// ---------------------------------------------------------------------------

const SECRET: &str = "probe-secret";

#[test]
fn probe_peer_is_true_for_a_healthy_peer() {
    let db = Database::open_in_memory().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let config = PeerServerConfig::new("127.0.0.1", port, SECRET, "peer-node");
    let handle = std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let conn = db.conn();
            let _ = remind_me_core::sync::serve_once(&mut stream, &config, &conn);
        }
    });

    let healthy = probe_peer(&format!("http://127.0.0.1:{port}"), SECRET);

    handle.join().unwrap();
    assert!(healthy);
}

#[test]
fn probe_peer_is_false_when_unreachable() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);

    assert!(!probe_peer(&format!("http://127.0.0.1:{port}"), SECRET));
}

#[test]
fn probe_peer_is_false_with_the_wrong_secret() {
    let db = Database::open_in_memory().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let config = PeerServerConfig::new("127.0.0.1", port, SECRET, "peer-node");
    let handle = std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let conn = db.conn();
            let _ = remind_me_core::sync::serve_once(&mut stream, &config, &conn);
        }
    });

    let healthy = probe_peer(&format!("http://127.0.0.1:{port}"), "wrong-secret");

    handle.join().unwrap();
    assert!(!healthy);
}
