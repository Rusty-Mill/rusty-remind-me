//! Coverage for `GET /api/reminders/{token}.ics` (gap A2, issue #118).
//!
//! This route is the one place in the crate where a secret rides in the URL
//! and the `Authorization` gate is deliberately skipped, so the tests are
//! mostly about that exemption not being wider than intended: the token has to
//! be *required*, a wrong one must not be distinguishable from a route that
//! does not exist, and no other path may inherit the bypass.

#[path = "../../remind_me_core/src/test_env.rs"]
mod test_env;

mod common;
use common::{get, seeded_server, server};
use remind_me_core::ics::{ICS_TOKEN_ENV, ICS_TOKEN_FILE_ENV};
use std::sync::{Mutex, OnceLock};

const TOKEN: &str = "test-ics-token-value";

/// The token is resolved from process-wide env vars, so tests take turns.
fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn with_token<T>(body: impl FnOnce() -> T) -> T {
    let _guard = env_lock().lock().unwrap();
    // Pinned rather than generated, so a failure is a real one and not a
    // stray token file left by another test.
    crate::test_env::set_var(ICS_TOKEN_ENV, TOKEN);
    crate::test_env::remove_var(ICS_TOKEN_FILE_ENV);
    let out = body();
    crate::test_env::remove_var(ICS_TOKEN_ENV);
    out
}

fn seed_reminder(conn: &rusqlite::Connection, content: &str, remind_at: &str) -> String {
    let memory = remind_me_core::db::queries::add_memory(
        conn,
        remind_me_core::MemoryAddInput {
            content: content.to_string(),
            category: "general".into(),
            tags: vec![],
            source: "manual".into(),
            metadata: serde_json::json!({}),
            subject: None,
            predicate: None,
            object: None,
            entities: vec![],
            sensitive: false,
        },
    )
    .unwrap();
    conn.execute(
        "UPDATE memories SET remind_at = ? WHERE id = ?",
        rusqlite::params![remind_at, &memory.id],
    )
    .unwrap();
    memory.id
}

/// An offset from now, in hours, as an RFC3339 UTC timestamp. Written
/// through `SystemTime` rather than `chrono` because this crate does not
/// depend on `chrono` and adding one for a test helper is not worth it.
fn offset_hours(hours: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let secs = now + hours * 3600;
    // Days since epoch to a civil date, then a fixed-width RFC3339 string.
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (y, m, d) = civil_from_days(days);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}+00:00",
        y,
        m,
        d,
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Howard Hinnant's days-from-civil, inverted. Small enough to inline and
/// exact, which a hand-rolled approximation would not be near a month border.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn future() -> String {
    offset_hours(48)
}

#[test]
fn a_valid_token_serves_the_feed_without_any_auth_header() {
    with_token(|| {
        let (srv, root) = seeded_server("ics-valid", |conn| {
            seed_reminder(conn, "renew the passport", &future());
        });

        // No Authorization header, on purpose. A calendar app polls this from
        // its provider's servers and cannot attach one — that is the entire
        // reason this route authenticates by path.
        let response = get(&srv, &format!("/api/reminders/{}.ics", TOKEN));

        assert_eq!(response.status, 200);
        assert!(
            response.content_type.starts_with("text/calendar"),
            "calendar clients dispatch on this, got {:?}",
            response.content_type
        );
        assert!(response.body.starts_with("BEGIN:VCALENDAR"));
        assert!(response.body.contains("renew the passport"));
        std::fs::remove_dir_all(&root).unwrap();
    });
}

#[test]
fn a_wrong_token_is_indistinguishable_from_a_missing_route() {
    with_token(|| {
        let (srv, root) = server("ics-wrong");

        let wrong = get(&srv, "/api/reminders/not-the-token.ics");
        let absent = get(&srv, "/api/reminders/");

        // A 401 would confirm the route exists and that a token was checked,
        // telling a prober they have the right shape and need only the secret.
        // A bare 404 tells them nothing.
        assert_eq!(wrong.status, 404);
        assert_eq!(absent.status, 404);
        assert_eq!(wrong.status, absent.status);

        // And the response must not echo what was tried, or the rejection
        // itself becomes a log of attempted secrets.
        assert!(!wrong.body.contains("not-the-token"));
        std::fs::remove_dir_all(&root).unwrap();
    });
}

#[test]
fn an_empty_token_does_not_open_the_feed() {
    with_token(|| {
        let (srv, root) = server("ics-empty");

        // `/api/reminders/.ics` strips to an empty token. Compared naively
        // against a non-empty secret this is safe, but it is exactly the
        // shape a prefix-matching bug turns into an open feed.
        assert_eq!(get(&srv, "/api/reminders/.ics").status, 404);
        std::fs::remove_dir_all(&root).unwrap();
    });
}

#[test]
fn the_exemption_does_not_leak_to_neighbouring_paths() {
    with_token(|| {
        let (srv, root) = common::authed_server("ics-neighbours");

        // The secret-path bypass is matched on a prefix and a suffix, so
        // these are the near-misses that would quietly inherit it. Each must
        // still hit the ordinary `Authorization` gate.
        for path in [
            "/api/reminders/token.ics/../memories",
            "/api/memories",
            "/api/stats",
        ] {
            let response = get(&srv, path);
            assert_ne!(
                response.status, 200,
                "{path} answered without auth — the ICS exemption is too wide"
            );
        }
        std::fs::remove_dir_all(&root).unwrap();
    });
}

#[test]
fn an_empty_vault_serves_a_valid_empty_calendar() {
    with_token(|| {
        let (srv, root) = server("ics-none");

        let response = get(&srv, &format!("/api/reminders/{}.ics", TOKEN));

        // Not a 404 and not an empty body: a subscriber with nothing due must
        // get a parseable document, or the calendar reports the subscription
        // as broken.
        assert_eq!(response.status, 200);
        assert!(response.body.starts_with("BEGIN:VCALENDAR"));
        assert!(response.body.trim_end().ends_with("END:VCALENDAR"));
        assert!(!response.body.contains("BEGIN:VEVENT"));
        std::fs::remove_dir_all(&root).unwrap();
    });
}

#[test]
fn the_feed_carries_the_same_window_the_listing_tool_shows() {
    with_token(|| {
        let past = offset_hours(-2);
        let (srv, root) = seeded_server("ics-window", |conn| {
            seed_reminder(conn, "upcoming one", &future());
            seed_reminder(conn, "overdue one", &past);
        });

        let response = get(&srv, &format!("/api/reminders/{}.ics", TOKEN));

        // The `all` window: upcoming plus overdue-and-undelivered. The feed
        // calls the same function the tool does rather than repeating its SQL,
        // so the calendar and `remind_me_list_reminders` cannot disagree.
        assert_eq!(response.body.matches("BEGIN:VEVENT").count(), 2);
        assert!(response.body.contains("upcoming one"));
        assert!(response.body.contains("overdue one"));
        std::fs::remove_dir_all(&root).unwrap();
    });
}

#[test]
fn a_deleted_memorys_reminder_never_reaches_the_feed() {
    with_token(|| {
        let (srv, root) = seeded_server("ics-deleted", |conn| {
            let id = seed_reminder(conn, "deleted but scheduled", &future());
            conn.execute(
                "UPDATE memories SET deleted_at = ? WHERE id = ?",
                rusqlite::params![offset_hours(-1), &id],
            )
            .unwrap();
        });

        let response = get(&srv, &format!("/api/reminders/{}.ics", TOKEN));

        // This URL is the least controlled surface in the product — whoever
        // holds it reads everything on it. Deleted content must not be there.
        assert!(!response.body.contains("deleted but scheduled"));
        assert!(!response.body.contains("BEGIN:VEVENT"));
        std::fs::remove_dir_all(&root).unwrap();
    });
}
