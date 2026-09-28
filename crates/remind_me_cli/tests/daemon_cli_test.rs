//! End-to-end coverage for the store daemon (ADR-0023 phase 2).
//!
//! These run the real binary with `REMIND_ME_DAEMON=1` (and `0` for the
//! in-process comparison), so the first client starts a real detached daemon and every later one talks to it over
//! loopback. Each test has its own database directory, so its own daemon,
//! and stops it on the way out, pass or fail.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_rusty-remind-me");

/// A scratch store whose daemon is stopped on drop.
struct Scratch {
    db: PathBuf,
}

impl Scratch {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "rrm-daemon-test-{}-{}-{}",
            name,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self {
            db: dir.join("remind_me.db"),
        }
    }

    fn command(&self, daemon: bool) -> Command {
        let mut command = Command::new(BIN);
        command
            .env("REMIND_ME_DB_PATH", &self.db)
            .env("REMIND_ME_AUTO_UPDATE_CHECK", "0");
        if daemon {
            command.env("REMIND_ME_DAEMON", "1");
        } else {
            command.env("REMIND_ME_DAEMON", "0");
        }
        command
    }

    fn run(&self, daemon: bool, args: &[&str]) -> Output {
        self.command(daemon).args(args).output().unwrap()
    }

    fn run_ok(&self, daemon: bool, args: &[&str]) -> String {
        let out = self.run(daemon, args);
        assert!(
            out.status.success(),
            "{args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        if daemon {
            assert!(
                !stderr.contains("not using the store daemon"),
                "{args:?} fell back: {stderr}"
            );
        }
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn daemon_status(&self) -> Option<Value> {
        let out = self.run(false, &["daemon", "status"]);
        out.status
            .success()
            .then(|| serde_json::from_slice(&out.stdout).unwrap())
    }

    fn dir(&self) -> &Path {
        self.db.parent().unwrap()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = self.run(false, &["daemon", "stop"]);
    }
}

/// Whether the binary under test keeps its store on the engine: its
/// default, unless `REMIND_ME_STORE=sqlite` (which it inherits).
fn on_engine() -> bool {
    !std::env::var("REMIND_ME_STORE").is_ok_and(|v| v.trim().eq_ignore_ascii_case("sqlite"))
}

#[test]
fn cli_commands_through_the_daemon_print_what_they_print_in_process() {
    let store = Scratch::new("cli");
    assert!(
        store.daemon_status().is_none(),
        "nothing runs before a client"
    );

    let added = store.run_ok(
        true,
        &["add", "the daemon keeps the store", "--tags", "phase2"],
    );
    assert!(added.starts_with("Added memory: "), "{added}");
    let status = store
        .daemon_status()
        .expect("the first client started a daemon");
    assert_ne!(status["pid"], std::process::id());

    // The same bytes either way. The engine store has one opener, so the
    // daemon lets go of it before the in-process runs.
    let commands = [
        &["list", "--json"][..],
        &["list"][..],
        &["search", "store", "--json"][..],
        &["get", added.trim().trim_start_matches("Added memory: ")][..],
    ];
    let through_daemon: Vec<String> = commands.iter().map(|a| store.run_ok(true, a)).collect();
    assert_eq!(store.run_ok(false, &["daemon", "stop"]).trim(), "stopped");
    for (args, via_daemon) in commands.iter().zip(&through_daemon) {
        assert_eq!(
            via_daemon,
            &store.run_ok(false, args),
            "{args:?} differs through the daemon"
        );
    }

    store.run_ok(true, &["wiki-write", "p2", "Phase 2", "The daemon."]);
    let page: Value = serde_json::from_str(&store.run_ok(true, &["wiki-read", "p2"])).unwrap();
    assert_eq!(page["title"], "Phase 2");
    let stats: Value = serde_json::from_str(&store.run_ok(true, &["stats"])).unwrap();
    assert_eq!(stats["total_memories"], 1);

    assert_eq!(store.run_ok(false, &["daemon", "stop"]).trim(), "stopped");
    assert!(store.daemon_status().is_none());
}

#[test]
fn a_client_with_other_settings_falls_back_and_says_why() {
    let store = Scratch::new("mismatch");
    store.run_ok(true, &["add", "started under the default settings"]);

    let out = store
        .command(true)
        .env("REMIND_ME_OUTBOX_RETENTION_DAYS", "3")
        .args(["list", "--json"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("not using the store daemon"), "{stderr}");
    assert!(
        stderr.contains("REMIND_ME_OUTBOX_RETENTION_DAYS"),
        "{stderr}"
    );
    if on_engine() {
        // The daemon holds the engine store, so the fallback cannot open
        // it beside the daemon: it fails, and says how to release it.
        assert!(!out.status.success());
        assert!(stderr.contains("rusty-remind-me daemon stop"), "{stderr}");
    } else {
        assert!(out.status.success(), "{stderr}");
        let listed: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(listed["total"], 1, "the fallback still sees the store");
    }

    // A per-session setting is not a mismatch.
    let out = store
        .command(true)
        .env("REMIND_ME_CLIENT", "someone-else")
        .args(["list"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains("not using the store daemon"), "{stderr}");
}

#[test]
fn mcp_over_stdio_relays_to_the_daemon_with_its_own_session() {
    let store = Scratch::new("mcp");
    let mut server = store
        .command(true)
        .arg("server")
        .env("REMIND_ME_CLIENT", "from-env")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = server.stdin.take().unwrap();
    let mut stdout = BufReader::new(server.stdout.take().unwrap());
    let mut ask = |request: Value| -> Value {
        writeln!(stdin, "{request}").unwrap();
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    };

    let init = ask(json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": { "protocolVersion": "2025-06-18",
                    "clientInfo": { "name": "daemon-test", "version": "1" } }
    }));
    assert_eq!(init["result"]["serverInfo"]["name"], "rusty_remind_me");
    let added = ask(json!({
        "jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": { "name": "remind_me_add",
                    "arguments": { "content": "written over MCP through the daemon" } }
    }));
    assert!(added.get("error").is_none(), "{added}");

    assert!(
        store.daemon_status().is_some(),
        "the MCP client started a daemon"
    );
    drop(stdin);
    assert!(server.wait().unwrap().success());

    let listed: Value = serde_json::from_str(&store.run_ok(true, &["list", "--json"])).unwrap();
    let memory = &listed["memories"][0];
    assert_eq!(memory["content"], "written over MCP through the daemon");
    // The handshake identity of this session, not the daemon's environment
    // and not whichever client shook hands last.
    assert_eq!(memory["client"], "daemon-test/1");
}

#[test]
fn the_dashboard_api_is_relayed_to_the_daemon() {
    let store = Scratch::new("api");
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut api = store
        .command(true)
        .args(["api", &port.to_string()])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut banner = String::new();
    BufReader::new(api.stdout.take().unwrap())
        .read_line(&mut banner)
        .unwrap();
    assert!(banner.contains("via the store daemon"), "{banner}");

    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    write!(
        stream,
        "GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");

    api.kill().unwrap();
    let _ = api.wait();
    assert!(store.dir().join("remind_me.db.daemon.json").exists());
}
