//! `--version` / `-V` print the binary name and crate version and touch no store.

use std::process::Command;

#[test]
fn version_flags_print_name_and_crate_version_without_creating_a_store() {
    let dir = std::env::temp_dir().join(format!("rrm-version-test-{}", std::process::id()));
    let db = dir.join("store").join("remind_me.db");
    let expected = format!("rusty-remind-me {}\n", env!("CARGO_PKG_VERSION"));
    for flag in ["--version", "-V"] {
        let out = Command::new(env!("CARGO_BIN_EXE_rusty-remind-me"))
            .arg(flag)
            .env("REMIND_ME_DB_PATH", &db)
            .env("REMIND_ME_DAEMON", "0")
            .output()
            .expect("the CLI binary should be runnable");
        assert!(out.status.success(), "{flag} should exit 0");
        assert_eq!(String::from_utf8_lossy(&out.stdout), expected, "{flag}");
    }
    assert!(
        !db.parent().is_some_and(|p| p.exists()),
        "--version must not create the store's directory"
    );
}
