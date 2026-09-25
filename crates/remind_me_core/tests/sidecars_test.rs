//! Supervision behaviour for sidecar processes (issue #169).
//!
//! The unit tests in `sidecars.rs` cover parsing and the port probe. These
//! drive real child processes, which is the only way to prove the parts that
//! actually matter operationally: that a live sidecar is not started twice,
//! that a dead one comes back, and that shutdown really kills them.
//!
//! Linux-only, and gated rather than left to fail: the assertions read `/proc`
//! and shell out to `sleep`/`kill`. That matches where CI runs. The module
//! under test is cross-platform; only this test harness is not.
#![cfg(target_os = "linux")]

use remind_me_core::sidecars::{SidecarSpec, Sidecars};
use std::net::TcpListener;
use std::time::{Duration, Instant};

/// A sidecar that just sleeps: long enough to outlive the test, harmless if
/// the teardown assertions are ever wrong.
///
/// `seconds` is unique per test so a leaked sleeper is traceable to the test
/// that leaked it. Nothing keys off it functionally — children are identified
/// by the PID their own supervisor reports, not by command line.
fn sleeper(name: &str, port: u16, seconds: u32) -> SidecarSpec {
    SidecarSpec {
        name: name.to_string(),
        command: vec!["sleep".to_string(), seconds.to_string()],
        host: "127.0.0.1".to_string(),
        port,
        env: Vec::new(),
        wait_for_port: false,
    }
}

/// A port nothing is listening on, so `ensure` always wants to start.
///
/// Fixed and per-test rather than "bind :0 and drop it". That trick returns an
/// *ephemeral* port, which the kernel is then free to hand straight to another
/// parallel test's listener — after which `ensure` sees the port answering,
/// skips the spawn, and the test fails claiming the sidecar did not start.
/// These sit below `ip_local_port_range` (32768–60999 on Linux), so `bind(0)`
/// never allocates one.
const fn closed_port(offset: u16) -> u16 {
    21300 + offset
}

/// Poll until `f` holds, so the test does not depend on how fast the OS
/// reaps a killed process.
fn eventually(mut f: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    false
}

fn pid_alive(pid: u32) -> bool {
    // No `libc::kill` here (see ADR-0012): ask the OS through /proc instead,
    // which is enough for a Linux-only test assertion.
    std::path::Path::new(&format!("/proc/{}", pid)).exists()
}

#[test]
fn a_configured_sidecar_is_started() {
    let mut sidecars = Sidecars::new();
    let specs = vec![sleeper("sleeper", closed_port(1), 301)];

    sidecars.ensure_specs(&specs);
    assert_eq!(sidecars.running(), vec!["sleeper".to_string()]);
}

#[test]
fn a_running_sidecar_is_not_started_twice() {
    let mut sidecars = Sidecars::new();
    let specs = vec![sleeper("sleeper", closed_port(2), 302)];

    sidecars.ensure_specs(&specs);
    // `ensure` is called on every sync tick, so this is the common path, not
    // an edge case: a second call must be a no-op rather than a second ssh.
    for _ in 0..3 {
        sidecars.ensure_specs(&specs);
    }
    assert_eq!(sidecars.running(), vec!["sleeper".to_string()]);
}

#[test]
fn a_sidecar_whose_port_already_answers_is_not_started() {
    // Something else -- a sibling server's tunnel -- already holds the port.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();

    let mut sidecars = Sidecars::new();
    sidecars.ensure_specs(&[sleeper("sleeper", port, 303)]);

    assert!(
        sidecars.running().is_empty(),
        "an answering port means the sidecar is already covered"
    );
    drop(listener);
}

#[test]
fn a_dead_sidecar_is_respawned_on_the_next_tick() {
    let mut sidecars = Sidecars::new();
    let specs = vec![sleeper("sleeper", closed_port(4), 304)];

    sidecars.ensure_specs(&specs);
    let pid = sidecars.pids().first().expect("one sidecar").1;

    // Kill it the way the OS would if a sibling server's job object closed.
    // By PID, not by command line: a command-line match would also reach a
    // sleeper leaked by an earlier `cargo test` run and fail an unrelated test.
    std::process::Command::new("kill")
        .args(["-9", &pid.to_string()])
        .status()
        .expect("kill should run");

    assert!(
        eventually(|| sidecars.running().is_empty()),
        "the killed sidecar should be observed as gone"
    );

    sidecars.ensure_specs(&specs);
    assert_eq!(
        sidecars.running(),
        vec!["sleeper".to_string()],
        "the next tick should bring it back"
    );
}

#[test]
fn shutdown_kills_the_children() {
    let mut sidecars = Sidecars::new();
    let specs = vec![sleeper("sleeper", closed_port(5), 305)];
    sidecars.ensure_specs(&specs);
    assert_eq!(sidecars.running().len(), 1);

    sidecars.shutdown();
    assert!(
        sidecars.running().is_empty(),
        "shutdown should leave no sidecars behind"
    );
}

/// The teardown guarantee the module actually claims: dropping the supervisor
/// -- which is what a normal server exit does -- kills the children rather
/// than orphaning them.
#[test]
fn dropping_the_supervisor_kills_the_children() {
    let port = closed_port(7);
    let pid = {
        let mut sidecars = Sidecars::new();
        sidecars.ensure_specs(&[sleeper("droptest", port, 297)]);
        assert_eq!(sidecars.running(), vec!["droptest".to_string()]);
        // Ask the supervisor which child it started, rather than searching the
        // process table for a matching command line — that search can return a
        // sleeper leaked by an earlier run, and then assert on the wrong pid.
        sidecars.pids().first().expect("one sidecar").1
    };

    assert!(
        eventually(|| !pid_alive(pid)),
        "pid {} should be gone once the supervisor dropped",
        pid
    );
}

#[test]
fn an_unrunnable_command_is_reported_rather_than_panicking() {
    let mut sidecars = Sidecars::new();
    sidecars.ensure_specs(&[SidecarSpec {
        name: "nope".to_string(),
        command: vec!["definitely-not-a-real-binary-xyzzy".to_string()],
        host: "127.0.0.1".to_string(),
        port: closed_port(8),
        env: Vec::new(),
        wait_for_port: false,
    }]);

    // A misconfigured sidecar must not take the server down with it, and must
    // not be recorded as running.
    assert!(sidecars.running().is_empty());
}

#[test]
fn an_empty_command_is_refused_rather_than_spawning_a_shell() {
    let mut sidecars = Sidecars::new();
    sidecars.ensure_specs(&[SidecarSpec {
        name: "empty".to_string(),
        command: Vec::new(),
        host: "127.0.0.1".to_string(),
        port: closed_port(9),
        env: Vec::new(),
        wait_for_port: false,
    }]);
    assert!(sidecars.running().is_empty());
}
