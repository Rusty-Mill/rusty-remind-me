//! G4, peak memory (spec § Thresholds): resident at most 1.5 GB at any
//! point in workloads 1 to 7. The evidence comes from separate samples,
//! and `verdict` combines them by one explicit rule (`RULE`).
//!
//! A bench run's parent keeps a fill peak (the dataset load, never G4),
//! restarts its peak after the fill and samples it at the end of
//! workloads 1 to 5, before workload 8 (the readers). It samples the
//! readers' peak (not G4), restarts again, and samples the deletes
//! (workload 4). It samples the rest (the G1 checks, the reopen and the
//! backup's side of workload 9) at the end, not G4. Every cold-open child
//! (workload 6) reports its own peak. A crash run's killed children and
//! its reopens (workload 7) report theirs. Each restart first returns the
//! allocator's free heap to the OS (glibc `malloc_trim`), a condition each
//! report names.

use serde_json::{json, Map, Value};

use super::{peak_rss_mb, rss_mb};

/// G4's limit, in MB.
pub const LIMIT_MB: f64 = 1_500.0;

/// How `verdict` combines the evidence.
pub const RULE: &str = "G4 covers workloads 1 to 7. It fails when any \
    scoring peak (a bench report's for D1, D2 or D3, or the crash report's) \
    is above 1500 MB. It passes when the bench reports for D1, D2 and D3 and \
    the crash report all have a scoring peak at or below 1500 MB. Otherwise \
    it is insufficient: a non-scoring sample, which may cover more than \
    workloads 1 to 7 or miss part of them, never decides a pass or a fail. \
    A set whose reports lack or differ in provenance (code, backup, host, \
    cgroup limits) is insufficient whatever its peaks.";

/// The larger of the values present; `None` when none is.
pub fn max_mb(values: impl IntoIterator<Item = Option<f64>>) -> Option<f64> {
    values
        .into_iter()
        .flatten()
        .fold(None, |m: Option<f64>, v| Some(m.map_or(v, |m| m.max(v))))
}

/// Record the parent's peak since its last restart as `segment` in the
/// report's `parent_peaks`.
pub fn note_peak(report: &mut Map<String, Value>, segment: &str) {
    let peaks = report.entry("parent_peaks").or_insert_with(|| json!({}));
    peaks[segment] = json!(peak_rss_mb());
}

/// Return the free heap to the OS, then restart the parent's peak
/// (`VmHWM`) from its resident memory now; appended to the report's
/// `peak_restarts`, saying whether each step happened.
pub fn restart(report: &mut Map<String, Value>) {
    let heap_trimmed = trim_heap();
    // Writing 5 resets VmHWM to the current resident size (Linux only).
    let restarted = std::fs::write("/proc/self/clear_refs", "5").is_ok();
    let entry = json!({ "restarted": restarted, "heap_trimmed": heap_trimmed, "rss_mb": rss_mb() });
    let restarts = report.entry("peak_restarts").or_insert_with(|| json!([]));
    if let Value::Array(list) = restarts {
        list.push(entry);
    }
}

/// The parent process's whole peak, from every sample: the fill, each
/// segment, and whatever came after the last restart. Reported, never G4.
pub fn process_peak(report: &Map<String, Value>) -> Option<f64> {
    let fill = report.get("fill_peak_rss_mb").and_then(Value::as_f64);
    let segments = report
        .get("parent_peaks")
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|p| p.values().map(Value::as_f64));
    max_mb(std::iter::once(fill).chain(segments).chain([peak_rss_mb()]))
}

/// A bench run's G4 evidence: the parent's workloads 1 to 5 and deletes
/// samples, and every cold-open child's peak. It scores only when both
/// restarts happened and every sample is present.
pub fn bench(report: &Map<String, Value>) -> Value {
    let restarts = report
        .get("peak_restarts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let restarted = restarts.len() == 2
        && restarts
            .iter()
            .all(|r| r["restarted"].as_bool() == Some(true));
    let segment = |key: &str| report.get("parent_peaks").and_then(|p| p[key].as_f64());
    let (workloads, deletes) = (segment("workloads_1_to_5"), segment("deletes"));
    // Every cold open's child: D1's, and on D3 the one after the ingest.
    let child = |key: &str| report.get(key).map(|c| c["idle"]["rss_peak_mb"].as_f64());
    let opens: Vec<Option<f64>> = ["cold_open", "cold_open_after_ingest"]
        .into_iter()
        .filter_map(child)
        .collect();
    let cold = if !opens.is_empty() && opens.iter().all(Option::is_some) {
        max_mb(opens)
    } else {
        None
    };
    let mut missing = Vec::new();
    if !restarted {
        missing.push("a restart of the parent's peak");
    }
    if workloads.is_none() {
        missing.push("the parent's peak over workloads 1 to 5");
    }
    if deletes.is_none() {
        missing.push("the parent's peak over the deletes (workload 4)");
    }
    if cold.is_none() {
        missing.push("the cold-open child's peak (workload 6)");
    }
    let scoring = missing.is_empty();
    let trimmed = restarts
        .iter()
        .any(|r| r["heap_trimmed"].as_bool() == Some(true));
    json!({
        "scoring": scoring,
        "peak_mb": if scoring { max_mb([workloads, deletes, cold]) } else { None },
        "parent_workloads_1_to_5_mb": workloads,
        "parent_deletes_mb": deletes,
        "cold_open_child_mb": cold,
        "covers": "workloads 1 to 6; workload 7 is the crash report's",
        "conditions": conditions(trimmed, "the parent's peak restarted after the fill and before the deletes (clear_refs)"),
        "missing": missing,
        "rule": RULE,
    })
}

/// Workload 7's G4 evidence: each killed child's peak up to its kill and
/// each reopen-and-check's (its peak restarted first). It scores only
/// when the run finished all `expected` rounds and every round has both;
/// `missing` says what falls short.
pub fn crash(rounds: &[Value], expected: usize, aborted: bool) -> Value {
    let lacking = |key: &str| rounds.iter().filter(|r| r[key].as_f64().is_none()).count();
    let mut missing = Vec::new();
    if aborted {
        missing.push("the run aborted before its final check".to_string());
    }
    if rounds.len() != expected || expected == 0 {
        missing.push(format!("{} of {expected} rounds", rounds.len()));
    }
    for (key, what) in [
        ("child_peak_mb", "a killed child's peak"),
        ("reopen_peak_mb", "a reopen's restarted peak"),
    ] {
        let n = lacking(key);
        if n > 0 {
            missing.push(format!("{n} rounds without {what}"));
        }
    }
    let max = |key: &str| max_mb(rounds.iter().map(|r| r[key].as_f64()));
    let scoring = missing.is_empty();
    json!({
        "scoring": scoring,
        "peak_mb": if scoring { max_mb([max("child_peak_mb"), max("reopen_peak_mb")]) } else { None },
        "covers": "workload 7: each killed child up to its kill, each reopen and check",
        "conditions": ["each reopen's peak restarted first (clear_refs)"],
        "missing": missing,
        "rule": RULE,
    })
}

/// `store_bench g4 <report.json>...`: one candidate's G4 under `RULE`,
/// from the JSON lines its bench and crash runs printed.
pub fn verdict_from_files(paths: &[String]) -> super::Result<Value> {
    let mut reports = Vec::new();
    for path in paths {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
        let mut report: Value =
            serde_json::from_str(text.trim()).map_err(|e| format!("{path}: {e}"))?;
        derive_crash_g4(&mut report);
        reports.push(report);
    }
    let backends: std::collections::BTreeSet<&str> = reports
        .iter()
        .filter_map(|r| r["backend"].as_str())
        .collect();
    if backends.len() != 1 {
        return Err(format!("the reports must be one candidate's; they name {backends:?}").into());
    }
    let mut v = verdict(&reports);
    v["backend"] = json!(backends.into_iter().next());
    Ok(v)
}

/// A crash report from before `g4` was recorded still has every round's
/// peaks. Its G4 is built from them, the same way `crash` does, but kept
/// as a diagnostic: it never scores, since nothing ties it to the bench
/// reports' code, backup or host.
fn derive_crash_g4(report: &mut Value) {
    if report["mode"] != "crash" || !report["g4"].is_null() {
        return;
    }
    let Some(rounds) = report["rounds"].as_array() else {
        return;
    };
    let expected = report["kills_per_stream"]
        .as_u64()
        .map_or(0, |k| 2 * k as usize);
    let aborted = !report["aborted"].is_null();
    let mut g4 = crash(rounds, expected, aborted);
    let peak = max_mb(
        ["child_peak_mb", "reopen_peak_mb"]
            .iter()
            .flat_map(|key| rounds.iter().map(|r| r[*key].as_f64())),
    );
    g4["scoring"] = json!(false);
    g4["peak_mb"] = Value::Null;
    g4["diagnostic_peak_mb"] = json!(peak);
    g4["derived_from_rounds"] = json!(true);
    if let Value::Array(missing) = &mut g4["missing"] {
        missing.insert(
            0,
            json!("derived from an older report's rounds: a diagnostic, not scoring"),
        );
    }
    report["g4"] = g4;
}

/// What every report in a scored set must share.
pub const PROVENANCE: [&str; 5] = ["git_sha", "backup_sha256", "host", "memory_max", "cpu_max"];

/// Why `reports` are not one run set: a provenance field that a report
/// lacks, or that differs between them. Empty when they agree.
fn provenance_gaps(reports: &[Value]) -> Vec<String> {
    let mut gaps = Vec::new();
    for field in PROVENANCE {
        let values: Vec<&Value> = reports.iter().map(|r| &r["provenance"][field]).collect();
        if values.iter().any(|v| v.is_null()) {
            gaps.push(format!("a report has no {field}"));
        } else if values.windows(2).any(|w| w[0] != w[1]) {
            gaps.push(format!("the reports' {field} differs"));
        }
    }
    gaps
}

/// One candidate's G4 under `RULE`, from its bench and crash reports.
pub fn verdict(reports: &[Value]) -> Value {
    let mut peaks = Vec::new();
    let mut over = Vec::new();
    let mut missing = Vec::new();
    let have = |name: &str| {
        reports.iter().any(|r| match name {
            "crash" => r["mode"] == "crash",
            d => r["mode"] == "bench" && r["dataset"].as_str().is_some_and(|s| s.starts_with(d)),
        })
    };
    for name in ["d1", "d2", "d3", "crash"] {
        if !have(name) {
            missing.push(format!("no {name} report"));
        }
    }
    for r in reports {
        let what = format!(
            "{} {}",
            r["mode"].as_str().unwrap_or("?"),
            r["dataset"].as_str().unwrap_or("?")
        );
        let g4 = &r["g4"];
        match (g4["scoring"].as_bool(), g4["peak_mb"].as_f64()) {
            (Some(true), Some(peak)) => {
                peaks.push(Some(peak));
                if peak > LIMIT_MB {
                    over.push(format!("{what}: {peak} MB"));
                }
            }
            _ => missing.push(format!("{what}: no scoring G4 evidence")),
        }
    }
    let provenance = provenance_gaps(reports);
    let verdict = if !provenance.is_empty() {
        "insufficient"
    } else if !over.is_empty() {
        "fail"
    } else if missing.is_empty() {
        "pass"
    } else {
        "insufficient"
    };
    json!({
        "verdict": verdict,
        "limit_mb": LIMIT_MB,
        "peak_mb": max_mb(peaks),
        "over": over,
        "missing": missing,
        "provenance": provenance,
        "rule": RULE,
    })
}

fn conditions(trimmed: bool, restart: &str) -> Vec<String> {
    let mut list = vec![restart.to_string()];
    if trimmed {
        list.push("free heap returned to the OS before each restart (glibc malloc_trim)".into());
    }
    list
}

/// Return the allocator's free pages to the OS, so memory freed before a
/// restart does not count after it (glibc keeps it otherwise).
#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn trim_heap() -> bool {
    extern "C" {
        fn malloc_trim(pad: usize) -> std::os::raw::c_int;
    }
    // SAFETY: glibc's malloc_trim takes no pointers and only releases
    // pages the allocator already holds free.
    unsafe {
        malloc_trim(0);
    }
    true
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
fn trim_heap() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bench_report(
        restarts: usize,
        w: Option<f64>,
        d: Option<f64>,
        cold: &[Option<f64>],
    ) -> Map<String, Value> {
        let mut r = Map::new();
        let one = json!({ "restarted": true, "heap_trimmed": true, "rss_mb": 100.0 });
        r.insert("peak_restarts".into(), json!(vec![one; restarts]));
        r.insert(
            "parent_peaks".into(),
            json!({ "workloads_1_to_5": w, "concurrency": 9_999.0, "deletes": d }),
        );
        for (key, peak) in ["cold_open", "cold_open_after_ingest"].iter().zip(cold) {
            r.insert((*key).into(), json!({ "idle": { "rss_peak_mb": peak } }));
        }
        r
    }

    #[test]
    fn bench_scores_workloads_deletes_and_every_cold_child_but_not_the_readers() {
        let g4 = bench(&bench_report(
            2,
            Some(1_200.0),
            Some(800.0),
            &[Some(900.0), Some(1_300.0)],
        ));
        assert_eq!(g4["scoring"], true, "{g4}");
        assert_eq!(g4["peak_mb"], 1_300.0, "the readers' 9,999 MB is not G4's");
        assert_eq!(g4["missing"], json!([]));
        assert_eq!(
            g4["conditions"].as_array().unwrap().len(),
            2,
            "trim is named"
        );
    }

    #[test]
    fn bench_does_not_score_without_both_restarts_or_any_sample() {
        for (r, why) in [
            (
                bench_report(1, Some(1.0), Some(1.0), &[Some(1.0)]),
                "a restart of the parent's peak",
            ),
            (
                bench_report(2, None, Some(1.0), &[Some(1.0)]),
                "the parent's peak over workloads 1 to 5",
            ),
            (
                bench_report(2, Some(1.0), None, &[Some(1.0)]),
                "the parent's peak over the deletes (workload 4)",
            ),
            (
                bench_report(2, Some(1.0), Some(1.0), &[Some(1.0), None]),
                "the cold-open child's peak (workload 6)",
            ),
            (
                bench_report(2, Some(1.0), Some(1.0), &[]),
                "the cold-open child's peak (workload 6)",
            ),
        ] {
            let g4 = bench(&r);
            assert_eq!(g4["scoring"], false, "{why}");
            assert_eq!(g4["peak_mb"], Value::Null, "{why}: no peak to misread");
            assert_eq!(g4["missing"], json!([why]));
        }
    }

    #[test]
    fn crash_scores_only_when_every_round_has_both_peaks() {
        let full = [
            json!({ "child_peak_mb": 300.0, "reopen_peak_mb": 700.0 }),
            json!({ "child_peak_mb": 800.0, "reopen_peak_mb": 600.0 }),
        ];
        let g4 = crash(&full, 2, false);
        assert_eq!(g4["scoring"], true, "{g4}");
        assert_eq!(g4["peak_mb"], 800.0);
        let gap = [full[0].clone(), json!({ "child_peak_mb": 800.0 })];
        let g4 = crash(&gap, 2, false);
        assert_eq!(g4["scoring"], false);
        assert_eq!(
            g4["missing"],
            json!(["1 rounds without a reopen's restarted peak"])
        );
        assert_eq!(crash(&[], 0, false)["scoring"], false);
    }

    #[test]
    fn crash_does_not_score_a_short_or_aborted_run() {
        let one = [json!({ "child_peak_mb": 300.0, "reopen_peak_mb": 700.0 })];
        let g4 = crash(&one, 200, false);
        assert_eq!(g4["scoring"], false, "{g4}");
        assert_eq!(g4["peak_mb"], Value::Null);
        assert_eq!(g4["missing"], json!(["1 of 200 rounds"]));
        let g4 = crash(&one, 1, true);
        assert_eq!(g4["scoring"], false, "{g4}");
        assert_eq!(
            g4["missing"],
            json!(["the run aborted before its final check"])
        );
    }

    #[test]
    fn an_older_crash_report_keeps_its_rounds_as_a_diagnostic() {
        let rounds = json!([{ "child_peak_mb": 300.0, "reopen_peak_mb": 700.0 }]);
        let mut old = json!({ "mode": "crash", "kills_per_stream": 1, "rounds": rounds });
        derive_crash_g4(&mut old);
        let g4 = &old["g4"];
        assert_eq!(g4["scoring"], false, "{old}");
        assert_eq!(g4["peak_mb"], Value::Null, "never a peak to score");
        assert_eq!(g4["diagnostic_peak_mb"], 700.0);
        assert_eq!(g4["derived_from_rounds"], true);
        assert_eq!(
            g4["missing"],
            json!([
                "derived from an older report's rounds: a diagnostic, not scoring",
                "1 of 2 rounds"
            ])
        );
        let mut bench = json!({ "mode": "bench" });
        derive_crash_g4(&mut bench);
        assert!(bench["g4"].is_null(), "only crash reports");
    }

    fn provenance() -> Value {
        json!({ "git_sha": "abc", "backup_sha256": "fp", "host": "ai",
                "memory_max": "4294967296", "cpu_max": "200000 100000" })
    }

    fn report(mode: &str, dataset: &str, scoring: bool, peak: f64) -> Value {
        json!({ "mode": mode, "dataset": dataset, "provenance": provenance(),
                "g4": { "scoring": scoring, "peak_mb": peak } })
    }

    #[test]
    fn verdict_follows_the_rule() {
        let all = |d3: Value| {
            vec![
                report("bench", "d1", true, 900.0),
                report("bench", "d2-75000", true, 1_100.0),
                d3,
                report("crash", "d1", true, 700.0),
            ]
        };
        let v = verdict(&all(report("bench", "d3-50000", true, 1_400.0)));
        assert_eq!(v["verdict"], "pass", "{v}");
        assert_eq!(v["peak_mb"], 1_400.0);
        let v = verdict(&all(report("bench", "d3-50000", true, 1_600.0)));
        assert_eq!(v["verdict"], "fail", "{v}");
        // A wide or partial sample decides nothing, above or below the limit.
        let v = verdict(&all(report("bench", "d3-50000", false, 9_000.0)));
        assert_eq!(v["verdict"], "insufficient", "{v}");
        let mut three = all(report("bench", "d3-50000", true, 1_000.0));
        three.pop();
        assert_eq!(verdict(&three)["verdict"], "insufficient");
        // One scoring peak over the limit fails even with others missing.
        three.push(report("crash", "d1", true, 1_700.0));
        three.remove(0);
        assert_eq!(verdict(&three)["verdict"], "fail");
    }

    #[test]
    fn a_mixed_or_unattributed_set_is_insufficient_whatever_its_peaks() {
        let set = || {
            vec![
                report("bench", "d1", true, 900.0),
                report("bench", "d2-75000", true, 1_100.0),
                report("bench", "d3-50000", true, 1_600.0),
                report("crash", "d1", true, 700.0),
            ]
        };
        for (field, value, why) in [
            ("git_sha", json!("def"), "the reports' git_sha differs"),
            (
                "backup_sha256",
                json!("other"),
                "the reports' backup_sha256 differs",
            ),
            ("host", json!("elsewhere"), "the reports' host differs"),
            (
                "memory_max",
                json!("max"),
                "the reports' memory_max differs",
            ),
            ("cpu_max", Value::Null, "a report has no cpu_max"),
        ] {
            let mut reports = set();
            reports[3]["provenance"][field] = value;
            let v = verdict(&reports);
            assert_eq!(v["verdict"], "insufficient", "{field}: {v}");
            assert_eq!(v["provenance"], json!([why]));
            assert_eq!(v["over"], json!(["bench d3-50000: 1600 MB"]), "still shown");
        }
        let mut reports = set();
        reports[0].as_object_mut().unwrap().remove("provenance");
        assert_eq!(verdict(&reports)["verdict"], "insufficient");
        assert_eq!(verdict(&set())["verdict"], "fail", "one run set decides");
    }
}
