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
    std::fs::create_dir_all(root).expect("the scratch root");
    loop {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("a clock after 1970")
            .as_nanos();
        let n = NEXT.fetch_add(1, Ordering::SeqCst);
        let dir = root.join(format!("{}-{nanos}-{n}", std::process::id()));
        match std::fs::create_dir(&dir) {
            Ok(()) => return dir,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => panic!("cannot create {}: {e}", dir.display()),
        }
    }
}

/// A fixture is empty even when a directory of the same shape already
/// holds a log: the name taken is refused and the next one is used.
#[test]
fn a_fixture_never_reopens_an_existing_log() {
    let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("conformance");
    let first = exclusive_dir(&root);
    let second = exclusive_dir(&root);
    assert_ne!(first, second);
    assert_eq!(std::fs::read_dir(&second).unwrap().count(), 0);
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
