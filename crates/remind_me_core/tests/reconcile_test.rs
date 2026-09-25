//! Coverage for `remind_me_sync_reconcile` / `_peer` (gap T2b, issue #115).
//!
//! The classifier gets the attention, because it is the whole output: raw
//! deltas are just numbers, and the benign case and the real fault differ only
//! by a sign. `classify` is tested directly rather than through a live remote
//! — the network half is one HTTP GET, and driving a fake server through every
//! verdict would test the harness more than the judgment.

use remind_me_core::sync::classify;
use remind_me_core::{CategoryDrift, ReconcileVerdict};

fn drift(category: &str, local: i64, remote: i64) -> CategoryDrift {
    CategoryDrift {
        category: category.to_string(),
        local,
        remote,
        delta: remote - local,
    }
}

#[test]
fn no_drift_is_in_sync() {
    let (verdict, hints) = classify(&[], Some(10));

    assert_eq!(verdict, ReconcileVerdict::InSync);
    assert!(hints.is_empty(), "nothing to explain when nothing is wrong");
}

#[test]
fn this_node_being_ahead_wins_over_everything_else() {
    // Local 10, remote 7: three records exist only here.
    let (verdict, hints) = classify(&[drift("general", 10, 7)], Some(5));

    // Checked first and unconditionally, because it is the only direction
    // where records sit on one machine with nothing coming to fix it. A recent
    // pull does not make it lag — lag is the remote being ahead.
    assert_eq!(verdict, ReconcileVerdict::NodeAhead);
    assert!(hints[0].contains("pushes are not landing"));
    assert!(hints[0].contains("general"));
}

#[test]
fn node_ahead_is_reported_even_when_the_remote_is_ahead_elsewhere() {
    let (verdict, _) = classify(
        &[drift("general", 10, 7), drift("engineering", 2, 40)],
        Some(5),
    );

    // Mixed drift is the case where reading the numbers by eye goes wrong: the
    // remote being ahead by 38 somewhere is loud, and the 3 records at risk
    // here are quiet. The verdict has to lead with the risk.
    assert_eq!(verdict, ReconcileVerdict::NodeAhead);
}

#[test]
fn the_remote_being_ahead_with_a_recent_pull_is_ordinary_lag() {
    let (verdict, hints) = classify(&[drift("general", 7, 10)], Some(5));

    assert_eq!(verdict, ReconcileVerdict::PullLag);
    assert!(hints[0].contains("ordinary pull lag"));
}

#[test]
fn the_remote_being_ahead_with_a_stale_pull_is_a_fault() {
    let stale = remind_me_core::sync::PULL_LAG_GRACE_SECONDS + 1;

    let (verdict, hints) = classify(&[drift("general", 7, 10)], Some(stale));

    // Same drift as the lag case above. Only the evidence differs — which is
    // exactly why the verdict is judged from the pull age rather than from
    // guessing which categories ought to be static.
    assert_eq!(verdict, ReconcileVerdict::Fault);
    assert!(hints[0].contains("not ordinary lag"));
}

#[test]
fn drift_against_a_never_pulled_remote_is_a_fault_not_lag() {
    let (verdict, hints) = classify(&[drift("general", 7, 10)], None);

    // "Never pulled" is not "pulled a long time ago" — it points at
    // connectivity or credentials rather than at a stalled loop.
    assert_eq!(verdict, ReconcileVerdict::Fault);
    assert!(hints[0].contains("never been pulled"));
}

#[test]
fn the_grace_boundary_is_inclusive_of_lag() {
    let at_grace = remind_me_core::sync::PULL_LAG_GRACE_SECONDS;

    // Pinned so a later `>=` cannot quietly start calling a healthy node
    // faulty one second early.
    assert_eq!(
        classify(&[drift("general", 7, 10)], Some(at_grace)).0,
        ReconcileVerdict::PullLag
    );
    assert_eq!(
        classify(&[drift("general", 7, 10)], Some(at_grace + 1)).0,
        ReconcileVerdict::Fault
    );
}

#[test]
fn a_never_pulled_remote_with_no_drift_is_still_in_sync() {
    // Two fresh nodes that have never spoken but hold identical data. There is
    // nothing wrong, and reporting a fault would train the reader to ignore
    // the verdict.
    assert_eq!(classify(&[], None).0, ReconcileVerdict::InSync);
}
