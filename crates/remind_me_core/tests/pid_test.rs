//! Coverage for the dashboard PID-file liveness mechanism (`#90`).

use remind_me_core::pid::{
    dashboard_status, pid_file_path, remove_pid_file, write_pid_file, PidError,
};
use remind_me_core::Database;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread::JoinHandle;

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "rrm_pid_{}_{}_{:?}",
        name,
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Starts a bare TCP listener that answers exactly one request with a
/// `200 OK` `/health` response, and returns its port plus a handle that
/// joins once that one request has been served.
fn fake_dashboard() -> (u16, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut buf = [0u8; 1024];
        let _ = stream.read(&mut buf); // don't care about the request itself
        let body = r#"{"status":"ok"}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        stream
            .write_all(response.as_bytes())
            .expect("write response");
    });
    (port, handle)
}

#[test]
fn pid_file_path_sits_beside_the_database_file() {
    let dir = scratch("beside");
    let db_path = dir.join("memories.db");
    let db = Database::open(&db_path).unwrap();

    let path = pid_file_path(&db.conn()).unwrap();

    assert_eq!(path, dir.join("server.pid"));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn pid_file_path_errors_for_an_in_memory_database() {
    let db = Database::open_in_memory().unwrap();

    let err = pid_file_path(&db.conn()).unwrap_err();

    assert!(matches!(err, PidError::InMemory));
}

#[test]
fn dashboard_status_reports_not_running_without_a_pid_file() {
    let dir = scratch("absent");
    let path = dir.join("server.pid");

    let status = dashboard_status(&path);

    assert!(!status.running);
    assert!(status.url.is_none());
    assert!(status.pid.is_none());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_pid_file_pointing_at_a_healthy_server_reports_running() {
    let dir = scratch("healthy");
    let path = dir.join("server.pid");
    let (port, handle) = fake_dashboard();
    let record = write_pid_file(&path, "127.0.0.1", port).unwrap();

    let status = dashboard_status(&path);

    assert!(
        status.running,
        "a live server answering /health should report running -- this is also what a \
         double-start check would refuse against"
    );
    assert_eq!(status.url.as_deref(), Some(record.url.as_str()));
    assert_eq!(status.pid, Some(record.pid));
    assert!(status.started_at.is_some());
    handle.join().unwrap();
    remove_pid_file(&path);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_pid_file_pointing_at_a_dead_server_is_stale_and_gets_cleaned_up() {
    let dir = scratch("stale");
    let path = dir.join("server.pid");
    // Bind to grab a free port, then drop the listener so nothing answers.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    write_pid_file(&path, "127.0.0.1", port).unwrap();
    assert!(path.exists());

    let status = dashboard_status(&path);

    assert!(
        !status.running,
        "a dead server must not be reported running"
    );
    assert!(
        !path.exists(),
        "a stale pid file should be removed once its health check fails, matching the \
         reference's own cleanup of a pid file whose process is gone"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_malformed_pid_file_is_treated_as_not_running_and_removed() {
    let dir = scratch("malformed");
    let path = dir.join("server.pid");
    std::fs::write(&path, "{ not valid json").unwrap();

    let status = dashboard_status(&path);

    assert!(!status.running);
    assert!(!path.exists());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn remove_pid_file_is_a_no_op_when_nothing_is_there() {
    let dir = scratch("noop");
    let path = dir.join("server.pid");

    remove_pid_file(&path); // must not panic or error

    assert!(!path.exists());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn write_pid_file_records_this_process_and_the_bound_address() {
    let dir = scratch("record");
    let path = dir.join("server.pid");

    let record = write_pid_file(&path, "127.0.0.1", 4321).unwrap();

    assert_eq!(record.pid, std::process::id());
    assert_eq!(record.host, "127.0.0.1");
    assert_eq!(record.port, 4321);
    assert_eq!(record.url, "http://127.0.0.1:4321");
    assert!(!record.started_at.is_empty());
    std::fs::remove_dir_all(&dir).ok();
}
