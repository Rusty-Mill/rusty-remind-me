//! R3's G1 for Attest (D45, D46): every port conformance suite
//! remind_me_core exports, run on the Attest candidate. Each test gets an
//! empty Attest log of its own, in a directory this process created
//! exclusively under Cargo's integration-test scratch dir, so no earlier
//! run's log can be reopened.

use remind_me_core::db::engine::EngineTables;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Engine tables over a new, empty Attest log.
///
/// The directory is never deleted, so it outlives the tables that hold
/// its lock; the scratch dir is Cargo's and goes with `cargo clean`.
fn fresh() -> EngineTables {
    let dir = exclusive_dir(&PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("conformance"));
    attest_store::open(&dir).expect("a fresh Attest log opens")
}

/// A directory under `root` that did not exist before this call:
/// `create_dir` fails on a taken name, and the next name is tried.
fn exclusive_dir(root: &Path) -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    create_first_free(root, || {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("a clock after 1970")
            .as_nanos();
        let n = NEXT.fetch_add(1, Ordering::SeqCst);
        format!("{}-{nanos}-{n}", std::process::id())
    })
}

/// Create `root/<name>` for the first name `names` gives that is not
/// taken; the directory made.
fn create_first_free(root: &Path, mut names: impl FnMut() -> String) -> PathBuf {
    std::fs::create_dir_all(root).expect("the scratch root");
    loop {
        let dir = root.join(names());
        match std::fs::create_dir(&dir) {
            Ok(()) => return dir,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => panic!("cannot create {}: {e}", dir.display()),
        }
    }
}

/// Two fixtures never share a directory, and each starts empty.
#[test]
fn fixtures_get_distinct_empty_directories() {
    let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("conformance");
    let first = exclusive_dir(&root);
    let second = exclusive_dir(&root);
    assert_ne!(first, second);
    assert_eq!(std::fs::read_dir(&second).unwrap().count(), 0);
}

/// A forced collision: the first name offered already holds a log from an
/// earlier run, so it is skipped and the fixture gets the next, empty one.
#[test]
fn a_taken_name_holding_a_log_is_skipped() {
    let root = exclusive_dir(&PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("collision"));
    drop(attest_store::open(&root.join("taken")).expect("an earlier run's log"));
    let mut names = ["taken", "free"].into_iter().map(String::from);
    let dir = create_first_free(&root, || names.next().expect("a name"));
    assert_eq!(dir, root.join("free"));
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
}

macro_rules! suites {
    ($($module:ident => $suite:ident),* $(,)?) => {
        $(mod $module {
            use super::fresh;
            remind_me_core::$suite!(fresh());
        })*
    };
}

suites! {
    archives => archive_conformance,
    curation => curation_conformance,
    entities => entity_conformance,
    feedback => feedback_conformance,
    history => history_conformance,
    imports => import_conformance,
    memories => memory_conformance,
    outbox => outbox_conformance,
    promotions => promotion_conformance,
    references => reference_conformance,
    related => related_conformance,
    reminders => reminder_conformance,
    saved_searches => saved_search_conformance,
    sessions => session_conformance,
    stats => stats_conformance,
    snapshots => snapshot_conformance,
    sync_state => sync_state_conformance,
    vectors => vector_conformance,
    wiki => wiki_conformance,
}
