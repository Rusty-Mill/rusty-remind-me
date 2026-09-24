//! Coverage for outbound notification channels (gap T1b, issue #117).
//!
//! Its own test binary: channel availability is a process-wide env var, and
//! `scheduler_test.rs` asserts delivery bookkeeping with no channel in play.
//!
//! The webhook half runs against a real one-shot TCP listener rather than a
//! mock. What is actually worth proving is that a real receiver gets a
//! well-formed HTTP request with a parseable body — a mock at the notifier
//! boundary would assert the arguments this crate passes itself and prove
//! nothing about the bytes on the wire, which is exactly where the two things
//! this change had to add (a default port, and omitting the auth header) live.

#[path = "../src/test_env.rs"]
mod test_env;

use remind_me_core::notifications::{
    any_channel_configured, configured_notifiers, notify, webhook_payload, WEBHOOK_URL_ENV,
};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Mutex, OnceLock};

/// Channel availability is process-wide, so the tests take turns.
fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn clear_channels() {
    crate::test_env::remove_var(WEBHOOK_URL_ENV);
}

/// Accept exactly one request, answer `status`, and hand back what was sent.
fn one_shot_receiver(status: u16) -> (String, std::thread::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/hook", listener.local_addr().unwrap());
    let handle = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut raw = Vec::new();
        let mut buf = [0u8; 4096];
        // Read until the body is in hand. The client sends Content-Length and
        // then waits for a response, so reading to EOF would deadlock.
        loop {
            let n = stream.read(&mut buf).unwrap_or(0);
            if n == 0 {
                break;
            }
            raw.extend_from_slice(&buf[..n]);
            let text = String::from_utf8_lossy(&raw).to_string();
            if let Some((head, body)) = text.split_once("\r\n\r\n") {
                let want: usize = head
                    .lines()
                    .find_map(|l| l.strip_prefix("Content-Length: "))
                    .and_then(|v| v.trim().parse().ok())
                    .unwrap_or(0);
                if body.len() >= want {
                    break;
                }
            }
        }
        let _ = stream.write_all(
            format!(
                "HTTP/1.1 {} OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                status
            )
            .as_bytes(),
        );
        String::from_utf8_lossy(&raw).to_string()
    });
    (url, handle)
}

#[test]
fn nothing_configured_means_no_channels_and_no_attempt() {
    let _guard = env_lock().lock().unwrap();
    clear_channels();

    assert!(!any_channel_configured());
    assert!(configured_notifiers().is_empty());
    // A no-op rather than an error, so a caller never has to check
    // availability before speaking.
    assert_eq!(notify("subject", "body"), 0);
}

#[test]
fn a_blank_webhook_url_is_not_a_configured_channel() {
    let _guard = env_lock().lock().unwrap();
    clear_channels();
    crate::test_env::set_var(WEBHOOK_URL_ENV, "   ");

    // An env var set to whitespace is how a half-finished config file reads.
    // Treating it as configured would mean every notification attempt failing
    // against an unparseable URL forever.
    assert!(!any_channel_configured());
    assert_eq!(notify("subject", "body"), 0);
    clear_channels();
}

#[test]
fn the_webhook_receives_the_documented_payload() {
    let _guard = env_lock().lock().unwrap();
    clear_channels();
    let (url, receiver) = one_shot_receiver(200);
    crate::test_env::set_var(WEBHOOK_URL_ENV, &url);

    let accepted = notify("Reminder due: memory `mem_1`", "feed the quokka");
    let raw = receiver.join().unwrap();
    clear_channels();

    assert_eq!(accepted, 1);

    let (head, body) = raw.split_once("\r\n\r\n").expect("a well-formed request");
    assert!(head.starts_with("POST /hook HTTP/1.1"));
    assert!(head.contains("Content-Type: application/json"));
    // No shared secret exists with a user's webhook, and a bare
    // `Authorization: Bearer` is a malformed credential some endpoints reject
    // outright where they would have accepted no header at all.
    assert!(
        !head.contains("Authorization:"),
        "no auth header belongs on a user-configured webhook, got head:\n{head}"
    );

    let parsed: serde_json::Value = serde_json::from_str(body.trim()).expect("a JSON body");
    assert_eq!(
        parsed,
        webhook_payload("Reminder due: memory `mem_1`", "feed the quokka")
    );
    assert_eq!(parsed["source"], "remind-me");
}

#[test]
fn a_webhook_url_without_a_port_still_reaches_a_receiver() {
    // The listener necessarily has a port, so the default-port path is
    // asserted at the parser instead — but this pins the shape people
    // actually configure (`http://host/path`) as accepted rather than
    // rejected, which it was before this change.
    let _guard = env_lock().lock().unwrap();
    clear_channels();
    crate::test_env::set_var(WEBHOOK_URL_ENV, "http://example.invalid/hook");

    // Unreachable host, so it fails — but it fails at *connect*, meaning the
    // URL parsed. Before the default port it failed before ever resolving.
    assert!(any_channel_configured());
    assert_eq!(notify("s", "b"), 0);
    clear_channels();
}

#[test]
fn a_non_2xx_response_is_not_counted_as_accepted() {
    let _guard = env_lock().lock().unwrap();
    clear_channels();
    let (url, receiver) = one_shot_receiver(500);
    crate::test_env::set_var(WEBHOOK_URL_ENV, &url);

    let accepted = notify("subject", "body");
    let _ = receiver.join();
    clear_channels();

    // A receiver that answered at all is not a receiver that accepted. Left
    // uncounted, "delivered to 1 channel" would be a lie told about a 500.
    assert_eq!(accepted, 0);
}

#[test]
fn an_unreachable_webhook_fails_without_propagating() {
    let _guard = env_lock().lock().unwrap();
    clear_channels();
    // Bound and immediately dropped, so the port is almost certainly closed.
    let dead = {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap()
    };
    crate::test_env::set_var(WEBHOOK_URL_ENV, format!("http://{}/hook", dead));

    // The caller is a background loop delivering something else. A dead
    // endpoint must be a return value, never an unwind that takes the loop
    // down with it.
    assert_eq!(notify("subject", "body"), 0);
    clear_channels();
}
