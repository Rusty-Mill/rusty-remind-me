//! Version checking and self-update: `remind_me_check_update` and
//! `remind_me_self_update`.
//!
//! See `docs/adr/0003-self-update-strategy.md` for the decision this module
//! implements — the short version: `remind_me_check_update` ports the
//! reference's `git fetch` + commit comparison directly, and
//! `remind_me_self_update` means "`git pull --ff-only`, then `cargo build
//! --release --workspace`, then tell the operator to restart" rather than
//! attempting to fetch or swap a prebuilt binary.
//!
//! Since the move into the `Rusty-Mill/rusty_mill` monorepo (under
//! `crates/apps/rusty_remind_me`), the repository root and this product's
//! directory are no longer the same place. The updater therefore recognises
//! both layouts ([`product_dir`]), scopes "commits behind" to commits that
//! touch this product rather than any monorepo commit, and rebuilds only this
//! product's two shipped binaries ([`BUILD_PACKAGES`]) instead of
//! `--workspace`, which in the monorepo would compile ~240 unrelated crates.

use serde::{Deserialize, Serialize};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Set to `false`/`0`/`no`/`off` to skip the background update check this
/// crate runs once at startup. The manual `remind_me_check_update` and
/// `remind_me_self_update` tools are unaffected either way.
pub const AUTO_UPDATE_CHECK_ENV: &str = "REMIND_ME_AUTO_UPDATE_CHECK";
/// Optional trust pin for `remind_me_self_update`: if set, an `origin`
/// remote that doesn't match exactly refuses the update rather than pulling
/// from an unexpected remote.
pub const UPDATE_EXPECTED_ORIGIN_ENV: &str = "REMIND_ME_UPDATE_EXPECTED_ORIGIN";

/// This crate's own version, matching the `serverInfo.version` already
/// reported by the MCP `initialize` handshake.
pub const INSTALLED_VERSION: &str = env!("CARGO_PKG_VERSION");

const GIT_TIMEOUT: Duration = Duration::from_secs(60);
/// Considerably longer than a git operation — a release-mode workspace
/// rebuild is real compilation, not a network round-trip.
const BUILD_TIMEOUT: Duration = Duration::from_secs(600);

/// Result of a version/update check against the remote repository.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UpdateStatus {
    pub installed_version: String,
    pub local_commit: String,
    pub remote_commit: String,
    pub update_available: bool,
    pub commits_behind: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub commit_messages: Vec<String>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub repo_path: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub origin_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Result of a `remind_me_self_update` run.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UpdateResult {
    pub success: bool,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub previous_commit: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub new_commit: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub origin_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Always `true` on success — even the reference's own `pip install -e
    /// .` requires a restart, since a running process keeps executing the
    /// code it already loaded regardless of what changed on disk.
    pub restart_required: bool,
    /// `true` when a build failure after a successful pull was
    /// automatically rolled back (`git reset --hard` to the pre-pull
    /// commit), restoring a consistent state.
    pub rolled_back: bool,
}

// ---------------------------------------------------------------------------
// Repository discovery
// ---------------------------------------------------------------------------

/// Where this product lives relative to the repository root in the
/// `Rusty-Mill/rusty_mill` monorepo.
pub const MONOREPO_PRODUCT_DIR: &str = "crates/apps/rusty_remind_me";

/// The packages `remind_me_self_update` rebuilds: the two binaries a release
/// ships (`rusty-remind-me` and `rusty-remind-me-hub`). Named explicitly
/// rather than `--workspace` because in the monorepo the workspace is every
/// Rusty Mill crate, not just this product.
pub const BUILD_PACKAGES: [&str; 2] = ["rusty-remind-me", "remind_me_hub"];

/// This product's directory relative to `repo`: [`MONOREPO_PRODUCT_DIR`] in
/// the monorepo, `"."` in a standalone `baileyrd/rusty_remind_me` clone (the
/// layout before the move, still recognised so an old clone reports
/// accurately instead of failing to find itself). `None` when `repo`'s root
/// `Cargo.toml` names neither.
///
/// Checks for this product's own distinctive member path rather than
/// parsing TOML with a new dependency just for this one check. The monorepo
/// path is tested first because it also contains the standalone one.
pub fn product_dir(repo: &Path) -> Option<&'static str> {
    let manifest = std::fs::read_to_string(repo.join("Cargo.toml")).ok()?;
    if manifest.contains(&format!("{MONOREPO_PRODUCT_DIR}/crates/remind_me_core")) {
        return Some(MONOREPO_PRODUCT_DIR);
    }
    manifest.contains("crates/remind_me_core").then_some(".")
}

/// Whether `candidate` is a repository root holding this product, not some
/// unrelated repository the upward walk happened to pass through (e.g. a
/// nested vendor checkout).
fn looks_like_this_workspace(candidate: &Path) -> bool {
    product_dir(candidate).is_some()
}

/// Walk upward from `start` looking for this workspace's own `.git`
/// directory.
fn find_repo_root_from(start: &Path) -> Option<PathBuf> {
    let mut dir = start;
    loop {
        if dir.join(".git").exists() && looks_like_this_workspace(dir) {
            return Some(dir.to_path_buf());
        }
        dir = dir.parent()?;
    }
}

/// Locate this workspace's git repository root from the current working
/// directory.
///
/// Unlike the reference (which walks up from its own installed package
/// file, always inside the repo because of `pip install -e .`), a compiled
/// binary has no fixed relationship between its executable path and a
/// source tree — `cargo install` copies it entirely outside any checkout.
/// Self-update is only coherent when invoked from inside the repo it is
/// meant to rebuild, so this walks up from the process's current
/// directory instead — see `docs/adr/0003-self-update-strategy.md`.
pub fn find_repo_root() -> Option<PathBuf> {
    std::env::current_dir()
        .ok()
        .and_then(|cwd| find_repo_root_from(&cwd))
}

/// The `origin` remote's URL, or `""` if it can't be read. Purely local —
/// `git config --get` reads `.git/config` directly, no network. Best-effort:
/// an unreadable/missing origin must never break a status check.
fn get_origin_url(repo: &Path) -> String {
    run_git(repo, &["config", "--get", "remote.origin.url"]).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Subprocess helpers
// ---------------------------------------------------------------------------

/// Runs `cmd` to completion, killing it if it outruns `timeout`.
///
/// stdout/stderr are drained on dedicated threads while polling for exit —
/// `cargo build`'s own output is easily large enough to fill a pipe buffer,
/// and reading it only after the process exits (or only inside the poll
/// loop) risks the classic deadlock where the child blocks writing to a
/// full pipe nobody is draining.
fn run_with_timeout(mut cmd: Command, timeout: Duration) -> Result<String, String> {
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to spawn: {e}"))?;

    let mut stdout_pipe = child.stdout.take().expect("stdout was piped");
    let mut stderr_pipe = child.stderr.take().expect("stderr was piped");
    let stdout_thread = std::thread::spawn(move || {
        let mut buf = String::new();
        let _ = stdout_pipe.read_to_string(&mut buf);
        buf
    });
    let stderr_thread = std::thread::spawn(move || {
        let mut buf = String::new();
        let _ = stderr_pipe.read_to_string(&mut buf);
        buf
    });

    let start = Instant::now();
    let outcome = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status.success()),
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    break Err("timed out".to_string());
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => break Err(format!("failed to wait: {e}")),
        }
    };

    let stdout = stdout_thread.join().unwrap_or_default();
    let stderr = stderr_thread.join().unwrap_or_default();

    match outcome {
        Ok(true) => Ok(stdout.trim().to_string()),
        Ok(false) => Err(if stderr.trim().is_empty() {
            stdout.trim().to_string()
        } else {
            stderr.trim().to_string()
        }),
        Err(reason) => Err(if stderr.trim().is_empty() {
            reason
        } else {
            format!("{reason}: {}", stderr.trim())
        }),
    }
}

fn run_git(repo: &Path, args: &[&str]) -> Result<String, String> {
    let mut cmd = Command::new("git");
    cmd.args(args).current_dir(repo);
    run_with_timeout(cmd, GIT_TIMEOUT)
}

fn run_cargo_build(repo: &Path) -> Result<(), String> {
    let mut cmd = Command::new("cargo");
    cmd.args(["build", "--release"]).current_dir(repo);
    for package in BUILD_PACKAGES {
        cmd.args(["-p", package]);
    }
    run_with_timeout(cmd, BUILD_TIMEOUT).map(|_| ())
}

/// Best-effort `git reset --hard` back to `previous_commit`. Only ever
/// called after a successful `git pull` followed by a failed build, to
/// restore a consistent state. Returns `false` (never panics) if the reset
/// itself fails — the caller surfaces that so the operator knows manual
/// recovery is needed.
fn rollback(repo: &Path, previous_commit: &str) -> bool {
    if previous_commit.is_empty() || previous_commit == "unknown" {
        return false;
    }
    run_git(repo, &["reset", "--hard", previous_commit]).is_ok()
}

fn short(commit: &str) -> String {
    commit.chars().take(12).collect()
}

// ---------------------------------------------------------------------------
// Core logic
// ---------------------------------------------------------------------------

/// Check whether `repo` is behind `origin/main`.
fn check_for_update_at(repo: &Path) -> UpdateStatus {
    let repo_path = repo.display().to_string();
    let origin_url = get_origin_url(repo);

    if let Err(e) = run_git(repo, &["fetch", "origin", "--quiet"]) {
        return UpdateStatus {
            installed_version: INSTALLED_VERSION.to_string(),
            repo_path,
            origin_url,
            error: Some(format!("Failed to fetch from origin: {e}")),
            ..Default::default()
        };
    }

    let local_commit = match run_git(repo, &["rev-parse", "HEAD"]) {
        Ok(c) => c,
        Err(e) => {
            return UpdateStatus {
                installed_version: INSTALLED_VERSION.to_string(),
                repo_path,
                origin_url,
                error: Some(format!("Failed to read commit info: {e}")),
                ..Default::default()
            }
        }
    };
    let remote_commit = match run_git(repo, &["rev-parse", "origin/main"]) {
        Ok(c) => c,
        Err(e) => {
            return UpdateStatus {
                installed_version: INSTALLED_VERSION.to_string(),
                repo_path,
                origin_url,
                error: Some(format!("Failed to read commit info: {e}")),
                ..Default::default()
            }
        }
    };

    if local_commit == remote_commit {
        return UpdateStatus {
            installed_version: INSTALLED_VERSION.to_string(),
            local_commit: short(&local_commit),
            remote_commit: short(&remote_commit),
            update_available: false,
            commits_behind: 0,
            repo_path,
            origin_url,
            error: None,
            commit_messages: Vec::new(),
        };
    }

    // Only commits touching this product count: in the monorepo, `main`
    // moves for every Rusty Mill crate, and reporting each of those as an
    // update to this one would make the notice permanently on and useless.
    // (A root `Cargo.lock` bump alone is deliberately not counted either --
    // it is picked up by the next update that does touch this product.)
    let scope = product_dir(repo).unwrap_or(".");
    let commits_behind = run_git(
        repo,
        &["rev-list", "--count", "HEAD..origin/main", "--", scope],
    )
    .ok()
    .and_then(|s| s.parse().ok())
    .unwrap_or(0);
    let commit_messages = run_git(
        repo,
        &[
            "log",
            "--oneline",
            "HEAD..origin/main",
            "--max-count=10",
            "--",
            scope,
        ],
    )
    .map(|s| s.lines().map(String::from).collect())
    .unwrap_or_default();

    UpdateStatus {
        installed_version: INSTALLED_VERSION.to_string(),
        local_commit: short(&local_commit),
        remote_commit: short(&remote_commit),
        update_available: commits_behind > 0,
        commits_behind,
        commit_messages,
        repo_path,
        origin_url,
        error: None,
    }
}

/// Check whether the local clone is behind `origin/main`. Read-only: this
/// never modifies any file. `git fetch` is the only network call.
pub fn check_for_update() -> UpdateStatus {
    match find_repo_root() {
        Some(repo) => check_for_update_at(&repo),
        None => UpdateStatus {
            installed_version: INSTALLED_VERSION.to_string(),
            error: Some(
                "Not running from inside this project's git repository — \
                 self-update requires being invoked from the repo (or a \
                 subdirectory of it), not wherever the built binary lives."
                    .to_string(),
            ),
            ..Default::default()
        },
    }
}

/// Pull the latest changes into `repo` and rebuild the workspace.
fn perform_update_at(repo: &Path, force: bool) -> UpdateResult {
    let origin_url = get_origin_url(repo);

    if let Ok(expected) = std::env::var(UPDATE_EXPECTED_ORIGIN_ENV) {
        if !expected.is_empty() && origin_url != expected {
            return UpdateResult {
                success: false,
                origin_url: origin_url.clone(),
                error: Some(format!(
                    "Refusing to update: origin is {origin_url:?}, expected {expected:?} \
                     ({UPDATE_EXPECTED_ORIGIN_ENV}). If this remote change is intentional, \
                     update the env var to match."
                )),
                ..Default::default()
            };
        }
    }

    let previous_commit =
        run_git(repo, &["rev-parse", "--short", "HEAD"]).unwrap_or_else(|_| "unknown".to_string());

    if !force {
        match run_git(repo, &["status", "--porcelain"]) {
            Ok(status) if !status.is_empty() => {
                return UpdateResult {
                    success: false,
                    previous_commit,
                    origin_url,
                    error: Some(
                        "Working tree has uncommitted changes. Commit or stash them first, \
                         or use force=true to override."
                            .to_string(),
                    ),
                    ..Default::default()
                };
            }
            Err(e) => {
                return UpdateResult {
                    success: false,
                    previous_commit,
                    origin_url,
                    error: Some(format!("Failed to check working tree status: {e}")),
                    ..Default::default()
                }
            }
            _ => {}
        }
    }

    // `force` bypasses only the dirty-tree guard above -- never this one.
    // A diverged local history must still refuse rather than merge or
    // rebase it away (docs/adr/0003).
    if let Err(e) = run_git(repo, &["pull", "--ff-only", "origin", "main"]) {
        return UpdateResult {
            success: false,
            previous_commit,
            origin_url,
            error: Some(format!("git pull failed: {e}")),
            ..Default::default()
        };
    }

    if let Err(e) = run_cargo_build(repo) {
        let rolled_back = rollback(repo, &previous_commit);
        let error = if rolled_back {
            format!(
                "cargo build failed: {e} Rolled the source tree back to {previous_commit} -- \
                 nothing changed overall."
            )
        } else {
            format!(
                "cargo build failed: {e} Automatic rollback ALSO failed -- the source tree is \
                 now ahead of the last built binary (still {previous_commit}'s). Run \
                 'git reset --hard {previous_commit}' manually, then rebuild."
            )
        };
        return UpdateResult {
            success: false,
            previous_commit,
            origin_url,
            rolled_back,
            error: Some(error),
            ..Default::default()
        };
    }

    let new_commit =
        run_git(repo, &["rev-parse", "--short", "HEAD"]).unwrap_or_else(|_| "unknown".to_string());

    UpdateResult {
        success: true,
        previous_commit,
        new_commit,
        origin_url,
        restart_required: true,
        rolled_back: false,
        error: None,
    }
}

/// Pull the latest changes from `origin/main` and rebuild the workspace.
///
/// Refuses a dirty working tree unless `force` is set — `force` bypasses
/// only that guard, never the fast-forward-only pull, so a diverged local
/// history is still refused either way. On success, `restart_required` is
/// always `true`: a running process keeps executing the code it already
/// loaded, exactly like the reference's own `pip install -e .` step.
pub fn perform_update(force: bool) -> UpdateResult {
    match find_repo_root() {
        Some(repo) => perform_update_at(&repo, force),
        None => UpdateResult {
            success: false,
            error: Some(
                "Not running from inside this project's git repository — \
                 self-update requires being invoked from the repo (or a \
                 subdirectory of it), not wherever the built binary lives."
                    .to_string(),
            ),
            ..Default::default()
        },
    }
}

// ---------------------------------------------------------------------------
// Background startup check and one-shot notification state
// ---------------------------------------------------------------------------

static UPDATE_NOTICE: OnceLock<Mutex<Option<String>>> = OnceLock::new();

fn notice_cell() -> &'static Mutex<Option<String>> {
    UPDATE_NOTICE.get_or_init(|| Mutex::new(None))
}

fn format_notice(status: &UpdateStatus) -> String {
    let mut parts = vec![
        format!(
            "**Update available** for rusty_remind_me ({} commit{} behind)",
            status.commits_behind,
            if status.commits_behind != 1 { "s" } else { "" }
        ),
        format!(
            "Installed: `{}` (commit `{}`)",
            status.installed_version, status.local_commit
        ),
        format!("Latest: commit `{}`", status.remote_commit),
    ];
    if !status.commit_messages.is_empty() {
        parts.push("\nRecent changes:".to_string());
        for msg in status.commit_messages.iter().take(5) {
            parts.push(format!("- `{msg}`"));
        }
    }
    parts.push(
        "\nRun `remind_me_self_update` to update, or `remind_me_check_update` for details."
            .to_string(),
    );
    parts.join("\n")
}

/// Sets the pending notice from an already-computed status — split out from
/// [`background_check`] so the notice-setting behavior is testable without
/// spawning a thread or touching git.
fn background_check_with(status: &UpdateStatus) {
    if status.update_available {
        let mut guard = notice_cell().lock().unwrap_or_else(|e| e.into_inner());
        *guard = Some(format_notice(status));
    }
}

fn background_check() {
    background_check_with(&check_for_update());
}

fn auto_update_check_enabled() -> bool {
    let v = std::env::var(AUTO_UPDATE_CHECK_ENV).unwrap_or_else(|_| "true".to_string());
    !matches!(
        v.trim().to_lowercase().as_str(),
        "false" | "0" | "no" | "off"
    )
}

/// Start the update check in a background thread. Non-blocking — call once,
/// at real server startup, never from a per-request or per-connection path.
///
/// Honors `REMIND_ME_AUTO_UPDATE_CHECK=false` as an opt-out for the startup
/// `git fetch`; the manual check/update tools are unaffected.
pub fn start_background_check() {
    if !auto_update_check_enabled() {
        return;
    }
    std::thread::spawn(background_check);
}

/// Return and clear the cached update notice. Returns the notice exactly
/// once, then clears it so subsequent calls return `None`.
pub fn pop_update_notice() -> Option<String> {
    notice_cell()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    /// `UPDATE_EXPECTED_ORIGIN_ENV`/`AUTO_UPDATE_CHECK_ENV` are process-global,
    /// and the update-notice cache is a shared static -- tests run
    /// concurrently by default, so every test touching any of those holds
    /// this lock for the duration (the same convention `mempalace_import_test.rs`
    /// established for `REMIND_ME_MEMPALACE_PATH`).
    static ENV_LOCK: StdMutex<()> = StdMutex::new(());

    fn init_repo(dir: &Path) {
        Command::new("git")
            .args(["init", "--quiet", "-b", "main"])
            .current_dir(dir)
            .status()
            .unwrap();
        Command::new("git")
            .args(["config", "user.email", "test@example.com"])
            .current_dir(dir)
            .status()
            .unwrap();
        Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(dir)
            .status()
            .unwrap();
    }

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let unique = format!(
                "rmm_updater_{}_{}_{:?}",
                tag,
                std::process::id(),
                std::thread::current().id()
            );
            let path = std::env::temp_dir().join(unique.replace(['(', ')', ' '], ""));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn finds_the_repo_root_when_cargo_toml_names_this_workspace() {
        let tmp = TempDir::new("root");
        std::fs::create_dir_all(tmp.0.join(".git")).unwrap();
        std::fs::write(
            tmp.0.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/remind_me_core\"]\n",
        )
        .unwrap();
        let nested = tmp.0.join("crates").join("remind_me_core").join("src");
        std::fs::create_dir_all(&nested).unwrap();

        assert_eq!(find_repo_root_from(&nested), Some(tmp.0.clone()));
    }

    #[test]
    fn returns_none_with_no_git_directory_anywhere() {
        let tmp = TempDir::new("nogit");
        assert_eq!(find_repo_root_from(&tmp.0), None);
    }

    #[test]
    fn an_unrelated_nested_git_dir_does_not_stop_the_upward_walk() {
        // A vendored/nested repo's .git must not be mistaken for this
        // workspace's own -- self-update would otherwise operate on a
        // completely unrelated repository.
        let tmp = TempDir::new("nested");
        std::fs::create_dir_all(tmp.0.join(".git")).unwrap();
        std::fs::write(
            tmp.0.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/remind_me_core\"]\n",
        )
        .unwrap();
        let vendored = tmp.0.join("vendor").join("someone-elses-project");
        std::fs::create_dir_all(vendored.join(".git")).unwrap();
        std::fs::write(
            vendored.join("Cargo.toml"),
            "[package]\nname = \"unrelated\"\n",
        )
        .unwrap();
        let nested_start = vendored.join("src");
        std::fs::create_dir_all(&nested_start).unwrap();

        assert_eq!(find_repo_root_from(&nested_start), Some(tmp.0.clone()));
    }

    #[test]
    fn a_git_dir_whose_cargo_toml_does_not_match_is_not_this_workspace() {
        let tmp = TempDir::new("notours");
        std::fs::create_dir_all(tmp.0.join(".git")).unwrap();
        std::fs::write(
            tmp.0.join("Cargo.toml"),
            "[package]\nname = \"some-other-crate\"\n",
        )
        .unwrap();

        assert_eq!(find_repo_root_from(&tmp.0), None);
    }

    #[test]
    fn product_dir_recognises_the_monorepo_layout() {
        let tmp = TempDir::new("monorepo_layout");
        std::fs::write(
            tmp.0.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/apps/rusty_remind_me/crates/remind_me_core\"]\n",
        )
        .unwrap();

        assert_eq!(product_dir(&tmp.0), Some(MONOREPO_PRODUCT_DIR));
    }

    #[test]
    fn product_dir_recognises_the_standalone_layout() {
        let tmp = TempDir::new("standalone_layout");
        std::fs::write(
            tmp.0.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/remind_me_core\"]\n",
        )
        .unwrap();

        assert_eq!(product_dir(&tmp.0), Some("."));
    }

    #[test]
    fn product_dir_is_none_for_an_unrelated_manifest_or_none_at_all() {
        let tmp = TempDir::new("no_layout");
        assert_eq!(product_dir(&tmp.0), None);

        std::fs::write(tmp.0.join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        assert_eq!(product_dir(&tmp.0), None);
    }

    #[test]
    fn get_origin_url_reads_the_configured_remote() {
        let tmp = TempDir::new("originurl");
        init_repo(&tmp.0);
        run_git(
            &tmp.0,
            &[
                "remote",
                "add",
                "origin",
                "https://example.com/some/repo.git",
            ],
        )
        .unwrap();

        assert_eq!(get_origin_url(&tmp.0), "https://example.com/some/repo.git");
    }

    #[test]
    fn get_origin_url_is_empty_with_no_remote_configured() {
        let tmp = TempDir::new("noorigin");
        init_repo(&tmp.0);

        assert_eq!(get_origin_url(&tmp.0), "");
    }

    fn commit(dir: &Path, file: &str, contents: &str, message: &str) {
        std::fs::write(dir.join(file), contents).unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(dir)
            .status()
            .unwrap();
        Command::new("git")
            .args(["commit", "--quiet", "-m", message])
            .current_dir(dir)
            .status()
            .unwrap();
    }

    /// A local "origin" (a second real repo, cloned via a filesystem path --
    /// git treats that exactly like any other remote, no network needed)
    /// plus a clone of it, so `check_for_update_at`/`perform_update_at` run
    /// against a real git plumbing without ever touching the network.
    struct OriginAndClone {
        origin: TempDir,
        clone: TempDir,
    }

    fn origin_and_clone(tag: &str) -> OriginAndClone {
        let origin = TempDir::new(&format!("{tag}_origin"));
        init_repo(&origin.0);
        commit(&origin.0, "README.md", "hello", "initial commit");

        let clone = TempDir::new(&format!("{tag}_clone"));
        std::fs::remove_dir(&clone.0).unwrap();
        Command::new("git")
            .args([
                "clone",
                "--quiet",
                origin.0.to_str().unwrap(),
                clone.0.to_str().unwrap(),
            ])
            .status()
            .unwrap();
        Command::new("git")
            .args(["config", "user.email", "test@example.com"])
            .current_dir(&clone.0)
            .status()
            .unwrap();
        Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(&clone.0)
            .status()
            .unwrap();

        OriginAndClone { origin, clone }
    }

    #[test]
    fn up_to_date_reports_no_update_available() {
        let setup = origin_and_clone("uptodate");

        let status = check_for_update_at(&setup.clone.0);

        assert!(!status.update_available);
        assert_eq!(status.commits_behind, 0);
        assert!(status.error.is_none());
        assert_eq!(status.local_commit, status.remote_commit);
    }

    #[test]
    fn behind_reports_the_commit_count_and_messages() {
        let setup = origin_and_clone("behind");
        commit(&setup.origin.0, "a.txt", "a", "add a");
        commit(&setup.origin.0, "b.txt", "b", "add b");

        let status = check_for_update_at(&setup.clone.0);

        assert!(status.update_available);
        assert_eq!(status.commits_behind, 2);
        assert_eq!(status.commit_messages.len(), 2);
        assert!(
            status.commit_messages[0].contains("add b"),
            "newest first: {:?}",
            status.commit_messages
        );
    }

    #[test]
    fn in_the_monorepo_only_commits_touching_this_product_count() {
        let setup = origin_and_clone("monorepo_scope");
        commit(
            &setup.origin.0,
            "Cargo.toml",
            "[workspace]\nmembers = [\"crates/apps/rusty_remind_me/crates/remind_me_core\"]\n",
            "add the monorepo manifest",
        );
        run_git(
            &setup.clone.0,
            &["pull", "--quiet", "--ff-only", "origin", "main"],
        )
        .unwrap();

        std::fs::create_dir_all(setup.origin.0.join(MONOREPO_PRODUCT_DIR)).unwrap();
        commit(
            &setup.origin.0,
            &format!("{MONOREPO_PRODUCT_DIR}/NOTES.md"),
            "ours",
            "touch rusty_remind_me",
        );
        commit(
            &setup.origin.0,
            "other.txt",
            "theirs",
            "touch another crate",
        );

        let status = check_for_update_at(&setup.clone.0);

        assert!(status.update_available);
        assert_eq!(status.commits_behind, 1, "{:?}", status.commit_messages);
        assert!(status.commit_messages[0].contains("touch rusty_remind_me"));
    }

    #[test]
    fn in_the_monorepo_unrelated_commits_alone_are_not_an_update() {
        let setup = origin_and_clone("monorepo_unrelated");
        commit(
            &setup.origin.0,
            "Cargo.toml",
            "[workspace]\nmembers = [\"crates/apps/rusty_remind_me/crates/remind_me_core\"]\n",
            "add the monorepo manifest",
        );
        run_git(
            &setup.clone.0,
            &["pull", "--quiet", "--ff-only", "origin", "main"],
        )
        .unwrap();
        commit(
            &setup.origin.0,
            "other.txt",
            "theirs",
            "touch another crate",
        );

        let status = check_for_update_at(&setup.clone.0);

        assert!(status.error.is_none(), "{:?}", status.error);
        assert!(!status.update_available);
        assert_eq!(status.commits_behind, 0);
    }

    #[test]
    fn an_unreachable_origin_is_a_clear_error_not_a_panic() {
        let tmp = TempDir::new("badorigin");
        init_repo(&tmp.0);
        commit(&tmp.0, "README.md", "hello", "initial commit");
        run_git(&tmp.0, &["remote", "add", "origin", "/no/such/path/at/all"]).unwrap();

        let status = check_for_update_at(&tmp.0);

        assert!(status.error.is_some());
        assert!(!status.update_available);
    }

    #[test]
    fn check_for_update_reports_a_clear_error_outside_any_git_repo() {
        let tmp = TempDir::new("outside");
        // No .git anywhere -- find_repo_root_from must return None, and
        // check_for_update_at is never reached.
        assert_eq!(find_repo_root_from(&tmp.0), None);
    }

    #[test]
    fn a_dirty_tree_is_refused_without_force() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let setup = origin_and_clone("dirty");
        std::fs::write(setup.clone.0.join("uncommitted.txt"), "oops").unwrap();

        let result = perform_update_at(&setup.clone.0, false);

        assert!(!result.success);
        assert!(result.error.unwrap().contains("uncommitted changes"));
    }

    #[test]
    fn force_bypasses_the_dirty_tree_guard_but_not_ff_only() {
        // "force" must only skip the dirty-tree check -- a diverged local
        // history (a local commit not on origin) still refuses to pull,
        // force or not (docs/adr/0003).
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let setup = origin_and_clone("forcediverge");
        commit(&setup.origin.0, "upstream.txt", "new", "upstream change");
        commit(
            &setup.clone.0,
            "local.txt",
            "local",
            "a local commit not on origin",
        );
        std::fs::write(setup.clone.0.join("uncommitted.txt"), "oops").unwrap();

        let result = perform_update_at(&setup.clone.0, true);

        assert!(
            !result.success,
            "diverged history must still refuse, even with force"
        );
        assert!(result.error.unwrap().contains("git pull failed"));
    }

    #[test]
    fn an_expected_origin_mismatch_refuses_before_touching_anything() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let setup = origin_and_clone("originpin");
        std::env::set_var(
            UPDATE_EXPECTED_ORIGIN_ENV,
            "https://example.com/expected.git",
        );

        let result = perform_update_at(&setup.clone.0, false);

        std::env::remove_var(UPDATE_EXPECTED_ORIGIN_ENV);
        assert!(!result.success);
        assert!(result.error.unwrap().contains(UPDATE_EXPECTED_ORIGIN_ENV));
    }

    /// A workspace whose members carry exactly the names in
    /// [`BUILD_PACKAGES`], so `run_cargo_build`'s `-p` selection resolves
    /// the same way it does against the real product.
    fn write_minimal_binary_crate(dir: &Path) {
        let members: Vec<String> = BUILD_PACKAGES.iter().map(|p| format!("\"{p}\"")).collect();
        std::fs::write(
            dir.join("Cargo.toml"),
            format!(
                "[workspace]\nresolver = \"2\"\nmembers = [{}]\n",
                members.join(", ")
            ),
        )
        .unwrap();
        for package in BUILD_PACKAGES {
            let src = dir.join(package).join("src");
            std::fs::create_dir_all(&src).unwrap();
            std::fs::write(
                dir.join(package).join("Cargo.toml"),
                format!(
                    "[package]\nname = \"{package}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"
                ),
            )
            .unwrap();
            std::fs::write(src.join("main.rs"), "fn main() {}\n").unwrap();
        }
    }

    #[test]
    fn a_successful_pull_and_build_reports_success_and_the_new_commit() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let setup = origin_and_clone("buildok");
        write_minimal_binary_crate(&setup.origin.0);
        commit(
            &setup.origin.0,
            &format!("{}/src/main.rs", BUILD_PACKAGES[0]),
            "fn main() {}\n",
            "add a buildable workspace",
        );

        let result = perform_update_at(&setup.clone.0, false);

        assert!(result.success, "{:?}", result.error);
        assert!(result.restart_required);
        assert!(!result.rolled_back);
        assert_ne!(result.previous_commit, result.new_commit);
    }

    #[test]
    fn a_build_failure_is_rolled_back_and_reported() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let setup = origin_and_clone("buildfail");
        // A Cargo.toml with no corresponding source -- `cargo build` fails
        // immediately, which is exactly the rollback path this asserts.
        std::fs::write(
            setup.origin.0.join("Cargo.toml"),
            "[package]\nname = \"scratch\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        commit(
            &setup.origin.0,
            "Cargo.toml",
            "broken",
            "add a Cargo.toml with no source",
        );

        let result = perform_update_at(&setup.clone.0, false);

        assert!(!result.success);
        assert!(result.rolled_back, "{:?}", result.error);
        let head = run_git(&setup.clone.0, &["rev-parse", "--short", "HEAD"]).unwrap();
        assert_eq!(
            head, result.previous_commit,
            "the rollback actually moved HEAD back"
        );
    }

    #[test]
    fn pop_update_notice_returns_the_notice_exactly_once() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let status = UpdateStatus {
            installed_version: "0.1.0".to_string(),
            local_commit: "abc123".to_string(),
            remote_commit: "def456".to_string(),
            update_available: true,
            commits_behind: 3,
            commit_messages: vec!["fix bug".to_string()],
            ..Default::default()
        };

        background_check_with(&status);

        assert!(pop_update_notice().unwrap().contains("3 commits behind"));
        assert!(
            pop_update_notice().is_none(),
            "the notice fires once, then clears"
        );
    }

    #[test]
    fn background_check_sets_no_notice_when_already_up_to_date() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _ = pop_update_notice(); // drain anything a prior test left pending
        let status = UpdateStatus {
            update_available: false,
            ..Default::default()
        };

        background_check_with(&status);

        assert!(pop_update_notice().is_none());
    }

    #[test]
    fn auto_update_check_is_enabled_by_default() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var(AUTO_UPDATE_CHECK_ENV);
        assert!(auto_update_check_enabled());
    }

    #[test]
    fn auto_update_check_env_values_that_disable_it() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        for value in ["false", "0", "no", "off", "FALSE", "Off"] {
            std::env::set_var(AUTO_UPDATE_CHECK_ENV, value);
            assert!(
                !auto_update_check_enabled(),
                "{value:?} should disable the check"
            );
        }
        std::env::remove_var(AUTO_UPDATE_CHECK_ENV);
    }
}
