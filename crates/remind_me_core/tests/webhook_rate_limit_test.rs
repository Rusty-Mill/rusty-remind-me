//! Rate limiting on the webhook ingest endpoint (gap E2, issue #121).
//!
//! Its own binary because the limiter is a process-wide singleton bucketing by
//! peer address. Sharing a binary with `webhook_test.rs` would couple every
//! ingest-semantics test to how many requests the limiting tests happen to
//! make, which is exactly the coupling that turned up when this change first
//! landed — 29 passing tests went red because the 61st request in the binary
//! started returning 429.
//!
//! The enable flag is a process-wide env var, so the tests take turns on a
//! mutex — one wanting it off while another wants it on is otherwise a race
//! that fails intermittently and looks like a limiter bug. Each test also uses
//! a distinct peer address, so no test depends on a fresh allowance.

#[path = "../src/test_env.rs"]
mod test_env;

use remind_me_core::rate_limit::RATE_LIMIT_ENABLED_ENV;
use remind_me_core::webhook::{self, WebhookConfig, WebhookCounters};
use remind_me_core::Database;
use std::io::{Cursor, Read, Write};
use std::sync::{Mutex, MutexGuard, OnceLock};

/// The enable flag is process-wide; tests take turns setting it.
fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

const SECRET: &str = "webhook-secret";

struct FakeStream {
    input: Cursor<Vec<u8>>,
    output: Vec<u8>,
}

impl FakeStream {
    fn new(raw: Vec<u8>) -> Self {
        Self {
            input: Cursor::new(raw),
            output: Vec::new(),
        }
    }
}

impl Read for FakeStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.input.read(buf)
    }
}

impl Write for FakeStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.output.extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn config() -> WebhookConfig {
    WebhookConfig::new("127.0.0.1", 0, SECRET).expect("a non-empty secret builds a config")
}

/// A request with no valid credential — the shape a stranger sends.
fn unauthenticated_request() -> String {
    let body = "{}";
    format!(
        "POST /ingest HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )
}

/// Serve one request from `peer` and return `(status, headers)`.
fn serve_from(conn: &rusqlite::Connection, peer: &str) -> (u16, String) {
    let counters = WebhookCounters::default();
    let mut stream = FakeStream::new(unauthenticated_request().into_bytes());
    webhook::serve_once_from(&mut stream, &config(), conn, &counters, peer)
        .expect("no I/O failure");
    let text = String::from_utf8_lossy(&stream.output).to_string();
    let status = text
        .lines()
        .next()
        .and_then(|line| line.split(' ').nth(1))
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    (status, text)
}

#[test]
fn an_unauthenticated_flood_is_cut_off_before_it_reaches_auth() {
    let _guard = env_lock();
    crate::test_env::set_var(RATE_LIMIT_ENABLED_ENV, "1");
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let peer = "198.51.100.7";

    // The limiter sits ahead of the credential check on purpose: an
    // unauthenticated flood is the one that matters on a tunnelled endpoint,
    // and a limiter engaging only after a valid token would never see it.
    // Under the limit these are 401s — rejected by auth, having passed the
    // limiter.
    let mut saw_401 = false;
    let mut saw_429 = false;
    let mut retry_after_header = String::new();

    for _ in 0..80 {
        let (status, raw) = serve_from(&conn, peer);
        match status {
            401 => saw_401 = true,
            429 => {
                saw_429 = true;
                if retry_after_header.is_empty() {
                    retry_after_header = raw
                        .lines()
                        .find(|l| l.to_ascii_lowercase().starts_with("retry-after:"))
                        .unwrap_or_default()
                        .to_string();
                }
            }
            other => panic!("unexpected status {other}"),
        }
    }

    assert!(
        saw_401,
        "requests under the limit should reach the auth check"
    );
    assert!(saw_429, "80 requests against a limit of 60 must be cut off");

    // Without this a rejected client retries immediately into the same wall
    // and reads the backoff as broken.
    assert!(
        !retry_after_header.is_empty(),
        "a 429 must say when to come back"
    );
    let seconds: u64 = retry_after_header
        .split(':')
        .nth(1)
        .unwrap_or("0")
        .trim()
        .parse()
        .unwrap_or(0);
    assert!(seconds >= 1, "Retry-After must be a whole positive second");
}

#[test]
fn one_floods_peer_does_not_lock_out_another() {
    let _guard = env_lock();
    crate::test_env::set_var(RATE_LIMIT_ENABLED_ENV, "1");
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();

    for _ in 0..80 {
        serve_from(&conn, "198.51.100.8");
    }
    assert_eq!(
        serve_from(&conn, "198.51.100.8").0,
        429,
        "the flooder is cut off"
    );

    // Per-address buckets. Shared, one abusive caller would be a denial of
    // service against everyone else — worse than having no limiter.
    assert_eq!(
        serve_from(&conn, "198.51.100.9").0,
        401,
        "an unrelated caller was locked out by someone else's flood"
    );
}

#[test]
fn the_limiter_can_be_turned_off() {
    let _guard = env_lock();
    crate::test_env::set_var(RATE_LIMIT_ENABLED_ENV, "");
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();

    // The opt-out has to actually opt out — an operator who disables it and
    // still gets 429s has no way to tell the feature from a bug.
    for _ in 0..80 {
        assert_eq!(serve_from(&conn, "198.51.100.10").0, 401);
    }
    crate::test_env::set_var(RATE_LIMIT_ENABLED_ENV, "1");
}
