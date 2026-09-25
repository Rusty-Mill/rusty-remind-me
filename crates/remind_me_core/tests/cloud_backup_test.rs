//! Cloud backup upload (gap E6, issue #154).
//!
//! The plaintext gate gets nearly all of the attention, and it is tested as a
//! pure function of the environment — no bucket, no network, no optional
//! feature required. That is deliberate: it is the one decision here that must
//! never be wrong, and gating its coverage behind a feature flag would mean
//! the default build never checks the control that protects the default build.

#[path = "../src/test_env.rs"]
mod test_env;

use remind_me_core::cloud_backup::{
    self, object_key, plaintext_gate, upload_backup, UploadOutcome,
};

/// These read process-wide environment variables, so they run one at a time.
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn clear() {
    for var in [
        cloud_backup::BUCKET_ENV,
        cloud_backup::PREFIX_ENV,
        cloud_backup::ENDPOINT_ENV,
        cloud_backup::REGION_ENV,
        cloud_backup::ALLOW_PLAINTEXT_ENV,
        cloud_backup::DB_ENCRYPTION_KEY_ENV,
    ] {
        crate::test_env::remove_var(var);
    }
}

// ---------------------------------------------------------------------------
// The plaintext gate
// ---------------------------------------------------------------------------

#[test]
fn an_unencrypted_backup_is_refused_without_explicit_consent() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear();

    // No encryption key and no opt-in: the backup file is every memory the
    // vault holds, in the clear. "Enable cloud backup" must not silently mean
    // "ship an unencrypted copy of all of it to a third party".
    let refusal = plaintext_gate().unwrap_err();

    // Both remedies named, because a refusal that does not say what to do is
    // just an obstacle.
    assert!(refusal.contains("REMIND_ME_BACKUP_S3_ALLOW_PLAINTEXT_UPLOAD"));
    assert!(refusal.contains("REMIND_ME_DB_ENCRYPTION_KEY"));
    clear();
}

#[test]
fn an_encrypted_backup_needs_no_opt_in() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear();
    crate::test_env::set_var(cloud_backup::DB_ENCRYPTION_KEY_ENV, "a-key");

    // Already ciphertext, so there is nothing to consent to.
    assert!(plaintext_gate().is_ok());
    clear();
}

#[test]
fn the_explicit_opt_in_permits_a_plaintext_upload() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear();
    crate::test_env::set_var(cloud_backup::ALLOW_PLAINTEXT_ENV, "1");

    assert!(plaintext_gate().is_ok());
    clear();
}

#[test]
fn only_a_truthy_opt_in_counts() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear();

    for truthy in ["1", "true", "TRUE", "yes", "on"] {
        crate::test_env::set_var(cloud_backup::ALLOW_PLAINTEXT_ENV, truthy);
        assert!(plaintext_gate().is_ok(), "{truthy} should permit");
    }
    // An unset-looking value must not accidentally grant consent — this is the
    // direction where being wrong ships someone's data.
    for falsy in ["0", "false", "no", "off", "", "  "] {
        crate::test_env::set_var(cloud_backup::ALLOW_PLAINTEXT_ENV, falsy);
        assert!(plaintext_gate().is_err(), "{falsy:?} must not permit");
    }
    clear();
}

#[test]
fn the_gate_is_checked_before_anything_leaves_the_machine() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear();
    crate::test_env::set_var(cloud_backup::BUCKET_ENV, "my-bucket");
    // An endpoint that would fail loudly if it were ever contacted.
    crate::test_env::set_var(cloud_backup::ENDPOINT_ENV, "http://127.0.0.1:1");

    let outcome = upload_backup(std::path::Path::new("/nonexistent/backup.db"));

    // Refused, not Failed: the refusal must come before the client is built
    // and before the file is even read, or the ordering guarantee is a lie.
    match outcome {
        UploadOutcome::Refused { reason } => assert!(reason.contains("refused"), "{reason}"),
        other => panic!("expected a refusal before any I/O, got {other:?}"),
    }
    clear();
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

#[test]
fn no_bucket_configured_is_the_ordinary_case_not_a_problem() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear();

    assert!(!cloud_backup::enabled());
    assert_eq!(
        upload_backup(std::path::Path::new("/tmp/whatever.db")),
        UploadOutcome::NotConfigured
    );
    clear();
}

#[test]
fn prefix_slashes_are_normalised() {
    // `/host/backups/` and `host/backups` are the same place, and a user
    // should not have to know which form this wants.
    assert_eq!(object_key("/host/backups/", "b.db"), "host/backups/b.db");
    assert_eq!(object_key("host/backups", "b.db"), "host/backups/b.db");
    // A blank prefix uploads at the root rather than under a doubled slash.
    assert_eq!(object_key("", "b.db"), "b.db");
    assert_eq!(object_key("   ", "b.db"), "b.db");
    assert_eq!(object_key("/", "b.db"), "b.db");
}

#[test]
fn there_are_no_credential_environment_variables() {
    // The SDK's own credential chain is used deliberately: a parallel
    // secret-storage convention would be one more thing to get right, with
    // none of the existing hardening. This pins that decision — a future
    // `REMIND_ME_BACKUP_S3_SECRET_KEY` would be a regression, not a feature.
    for name in [
        cloud_backup::BUCKET_ENV,
        cloud_backup::PREFIX_ENV,
        cloud_backup::ENDPOINT_ENV,
        cloud_backup::REGION_ENV,
    ] {
        let lowered = name.to_ascii_lowercase();
        assert!(
            !lowered.contains("secret")
                && !lowered.contains("key_id")
                && !lowered.contains("password")
                && !lowered.contains("credential"),
            "{name} looks like a credential variable"
        );
    }
}

#[test]
fn availability_matches_the_compiled_feature() {
    assert_eq!(cloud_backup::available(), cfg!(feature = "cloud-backup"));
}

// ---------------------------------------------------------------------------
// The backup itself always wins
// ---------------------------------------------------------------------------

#[test]
fn a_refused_upload_does_not_stop_the_local_backup() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear();
    crate::test_env::set_var(cloud_backup::BUCKET_ENV, "my-bucket");

    let dir = std::env::temp_dir().join(format!("rrm_cloud_backup_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    // No REMIND_ME_MCP_DIR here. This test used to set it, which read as though
    // it steered the backup location -- it never did. `backup::backup_dir`
    // derives the directory from the *open database's own path*, so passing the
    // path to `Database::open` below is what actually places the backup, and
    // the env var was inert. Now that the variable is genuinely honoured for
    // database resolution (#218), leaving a decorative set_var here would be
    // actively misleading rather than merely unused.
    let db = remind_me_core::Database::open(dir.join("memories.db")).unwrap();
    let outcome = remind_me_core::backup::create_backup(&db.conn(), "test").unwrap();

    // The local copy is the one that has to survive. A refused upload is
    // reported alongside it, never instead of it.
    assert!(std::path::Path::new(&outcome.path).exists());
    assert!(
        matches!(outcome.upload, UploadOutcome::Refused { .. }),
        "got {:?}",
        outcome.upload
    );

    let _ = std::fs::remove_dir_all(&dir);
    clear();
}
