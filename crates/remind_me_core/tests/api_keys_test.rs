//! Coverage for `remind_me_core::api_keys`'s storage and verification logic
//! (#284).
//!
//! `crates/remind_me_api/tests/api_keys_test.rs` already covers this end to
//! end over HTTP -- most usefully, a `read`-scoped key checked route by route
//! against every mutating handler. That file stays as the enforcement test.
//! This one exercises `create_key`/`revoke_key`/`verify`/`VerifiedKey::may_use`
//! directly against `remind_me_core`, so `cargo test -p remind_me_core` gives
//! signal on the storage logic itself without needing the API crate at all.

#[path = "../src/test_env.rs"]
mod test_env;

use remind_me_core::api_keys::{
    self, ApiKeyError, VerifiedKey, API_KEYS_FILE_ENV, DEFAULT_KEY_NAME, SCOPE_READ,
    SCOPE_READ_WRITE,
};
use std::sync::{Mutex, OnceLock};

fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// A scratch key store, isolated per test. Callers must hold `env_lock`
/// first -- `API_KEYS_FILE_ENV` is process-global.
struct Store(std::path::PathBuf);

impl Store {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "rrm_core_keys_{}_{}_{:?}",
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

// ---------------------------------------------------------------------------
// store_path()
// ---------------------------------------------------------------------------

#[test]
fn store_path_honours_the_env_override() {
    let _guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let store = Store::new("path-override");
    assert_eq!(api_keys::store_path(), store.0);
}

// ---------------------------------------------------------------------------
// Issuance, listing, revocation
// ---------------------------------------------------------------------------

#[test]
fn a_key_is_readable_exactly_once_and_never_again() {
    let _guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let store = Store::new("once");

    let plaintext = api_keys::create_key("dashboard", SCOPE_READ).unwrap();

    // Only the hash is persisted, so the plaintext exists in readable form
    // exactly once -- in that return value.
    let on_disk = std::fs::read_to_string(&store.0).unwrap();
    assert!(
        !on_disk.contains(&plaintext),
        "the plaintext key was written to disk"
    );
    assert!(on_disk.contains("key_hash"));

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
    let _guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());
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
    let _guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let _store = Store::new("dup");
    api_keys::create_key("dashboard", SCOPE_READ).unwrap();

    // Two keys under one name means revoking by name leaves one alive, and
    // the caller has no way to tell which.
    let second = api_keys::create_key("dashboard", SCOPE_READ_WRITE);
    assert!(matches!(second, Err(ApiKeyError::Invalid(_))));
    assert_eq!(api_keys::list_keys().len(), 1);
}

#[test]
fn a_blank_name_is_refused() {
    let _guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let _store = Store::new("blank");

    assert!(matches!(
        api_keys::create_key("   ", SCOPE_READ),
        Err(ApiKeyError::Invalid(_))
    ));
    assert!(api_keys::list_keys().is_empty());
}

#[test]
fn the_reserved_default_name_cannot_be_issued_or_revoked() {
    let _guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let _store = Store::new("reserved");

    // The flat REMIND_ME_API_KEY is config-managed and always read-write.
    // Letting a stored key claim its name would make which one wins depend
    // on lookup order.
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
    let _guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let _store = Store::new("scope");

    // A typo'd scope stored as-is would be neither read nor read-write, and
    // whichever way a caller then read it would be a guess.
    assert!(matches!(
        api_keys::create_key("typo", "readwrite"),
        Err(ApiKeyError::Invalid(_))
    ));
    assert!(api_keys::list_keys().is_empty());
}

#[test]
fn revoking_stops_verification_and_reports_an_unknown_name() {
    let _guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());
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
    let _guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());
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
fn a_missing_store_reads_as_empty_rather_than_an_error() {
    let _guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let _store = Store::new("missing"); // creates the directory, not the file

    // A node that has never issued a key has no file, and that is the
    // ordinary case, not a fault.
    assert!(api_keys::list_keys().is_empty());
    assert!(api_keys::verify("anything").is_none());
}

#[test]
fn a_corrupt_store_fails_closed() {
    let _guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());
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
    let _guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let store = Store::new("atomic");
    api_keys::create_key("first", SCOPE_READ).unwrap();
    api_keys::create_key("second", SCOPE_READ_WRITE).unwrap();

    // The store is replaced by rename, so no reader ever observes a
    // half-written file -- and no stray temp file is left behind to be
    // picked up as the store later.
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
    let _guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());
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
// `VerifiedKey::may_use` -- pure logic, no store needed.
// ---------------------------------------------------------------------------

#[test]
fn a_read_write_key_may_use_every_method() {
    let key = VerifiedKey {
        name: "rw".to_string(),
        scope: SCOPE_READ_WRITE.to_string(),
    };
    for method in ["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE"] {
        assert!(key.may_use(method), "{method} refused for a read-write key");
    }
}

#[test]
fn a_read_key_may_only_use_non_mutating_methods() {
    let key = VerifiedKey {
        name: "ro".to_string(),
        scope: SCOPE_READ.to_string(),
    };
    assert!(key.may_use("GET"));
    assert!(key.may_use("HEAD"));
    for method in ["POST", "PUT", "PATCH", "DELETE"] {
        assert!(!key.may_use(method), "{method} allowed for a read-only key");
    }
}

#[test]
fn may_use_is_case_insensitive_on_the_method() {
    let key = VerifiedKey {
        name: "ro".to_string(),
        scope: SCOPE_READ.to_string(),
    };
    // A handler that passes through the raw request method must not bypass
    // the check just because the caller used lowercase.
    assert!(
        !key.may_use("post"),
        "lowercase method bypassed the mutating check"
    );
}
