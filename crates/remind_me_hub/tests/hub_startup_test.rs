//! The hub binary's store selection, end to end: it serves only from the
//! engine, and refuses to start on a retired store's configuration rather
//! than come up empty in front of data nobody copied.

use std::process::{Command, Output};

/// Run the hub with `env` as its only store settings (plus a secret),
/// expecting it to exit.
fn hub(env: &[(&str, &str)]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_rusty-remind-me-hub"))
        .env_remove("DATABASE_URL")
        .env_remove("REMIND_ME_HUB_DB_PATH")
        .env_remove("REMIND_ME_HUB_DATA_DIR")
        .env("SYNC_SECRET", "test-secret")
        .envs(env.iter().copied())
        .output()
        .expect("run the hub")
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn a_retired_store_variable_refuses_start_and_names_the_copy() {
    let dir = std::env::temp_dir().join(format!("remind_me_hub_startup_{}", std::process::id()));
    let data_dir = dir.to_str().expect("a UTF-8 path");
    for (variable, value, copy_flag) in [
        (
            "DATABASE_URL",
            "postgresql://hub@localhost/hub",
            "--from-postgres",
        ),
        ("REMIND_ME_HUB_DB_PATH", "/data/hub.db", "--from-sqlite"),
    ] {
        // Even beside a data directory: an empty engine in front of data that
        // was never copied would hide it.
        let output = hub(&[(variable, value), ("REMIND_ME_HUB_DATA_DIR", data_dir)]);
        let message = stderr(&output);
        assert!(!output.status.success(), "{variable}: {message}");
        assert!(message.contains(&format!("{variable} is set")), "{message}");
        assert!(
            message.contains(&format!("rusty-remind-me-hub-copy {copy_flag}")),
            "{message}"
        );
        assert!(
            !dir.exists(),
            "{variable}: the refused hub created its data directory"
        );
    }
}

#[test]
fn no_data_directory_refuses_start() {
    let output = hub(&[]);
    let message = stderr(&output);
    assert!(!output.status.success(), "{message}");
    assert!(message.contains("REMIND_ME_HUB_DATA_DIR"), "{message}");
}
