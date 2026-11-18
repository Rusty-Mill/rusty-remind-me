//! The `decisions` dry runs never write to the store (Mill Board #1564, pack
//! D30). They run only in an already-running daemon; with none they fail
//! closed: no daemon is started and the store is not opened in-process,
//! since opening it runs housekeeping that writes.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::SystemTime;

fn scratch(name: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("rrm-decisions-test-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn cli(db: &Path, args: &[&str], daemon: Option<&str>) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_rusty-remind-me"));
    command.args(args).env("REMIND_ME_DB_PATH", db);
    match daemon {
        Some(value) => command.env("REMIND_ME_DAEMON", value),
        None => command.env_remove("REMIND_ME_DAEMON"),
    };
    command.output().expect("the CLI binary should be runnable")
}

/// Every file under `dir`: its bytes and modification time.
fn snapshot(dir: &Path) -> BTreeMap<PathBuf, (Vec<u8>, SystemTime)> {
    let mut files = BTreeMap::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(next) = pending.pop() {
        for entry in std::fs::read_dir(&next).expect("readable dir") {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                pending.push(path);
                continue;
            }
            let bytes = std::fs::read(&path).expect("readable file");
            let modified = path.metadata().and_then(|m| m.modified()).expect("mtime");
            files.insert(path, (bytes, modified));
        }
    }
    files
}

#[test]
fn with_no_daemon_both_dry_runs_fail_closed_and_touch_nothing() {
    let store_dir = scratch("store");
    let db = store_dir.join("remind_me.db");
    let made = cli(&db, &["add", "D1: keep one store"], Some("0"));
    assert!(made.status.success(), "setup add failed: {made:?}");
    let log_dir = scratch("log");
    let log = log_dir.join("DECISIONS.md");
    std::fs::write(
        &log,
        "| # | Decision | Why |\n|---|---|---|\n| D1 | Keep one store | Simple |\n",
    )
    .expect("log written");
    let log_arg = log.to_string_lossy().to_string();
    let before = snapshot(&store_dir);

    let runs: [(&[&str], Option<&str>); 4] = [
        (&["decisions", "normalize"], None),
        (
            &["decisions", "import", &log_arg, "--project", "mill"],
            None,
        ),
        (&["decisions", "normalize"], Some("0")),
        (
            &["decisions", "import", &log_arg, "--project", "mill"],
            Some("0"),
        ),
    ];
    for (args, daemon) in runs {
        let out = cli(&db, args, daemon);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "{args:?} must fail with no daemon");
        assert!(
            stderr.contains("need the remind-me daemon already running")
                && stderr.contains("Nothing was read or written."),
            "{args:?}: {stderr}"
        );
        assert!(out.stdout.is_empty(), "{args:?} printed a plan");
    }

    assert_eq!(
        snapshot(&store_dir),
        before,
        "no file in the store directory was added, changed or touched (no WAL, SHM or daemon.json)"
    );
}
