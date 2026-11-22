//! R3's G1 for Attest (D45, D46): every port conformance suite
//! remind_me_core exports, run on the Attest candidate. Each test gets a
//! fresh Attest log of its own under Cargo's integration-test scratch dir.

use remind_me_core::db::engine::EngineTables;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Engine tables over a new, empty Attest log.
fn fresh() -> EngineTables {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, Ordering::SeqCst);
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("conformance-{}-{n}", std::process::id()));
    attest_store::open(&dir).expect("a fresh Attest log opens")
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
