//! What a session changed in its repository, read from git rather than from
//! the transcript.
//!
//! A model's account of its own work drifts; `git diff --numstat` does not.
//! `session end` calls [`build`] for the span from the session's `start_sha`
//! to now, and stores [`render_markdown`] as a `work_log` memory.
//!
//! Everything shells out to the `git` binary on `PATH`: no git library, and a
//! missing binary or a directory that is not a repository reads as "no
//! change", never as an error that would fail a session's end.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

/// File lines rendered before the rest are summarised as "+N more".
pub const MAX_FILE_LINES: usize = 40;

/// One path's added and deleted line counts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileChange {
    pub path: String,
    pub added: u64,
    pub deleted: u64,
}

/// The repository's change since a session began.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkLog {
    pub files_changed: usize,
    pub insertions: u64,
    pub deletions: u64,
    /// Largest change first, then by path.
    pub files: Vec<FileChange>,
    /// Files git does not track yet; their size is unknown, so uncounted.
    pub untracked: usize,
    pub head: Option<String>,
    pub branch: Option<String>,
}

/// Run `git -C cwd args`, returning trimmed stdout, or `None` when git is
/// missing, `cwd` is not a repository, or the command failed.
pub fn git(cwd: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["-c", "core.quotepath=off"])
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Add each `numstat` line (`added<TAB>deleted<TAB>path`) into `into`. A
/// binary file (`-` counts) is a changed file of 0/0.
fn add_numstat(into: &mut BTreeMap<String, (u64, u64)>, numstat: &str) {
    for line in numstat.lines() {
        let mut parts = line.splitn(3, '\t');
        let (Some(added), Some(deleted), Some(path)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let entry = into.entry(path.to_string()).or_default();
        entry.0 += added.parse().unwrap_or(0);
        entry.1 += deleted.parse().unwrap_or(0);
    }
}

/// Paths git lists as untracked in `git status --porcelain`.
fn count_untracked(porcelain: &str) -> usize {
    porcelain.lines().filter(|l| l.starts_with("??")).count()
}

/// The repository's change since `start_sha` (committed since, plus staged
/// and unstaged now), or `None` when there is none to report: not a
/// repository, git missing, or nothing changed.
///
/// With no `start_sha` only the working tree is read.
pub fn build(cwd: &Path, start_sha: Option<&str>) -> Option<WorkLog> {
    let head = git(cwd, &["rev-parse", "HEAD"]);
    let branch = git(cwd, &["rev-parse", "--abbrev-ref", "HEAD"]);
    if head.is_none() && branch.is_none() {
        return None;
    }

    let mut totals = BTreeMap::new();
    let committed_since = start_sha.filter(|sha| head.as_deref() != Some(*sha));
    if let Some(sha) = committed_since {
        let range = format!("{sha}..HEAD");
        add_numstat(&mut totals, &git(cwd, &["diff", "--numstat", &range])?);
    }
    // Staged and unstaged together, against HEAD. A path edited again after
    // a commit counts in both spans, which is the work done on it.
    if let Some(tree) = git(cwd, &["diff", "--numstat", "HEAD"]) {
        add_numstat(&mut totals, &tree);
    }
    let untracked = count_untracked(&git(cwd, &["status", "--porcelain"]).unwrap_or_default());
    if totals.is_empty() && untracked == 0 {
        return None;
    }

    let mut files: Vec<FileChange> = totals
        .into_iter()
        .map(|(path, (added, deleted))| FileChange {
            path,
            added,
            deleted,
        })
        .collect();
    files.sort_by(|a, b| {
        (b.added + b.deleted)
            .cmp(&(a.added + a.deleted))
            .then(a.path.cmp(&b.path))
    });
    Some(WorkLog {
        files_changed: files.len(),
        insertions: files.iter().map(|f| f.added).sum(),
        deletions: files.iter().map(|f| f.deleted).sum(),
        files,
        untracked,
        head,
        branch,
    })
}

/// A compact Markdown body: a summary line, then at most
/// [`MAX_FILE_LINES`] file lines and "+N more".
pub fn render_markdown(log: &WorkLog) -> String {
    let short = |sha: &Option<String>| {
        sha.as_deref()
            .map(|s| s.chars().take(7).collect::<String>())
    };
    let at = match (&log.branch, short(&log.head)) {
        (Some(b), Some(h)) => format!("{b} @ {h}"),
        (Some(b), None) => b.clone(),
        (None, Some(h)) => h,
        (None, None) => "unknown".to_string(),
    };
    let mut out = format!(
        "Work log ({at}): {} files changed, +{} -{}, {} untracked\n",
        log.files_changed, log.insertions, log.deletions, log.untracked
    );
    for f in log.files.iter().take(MAX_FILE_LINES) {
        out.push_str(&format!("\n- {} (+{} -{})", f.path, f.added, f.deleted));
    }
    if log.files.len() > MAX_FILE_LINES {
        out.push_str(&format!("\n+{} more", log.files.len() - MAX_FILE_LINES));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A fresh repository with one commit, or `None` when git is missing.
    fn repo() -> Option<PathBuf> {
        let dir =
            std::env::temp_dir().join(format!("rrm_worklog_{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).ok()?;
        git(&dir, &["init", "-q"])?;
        std::fs::write(dir.join("a.txt"), "one\ntwo\n").ok()?;
        commit_all(&dir, "first")?;
        Some(dir)
    }

    fn commit_all(dir: &Path, msg: &str) -> Option<()> {
        git(dir, &["add", "-A"])?;
        git(
            dir,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-q",
                "-m",
                msg,
            ],
        )?;
        Some(())
    }

    #[test]
    fn a_commit_and_an_unstaged_edit_are_counted() {
        let Some(dir) = repo() else { return };
        let start = git(&dir, &["rev-parse", "HEAD"]).unwrap();
        std::fs::write(dir.join("b.txt"), "x\ny\nz\n").unwrap();
        commit_all(&dir, "second").unwrap();
        std::fs::write(dir.join("a.txt"), "one\ntwo\nthree\n").unwrap();
        std::fs::write(dir.join("new.txt"), "untracked").unwrap();

        let log = build(&dir, Some(&start)).unwrap();
        assert_eq!(log.files_changed, 2);
        assert_eq!(
            log.insertions, 4,
            "3 committed in b.txt + 1 unstaged in a.txt"
        );
        assert_eq!(log.deletions, 0);
        assert_eq!(log.untracked, 1);
        assert_eq!(log.files[0].path, "b.txt", "largest change first");
        assert!(render_markdown(&log).contains("- b.txt (+3 -0)"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_change_and_no_repository_are_none() {
        let Some(dir) = repo() else { return };
        let head = git(&dir, &["rev-parse", "HEAD"]).unwrap();
        assert_eq!(build(&dir, Some(&head)), None);
        assert_eq!(build(&dir, None), None);
        let _ = std::fs::remove_dir_all(&dir);

        let bare =
            std::env::temp_dir().join(format!("rrm_notrepo_{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&bare).unwrap();
        assert_eq!(build(&bare, None), None);
        let _ = std::fs::remove_dir_all(&bare);
    }

    #[test]
    fn rendering_caps_the_file_list() {
        let files = (0..45)
            .map(|i| FileChange {
                path: format!("f{i}"),
                added: 1,
                deleted: 0,
            })
            .collect();
        let log = WorkLog {
            files_changed: 45,
            insertions: 45,
            deletions: 0,
            files,
            untracked: 0,
            head: Some("abcdef0123".into()),
            branch: Some("main".into()),
        };
        let text = render_markdown(&log);
        assert_eq!(
            text.lines().filter(|l| l.starts_with("- ")).count(),
            MAX_FILE_LINES
        );
        assert!(text.ends_with("+5 more"));
        assert!(text.contains("main @ abcdef0"));
    }

    #[test]
    fn numstat_parsing_sums_and_tolerates_binary() {
        let mut m = BTreeMap::new();
        add_numstat(&mut m, "1\t2\tx\n-\t-\timg.png\ngarbage\n3\t0\tx");
        assert_eq!(m["x"], (4, 2));
        assert_eq!(m["img.png"], (0, 0));
        assert_eq!(m.len(), 2);
    }
}
