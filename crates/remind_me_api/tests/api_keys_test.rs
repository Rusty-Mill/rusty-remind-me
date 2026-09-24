//! Coverage for named, scope-limited API keys (gap T9, issue #120).
//!
//! The enforcement is the feature, so most of this file is about a `read` key
//! being refused on mutating routes — checked route by route rather than on
//! one representative, because the failure mode is a single handler reachable
//! by a path the gate does not cover, and a spot check would not find it.
//!
//! A read-scoped key that can still write is worse than no feature at all: it
//! gets handed to someone on the understanding that it is safe.

#[path = "../../remind_me_core/src/test_env.rs"]
mod test_env;

mod common;
use common::{authed_server, call, KEY};
use remind_me_core::api_keys::{
    self, ApiKeyError, API_KEYS_FILE_ENV, DEFAULT_KEY_NAME, SCOPE_READ, SCOPE_READ_WRITE,
};
use std::sync::{Mutex, OnceLock};

fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// A scratch key store, isolated per test.
struct Store(std::path::PathBuf);

impl Store {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "rrm_keys_{}_{}_{:?}",
            tag,
            std::process::id(),
            std::thread::current().id()
        ));
        let dir: std::path::PathBuf = dir.to_string_lossy().replace(['(', ')', ' '], "").into();
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("api_keys.json");
        crate::test_env::set_var(API_KEYS_FILE_ENV, &path);
        Self(path)
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        crate::test_env::remove_var(API_KEYS_FILE_ENV);
        if let Some(parent) = self.0.parent() {
            let _ = std::fs::remove_dir_all(parent);
        }
    }
}

fn bearer(key: &str) -> String {
    format!("Bearer {}", key)
}

// ---------------------------------------------------------------------------
// Issuance, listing, revocation
// ---------------------------------------------------------------------------

#[test]
fn a_key_is_readable_exactly_once_and_never_again() {
    let _guard = env_lock().lock().unwrap();
    let store = Store::new("once");

    let plaintext = api_keys::create_key("dashboard", SCOPE_READ).unwrap();

    // Only the hash is persisted, so the plaintext exists in readable form
    // exactly once — in that return value.
    let on_disk = std::fs::read_to_string(&store.0).unwrap();
    assert!(
        !on_disk.contains(&plaintext),
        "the plaintext key was written to disk"
    );
    assert!(on_disk.contains("key_hash"));

    // And listing never reaches for it either.
    let listed = api_keys::list_keys();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].name, "dashboard");
    assert_eq!(listed[0].scope, SCOPE_READ);
    let listed_json = serde_json::to_string(&listed).unwrap();
    assert!(!listed_json.contains(&plaintext));
    assert!(!listed_json.contains("key_hash"), "list leaked the hash");
}

#[test]
fn the_store_file_is_owner_only() {
    let _guard = env_lock().lock().unwrap();
    let store = Store::new("perms");
    api_keys::create_key("dashboard", SCOPE_READ).unwrap();

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&store.0).unwrap().permissions().mode();
        // Group/other readable would hand every hash to any local account.
        assert_eq!(mode & 0o077, 0, "key store is group/other accessible");
    }
    #[cfg(not(unix))]
    let _ = &store;
}

#[test]
fn a_duplicate_name_is_refused_rather_than_shadowing() {
    let _guard = env_lock().lock().unwrap();
    let _store = Store::new("dup");
    api_keys::create_key("dashboard", SCOPE_READ).unwrap();

    // Two keys under one name means revoking by name leaves one alive, and
    // the caller has no way to tell which.
    let second = api_keys::create_key("dashboard", SCOPE_READ_WRITE);
    assert!(matches!(second, Err(ApiKeyError::Invalid(_))));
    assert_eq!(api_keys::list_keys().len(), 1);
}

#[test]
fn the_reserved_default_name_cannot_be_issued_or_revoked() {
    let _guard = env_lock().lock().unwrap();
    let _store = Store::new("reserved");

    // The flat REMIND_ME_API_KEY is config-managed and always read-write.
    // Letting a stored key claim its name would make which one wins depend on
    // lookup order.
    assert!(matches!(
        api_keys::create_key(DEFAULT_KEY_NAME, SCOPE_READ),
        Err(ApiKeyError::Invalid(_))
    ));
    assert!(matches!(
        api_keys::revoke_key(DEFAULT_KEY_NAME),
        Err(ApiKeyError::Invalid(_))
    ));
}

#[test]
fn an_unknown_scope_is_refused() {
    let _guard = env_lock().lock().unwrap();
    let _store = Store::new("scope");

    // A typo'd scope stored as-is would be neither read nor read-write, and
    // whichever way the gate then read it would be a guess.
    assert!(matches!(
        api_keys::create_key("typo", "readwrite"),
        Err(ApiKeyError::Invalid(_))
    ));
    assert!(api_keys::list_keys().is_empty());
}

#[test]
fn revoking_stops_verification_and_reports_an_unknown_name() {
    let _guard = env_lock().lock().unwrap();
    let _store = Store::new("revoke");
    let plaintext = api_keys::create_key("temp", SCOPE_READ_WRITE).unwrap();
    assert!(api_keys::verify(&plaintext).is_some());

    assert!(api_keys::revoke_key("temp").unwrap());
    assert!(api_keys::verify(&plaintext).is_none());

    // Distinct from success: a revoke that did nothing must not report that
    // a key the caller cannot see is gone.
    assert!(!api_keys::revoke_key("temp").unwrap());
}

#[test]
fn a_change_made_by_another_process_is_picked_up_immediately() {
    let _guard = env_lock().lock().unwrap();
    let store = Store::new("concurrent");
    let plaintext = api_keys::create_key("shared", SCOPE_READ_WRITE).unwrap();
    assert!(api_keys::verify(&plaintext).is_some());

    // Simulate another process revoking it: rewrite the file underneath.
    // A cached store would keep honouring a key revoked seconds ago in a
    // different terminal, which is the whole point of re-reading.
    std::fs::write(&store.0, "{\"keys\": []}\n").unwrap();

    assert!(
        api_keys::verify(&plaintext).is_none(),
        "a revoked key was still accepted from a stale cache"
    );
}

#[test]
fn a_corrupt_store_fails_closed() {
    let _guard = env_lock().lock().unwrap();
    let store = Store::new("corrupt");
    let plaintext = api_keys::create_key("real", SCOPE_READ_WRITE).unwrap();

    std::fs::write(&store.0, "{ this is not json").unwrap();

    // Empty rather than "allow everything": an unreadable store authorises
    // nothing, so a corrupt file locks the door rather than opening it.
    assert!(api_keys::verify(&plaintext).is_none());
    assert!(api_keys::list_keys().is_empty());
}

#[test]
fn a_partial_write_cannot_destroy_the_existing_keys() {
    let _guard = env_lock().lock().unwrap();
    let store = Store::new("atomic");
    api_keys::create_key("first", SCOPE_READ).unwrap();
    api_keys::create_key("second", SCOPE_READ_WRITE).unwrap();

    // The store is replaced by rename, so no reader ever observes a
    // half-written file — and no stray temp file is left behind to be picked
    // up as the store later.
    let dir = store.0.parent().unwrap();
    let strays: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().contains("tmp"))
        .collect();
    assert!(strays.is_empty(), "temp file left behind: {strays:?}");
    assert_eq!(api_keys::list_keys().len(), 2);
}

#[test]
fn an_unknown_scope_on_disk_is_treated_as_read_only() {
    let _guard = env_lock().lock().unwrap();
    let store = Store::new("failclosed");
    let plaintext = api_keys::create_key("weird", SCOPE_READ_WRITE).unwrap();

    // Hand-edited, or written by a future version with a scope this build
    // does not know. Failing closed is the only safe reading: the
    // alternative grants write access on a typo.
    let raw = std::fs::read_to_string(&store.0).unwrap();
    std::fs::write(&store.0, raw.replace(SCOPE_READ_WRITE, "superuser")).unwrap();

    let verified = api_keys::verify(&plaintext).expect("still a valid key");
    assert!(verified.may_use("GET"));
    assert!(
        !verified.may_use("POST"),
        "unknown scope granted write access"
    );
}

// ---------------------------------------------------------------------------
// Enforcement, route by route
// ---------------------------------------------------------------------------

/// Every mutating route the API serves, as (method, path, body).
const MUTATING_ROUTES: &[(&str, &str, &str)] = &[
    ("POST", "/api/memories", r#"{"content":"x"}"#),
    ("POST", "/api/memories/bulk/delete", r#"{"ids":["mem_1"]}"#),
    (
        "POST",
        "/api/memories/bulk/tag",
        r#"{"ids":["mem_1"],"tags":["t"]}"#,
    ),
    (
        "POST",
        "/api/memories/bulk/reclassify",
        r#"{"ids":["mem_1"]}"#,
    ),
    ("PUT", "/api/memories/mem_1", r#"{"content":"y"}"#),
    ("PATCH", "/api/memories/mem_1", r#"{"content":"y"}"#),
    ("DELETE", "/api/memories/mem_1", ""),
    ("POST", "/api/import", r#"{"path":"/tmp/nope"}"#),
];

#[test]
fn a_read_scoped_key_is_refused_on_every_mutating_route() {
    let _guard = env_lock().lock().unwrap();
    let _store = Store::new("enforce-write");
    let read_key = api_keys::create_key("readonly", SCOPE_READ).unwrap();
    let (srv, root) = authed_server("keys-enforce");

    // Route by route, not one representative: the failure this test exists
    // to catch is a single handler reachable by a path the gate misses, and
    // a spot check would sail past it.
    for (method, path, body) in MUTATING_ROUTES {
        let response = call(
            &srv,
            method,
            path,
            Some(&bearer(&read_key)),
            Some("application/json"),
            body,
        );
        assert_eq!(
            response.status, 403,
            "{method} {path} accepted a read-scoped key (got {})",
            response.status
        );
        assert!(
            response.json()["error"]
                .as_str()
                .unwrap_or_default()
                .contains("read-only"),
            "{method} {path} gave an unhelpful refusal: {:?}",
            response.body
        );
    }
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn a_read_scoped_key_still_reads() {
    let _guard = env_lock().lock().unwrap();
    let _store = Store::new("enforce-read");
    let read_key = api_keys::create_key("readonly", SCOPE_READ).unwrap();
    let (srv, root) = authed_server("keys-read");

    for path in ["/api/stats", "/api/memories", "/api/vitality"] {
        let response = call(&srv, "GET", path, Some(&bearer(&read_key)), None, "");
        assert_eq!(response.status, 200, "GET {path} refused a read key");
    }
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn a_read_write_scoped_key_may_mutate() {
    let _guard = env_lock().lock().unwrap();
    let _store = Store::new("enforce-rw");
    let rw_key = api_keys::create_key("writer", SCOPE_READ_WRITE).unwrap();
    let (srv, root) = authed_server("keys-rw");

    let response = call(
        &srv,
        "POST",
        "/api/memories",
        Some(&bearer(&rw_key)),
        Some("application/json"),
        r#"{"content":"written by a scoped key"}"#,
    );

    assert_ne!(response.status, 401);
    assert_ne!(response.status, 403);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn the_flat_key_stays_read_write() {
    let _guard = env_lock().lock().unwrap();
    let _store = Store::new("flat");
    api_keys::create_key("readonly", SCOPE_READ).unwrap();
    let (srv, root) = authed_server("keys-flat");

    // Adding scopes must not retroactively restrict a deployment that
    // already relies on REMIND_ME_API_KEY. It was read-write before this
    // feature existed and stays that way.
    let response = call(
        &srv,
        "POST",
        "/api/memories",
        Some(&bearer(KEY)),
        Some("application/json"),
        r#"{"content":"still allowed"}"#,
    );

    assert_ne!(response.status, 401);
    assert_ne!(response.status, 403);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn a_revoked_key_stops_working_on_the_next_request() {
    let _guard = env_lock().lock().unwrap();
    let _store = Store::new("revoked-http");
    let key = api_keys::create_key("temp", SCOPE_READ_WRITE).unwrap();
    let (srv, root) = authed_server("keys-revoked");
    assert_eq!(
        call(&srv, "GET", "/api/stats", Some(&bearer(&key)), None, "").status,
        200
    );

    api_keys::revoke_key("temp").unwrap();

    // No restart, no cache flush. Revocation that only takes effect on the
    // next restart is not revocation.
    assert_eq!(
        call(&srv, "GET", "/api/stats", Some(&bearer(&key)), None, "").status,
        401
    );
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn an_unknown_key_is_401_while_a_scoped_refusal_is_403() {
    let _guard = env_lock().lock().unwrap();
    let _store = Store::new("status-codes");
    let read_key = api_keys::create_key("readonly", SCOPE_READ).unwrap();
    let (srv, root) = authed_server("keys-status");

    // The distinction is the whole diagnostic: 401 means "I do not know this
    // credential", 403 means "I know it and it is not allowed to do that".
    // Collapsing them sends a read-key holder hunting for an auth problem
    // they do not have.
    assert_eq!(
        call(
            &srv,
            "GET",
            "/api/stats",
            Some(&bearer("nonsense")),
            None,
            ""
        )
        .status,
        401
    );
    assert_eq!(
        call(
            &srv,
            "POST",
            "/api/memories",
            Some(&bearer(&read_key)),
            Some("application/json"),
            r#"{"content":"x"}"#,
        )
        .status,
        403
    );
    std::fs::remove_dir_all(&root).unwrap();
}
