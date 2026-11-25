//! Workload 6, the cold open (G7), and the idle memory a freshly opened
//! store holds (the G5 baseline), measured on the store exactly as the
//! dataset left it, before any workload has changed it.
//!
//! In a scoring run a child process (this executable, re-run as `cold`)
//! opens the store and answers one hybrid search; the parent times it
//! from spawn to the child's first line, so the time covers process start
//! to first answer and nothing after. The child keeps the database open
//! while it then idles and reads its own resident memory, so the sample
//! is of an open store. The query reaches the child on its stdin, never in
//! a file. Each read of the child is bounded, and a child that overruns
//! or fails is killed and reaped.
//!
//! A warm page cache makes this a warm open: the report says whether the
//! cache was dropped (`R3_DROP_CACHES=1`, root only, refused in CI) and
//! the cgroup limits the run actually had, and `R3_REQUIRE_CGROUP=1`
//! fails a run whose limits are not remindme's (4 GiB, two CPUs).

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use super::{millis, rss_mb, searched, status_mb, Candidate, Isolation, Precomputed, Result};
use remind_me_core::models::MemorySearchInput;

/// How long the child may take to answer its first query.
const FIRST_ANSWER: Duration = Duration::from_secs(300);
/// How long a resident process is left idle before its memory is read.
const IDLE_SETTLE: Duration = Duration::from_secs(2);
/// remindme's limits (D38): `MemoryMax=4G`, `CPUQuota=200%`.
const MEMORY_MAX: &str = "4294967296";
const CPU_MAX: &str = "200000 100000";

/// The cold open of the store in `dir` with `query`; the report.
pub fn cold_open(
    candidate: Candidate,
    dir: &Path,
    query: &(String, Vec<f32>),
    isolation: Isolation,
) -> Result<Value> {
    let limits = cgroup_limits();
    if std::env::var("R3_REQUIRE_CGROUP").is_ok_and(|v| v == "1") && !limits_match(&limits) {
        return Err(
            format!("R3_REQUIRE_CGROUP=1 but this run's cgroup limits are {limits}").into(),
        );
    }
    if isolation == Isolation::InProcess {
        let t = Instant::now();
        let held = Held::open(candidate, dir, query)?;
        let ms = millis(t.elapsed());
        return Ok(json!({
            "ms": ms,
            "isolation": "InProcess",
            "page_cache_dropped": false,
            "cache": cache_label(false),
            "cgroup": limits,
            "child": held.timing,
        }));
    }
    let dropped = std::env::var("R3_DROP_CACHES").is_ok_and(|v| v == "1");
    if dropped {
        drop_page_cache()?;
    }
    let t = Instant::now();
    let mut child = Command::new(std::env::current_exe()?)
        .args(["cold", candidate.name])
        .arg(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;
    let outcome = drive(&mut child, query, t);
    if outcome.is_err() {
        let _ = child.kill();
    }
    let status = child.wait()?;
    let (ms, first, idle) = outcome?;
    if !status.success() {
        return Err(format!("cold child failed: {status}").into());
    }
    Ok(json!({
        "ms": ms,
        "isolation": "Child",
        "page_cache_dropped": dropped,
        "cache": cache_label(dropped),
        "cgroup": limits,
        "child": first,
        "idle": idle,
    }))
}

/// Send the child its query, then read its two lines, each within its
/// bound: the time to the first, and both lines.
fn drive(child: &mut Child, query: &(String, Vec<f32>), t: Instant) -> Result<(f64, Value, Value)> {
    let mut stdin = child.stdin.take().ok_or("cold child has no stdin")?;
    stdin.write_all(&serde_json::to_vec(query)?)?;
    drop(stdin);
    let stdout = child.stdout.take().ok_or("cold child has no stdout")?;
    let (sent, lines) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            if sent.send(line).is_err() {
                return;
            }
        }
    });
    let line = |bound: Duration, what: &str| -> Result<Value> {
        let line = lines
            .recv_timeout(bound)
            .map_err(|_| format!("cold child gave no {what} within {bound:?}"))??;
        Ok(serde_json::from_str(&line)?)
    };
    let first = line(FIRST_ANSWER, "first answer")?;
    let ms = millis(t.elapsed());
    let idle = line(IDLE_SETTLE + FIRST_ANSWER, "idle sample")?;
    Ok((ms, first, idle))
}

/// The `cold` child: read the query from stdin, open the store, answer
/// it and print the times; then, the store still open, idle and print
/// the resident memory.
pub fn cold_child(candidate: Candidate, dir: &Path) -> Result<()> {
    let mut input = Vec::new();
    std::io::stdin().read_to_end(&mut input)?;
    let query: (String, Vec<f32>) = serde_json::from_slice(&input)?;
    let held = Held::open(candidate, dir, &query)?;
    println!("{}", held.timing);
    std::io::stdout().flush()?;
    std::thread::sleep(IDLE_SETTLE);
    let idle = json!({ "rss_idle_mb": rss_mb(), "rss_peak_mb": status_mb("VmHWM:") });
    println!("{idle}");
    drop(held);
    Ok(())
}

/// An open store that has answered its first query, and how long each
/// took. The database stays open as long as this does.
struct Held {
    _db: remind_me_core::Database,
    timing: Value,
}

impl Held {
    fn open(candidate: Candidate, dir: &Path, query: &(String, Vec<f32>)) -> Result<Self> {
        let t = Instant::now();
        let db = candidate.open(dir)?;
        let open_ms = millis(t.elapsed());
        let (text, vector) = query.clone();
        let input: MemorySearchInput = serde_json::from_value(json!({ "query": text }))?;
        let vectors = Precomputed([(text, vector)].into_iter().collect());
        let t = Instant::now();
        searched(&db.store(), &input, Some(&vectors))?;
        let timing = json!({ "open_ms": open_ms, "first_query_ms": millis(t.elapsed()) });
        Ok(Self { _db: db, timing })
    }
}

/// This process's cgroup (v2) and its `memory.max` and `cpu.max`, or
/// nulls where they cannot be read.
pub(super) fn cgroup_limits() -> Value {
    let path = std::fs::read_to_string("/proc/self/cgroup")
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("0::").map(str::to_string))
        });
    let read = |file: &str| {
        let path = path.as_ref()?;
        let full = Path::new("/sys/fs/cgroup")
            .join(path.trim_start_matches('/'))
            .join(file);
        std::fs::read_to_string(full)
            .ok()
            .map(|s| s.trim().to_string())
    };
    json!({ "path": path, "memory_max": read("memory.max"), "cpu_max": read("cpu.max") })
}

/// What the open measured: a cold cache only when it was dropped first.
fn cache_label(dropped: bool) -> &'static str {
    if dropped {
        "cold: page cache dropped before the open"
    } else {
        "warm: page cache not dropped, not G7 cold-cache evidence"
    }
}

/// Whether `limits` are remindme's.
fn limits_match(limits: &Value) -> bool {
    limits["memory_max"] == MEMORY_MAX && limits["cpu_max"] == CPU_MAX
}

/// Drop the page cache (Linux, root), so the next open reads from disk.
/// Refused under CI: it is host-wide, for a dedicated benchmark host only.
fn drop_page_cache() -> Result<()> {
    if std::env::var("CI").is_ok_and(|v| !v.is_empty()) {
        return Err("R3_DROP_CACHES=1 is refused under CI: dropping caches is host-wide".into());
    }
    let synced = Command::new("sync").status()?;
    if !synced.success() {
        return Err("sync failed before dropping the page cache".into());
    }
    std::fs::write("/proc/sys/vm/drop_caches", "3")
        .map_err(|e| format!("R3_DROP_CACHES=1 but the page cache cannot be dropped: {e}").into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use remind_me_core::db::backend::Backend;
    use remind_me_core::db::engine::EngineTables;

    fn engine(dir: &Path) -> Result<Box<dyn Backend>> {
        Ok(Box::new(EngineTables::open(dir)?))
    }

    const ENGINE: Candidate = Candidate {
        name: "engine",
        tables: engine,
        restore: engine,
        package: super::super::backup_package,
        verify: super::super::verify_engine,
    };

    /// The child's store stays open (its lock held) from the first answer
    /// until the held value is dropped, after the idle sample.
    #[test]
    fn the_store_stays_open_until_the_held_query_is_dropped() {
        let _serial = super::super::serial();
        let dir = std::env::temp_dir().join(format!("store_bench_held_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let query = ("rust storage".to_string(), vec![0.5; super::super::DIM]);
        let held = Held::open(ENGINE, &dir, &query).unwrap();
        assert!(held.timing["first_query_ms"].is_number());
        assert!(ENGINE.open(&dir).is_err(), "a second open while held");
        drop(held);
        drop(ENGINE.open(&dir).unwrap());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn only_a_dropped_cache_is_labelled_cold() {
        assert!(cache_label(true).starts_with("cold"));
        assert!(cache_label(false).contains("not G7 cold-cache evidence"));
    }

    #[test]
    fn only_remindmes_limits_match() {
        let ok = json!({ "memory_max": MEMORY_MAX, "cpu_max": CPU_MAX });
        assert!(limits_match(&ok));
        let unlimited = json!({ "memory_max": "max", "cpu_max": "max 100000" });
        assert!(!limits_match(&unlimited));
        assert!(!limits_match(
            &json!({ "memory_max": null, "cpu_max": null })
        ));
    }
}
